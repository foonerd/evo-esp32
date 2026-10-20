// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! What the list pages show: the library's folder in view, and the
//! settings' facts. The distribution fills them from the source's
//! listing and from what it knows about itself; the face draws rows.

use alloc::string::String;
use alloc::vec::Vec;

/// One row of the library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// What the row says.
    pub name: String,
    /// A folder to open, else a track to play.
    pub folder: bool,
    /// The track's URI, to tell the one in hand; folders have none.
    pub uri: Option<String>,
    /// False for a track this player cannot play (its rate is above
    /// the board's ceiling): shown greyed, a tap does nothing.
    pub playable: bool,
}

/// A folder of the source, as listed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Library {
    /// The folder's path, as the source names it; empty at the root.
    pub path: String,
    /// What the header says: the folder's name, or the source's.
    pub name: String,
    /// The rows, folders first, in the source's order.
    pub entries: Vec<Entry>,
    /// The first row in view.
    pub first: usize,
}

impl Library {
    /// Scroll by `rows` (negative is up), keeping at least one row in
    /// view; `true` if the view moved.
    pub fn scroll(&mut self, rows: i32, shown: usize) -> bool {
        scroll(&mut self.first, self.entries.len(), rows, shown)
    }

    /// The rows in view, at most `shown`.
    pub fn visible(&self, shown: usize) -> &[Entry] {
        visible(&self.entries, self.first, shown)
    }
}

/// Move `first` by `rows` within a list of `len` showing `shown` rows,
/// so the last page is full; `true` if it moved.
fn scroll(first: &mut usize, len: usize, rows: i32, shown: usize) -> bool {
    let last_first = len.saturating_sub(shown);
    let next =
        (*first as i64 + i64::from(rows)).clamp(0, last_first as i64) as usize;
    let moved = next != *first;
    *first = next;
    moved
}

/// At most `shown` of `rows` from `first`.
fn visible<T>(rows: &[T], first: usize, shown: usize) -> &[T] {
    let end = (first + shown).min(rows.len());
    &rows[first.min(end)..end]
}

/// A fact about the player, for the settings page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fact {
    /// What it is.
    pub name: String,
    /// What it says.
    pub value: String,
}

/// What the settings page shows: facts, some of them settings.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct About {
    /// The facts, in order.
    pub facts: Vec<Fact>,
    /// The first row in view.
    pub first: usize,
}

impl About {
    /// Scroll by `rows` (negative is up), as [`Library::scroll`].
    pub fn scroll(&mut self, rows: i32, shown: usize) -> bool {
        scroll(&mut self.first, self.facts.len(), rows, shown)
    }

    /// The facts in view, at most `shown`.
    pub fn visible(&self, shown: usize) -> &[Fact] {
        visible(&self.facts, self.first, shown)
    }

    /// The fact behind the `row`th row in view, if there is one.
    pub fn fact_at(&self, row: usize, shown: usize) -> Option<&Fact> {
        self.visible(shown).get(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    fn library(n: usize) -> Library {
        Library {
            path: String::new(),
            name: "Card".to_string(),
            entries: (0..n)
                .map(|i| Entry {
                    name: alloc::format!("{i}"),
                    folder: false,
                    uri: None,
                    playable: true,
                })
                .collect(),
            first: 0,
        }
    }

    #[test]
    fn scrolling_stays_within_the_list() {
        let mut l = library(10);
        assert!(!l.scroll(-1, 7));
        assert!(l.scroll(2, 7));
        assert_eq!(l.first, 2);
        assert!(l.scroll(10, 7), "clamps to the last page");
        assert_eq!(l.first, 3);
        assert_eq!(l.visible(7).len(), 7);
        assert_eq!(l.visible(7)[0].name, "3");
        let mut short = library(3);
        assert!(!short.scroll(1, 7), "nothing to scroll");
        assert_eq!(short.visible(7).len(), 3);
    }

    #[test]
    fn the_facts_scroll_the_same_way() {
        let mut about = About {
            facts: (0..7)
                .map(|i| Fact {
                    name: alloc::format!("{i}"),
                    value: String::new(),
                })
                .collect(),
            first: 0,
        };
        assert!(!about.scroll(1, 7), "seven facts fill seven rows");
        assert!(about.scroll(5, 5), "two are off a five-row page");
        assert_eq!(about.first, 2);
        assert_eq!(about.fact_at(4, 5).map(|f| f.name.as_str()), Some("6"));
        assert_eq!(about.fact_at(5, 5), None);
    }
}

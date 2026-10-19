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
        let last_first = self.entries.len().saturating_sub(shown);
        let first = (self.first as i64 + i64::from(rows))
            .clamp(0, last_first as i64) as usize;
        let moved = first != self.first;
        self.first = first;
        moved
    }

    /// The rows in view, at most `shown`.
    pub fn visible(&self, shown: usize) -> &[Entry] {
        let end = (self.first + shown).min(self.entries.len());
        &self.entries[self.first.min(end)..end]
    }
}

/// A fact about the player, for the settings page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fact {
    /// What it is.
    pub name: String,
    /// What it says.
    pub value: String,
}

/// What the settings page shows: facts, until there are settings.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct About {
    /// The facts, in order.
    pub facts: Vec<Fact>,
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
}

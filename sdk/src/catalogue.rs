// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! The catalogue: the device, declared as data.
//!
//! A distribution writes a `catalogue.toml`; the firmware's build resolves
//! it against the plugin crates it links and refuses to build a device
//! whose shelves nothing stocks. On the device the catalogue is a static
//! table the steward reads at start. The types here are that table.
//!
//! Racks belong to a family (what the concern is for) and have kinds (what
//! the concern does). Each shelf has a shape, a version number a plugin's
//! stocking must equal: the steward enforces equality, as evo-core does.

use alloc::string::String;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

/// What a rack is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Family {
    /// What the distribution does: sources, playback, presentation.
    Domain,
    /// When and why it acts: appointments, watches.
    Coordination,
    /// How the fabric operates over time: network, power, storage.
    Infrastructure,
}

/// What a rack does. A rack may have more than one kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Originates instructions.
    Producer,
    /// Moves or changes something.
    Transformer,
    /// Renders projections to a surface.
    Presenter,
    /// Holds knowledge.
    Registrar,
}

/// A typed opening in a rack that plugins stock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shelf {
    /// Name, unique within the rack.
    pub name: String,
    /// Shape version. A plugin's stocking must declare the same number.
    pub shape: u32,
    /// What the shelf holds, for a reader.
    #[serde(default)]
    pub description: String,
}

/// Names one shelf: the rack and the shelf within it. What a request or
/// an assignment is addressed to.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ShelfRef {
    /// The rack.
    pub rack: String,
    /// The shelf within it.
    pub shelf: String,
}

impl ShelfRef {
    /// A reference from rack and shelf names.
    pub fn new(rack: impl Into<String>, shelf: impl Into<String>) -> Self {
        Self {
            rack: rack.into(),
            shelf: shelf.into(),
        }
    }
}

impl core::fmt::Display for ShelfRef {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}/{}", self.rack, self.shelf)
    }
}

/// A concern the device has.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rack {
    /// Name, unique within the catalogue.
    pub name: String,
    /// What it is for.
    pub family: Family,
    /// What it does.
    pub kinds: Vec<Kind>,
    /// Its charter: one sentence a reader can hold the rack to.
    #[serde(default)]
    pub charter: String,
    /// The shelves plugins stock.
    #[serde(default)]
    pub shelves: Vec<Shelf>,
}

/// The device, declared.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Catalogue {
    /// Every rack. Order is the order of the declaration.
    #[serde(default)]
    pub racks: Vec<Rack>,
}

/// Why a catalogue is not well formed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogueError {
    /// A rack or shelf has an empty name.
    EmptyName,
    /// Two racks share a name.
    DuplicateRack(String),
    /// Two shelves in one rack share a name.
    DuplicateShelf {
        /// The rack.
        rack: String,
        /// The shelf.
        shelf: String,
    },
    /// A rack declares no kind.
    NoKind(String),
}

impl core::fmt::Display for CatalogueError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::EmptyName => f.write_str("a rack or shelf has an empty name"),
            Self::DuplicateRack(r) => write!(f, "rack `{r}` is declared twice"),
            Self::DuplicateShelf { rack, shelf } => {
                write!(f, "shelf `{shelf}` is declared twice in rack `{rack}`")
            }
            Self::NoKind(r) => write!(f, "rack `{r}` declares no kind"),
        }
    }
}

impl Catalogue {
    /// Check the declaration is well formed: names present and unique,
    /// every rack with a kind.
    pub fn validate(&self) -> Result<(), CatalogueError> {
        let mut seen_racks: Vec<&str> = Vec::new();
        for rack in &self.racks {
            if rack.name.is_empty() {
                return Err(CatalogueError::EmptyName);
            }
            if seen_racks.contains(&rack.name.as_str()) {
                return Err(CatalogueError::DuplicateRack(rack.name.clone()));
            }
            seen_racks.push(&rack.name);
            if rack.kinds.is_empty() {
                return Err(CatalogueError::NoKind(rack.name.clone()));
            }
            let mut seen_shelves: Vec<&str> = Vec::new();
            for shelf in &rack.shelves {
                if shelf.name.is_empty() {
                    return Err(CatalogueError::EmptyName);
                }
                if seen_shelves.contains(&shelf.name.as_str()) {
                    return Err(CatalogueError::DuplicateShelf {
                        rack: rack.name.clone(),
                        shelf: shelf.name.clone(),
                    });
                }
                seen_shelves.push(&shelf.name);
            }
        }
        Ok(())
    }

    /// The rack called `name`.
    pub fn rack(&self, name: &str) -> Option<&Rack> {
        self.racks.iter().find(|r| r.name == name)
    }

    /// The shelf `shelf` in rack `rack`.
    pub fn shelf(&self, rack: &str, shelf: &str) -> Option<&Shelf> {
        self.rack(rack)?.shelves.iter().find(|s| s.name == shelf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn shelf(name: &str, shape: u32) -> Shelf {
        Shelf {
            name: name.into(),
            shape,
            description: String::new(),
        }
    }

    fn rack(name: &str, shelves: Vec<Shelf>) -> Rack {
        Rack {
            name: name.into(),
            family: Family::Domain,
            kinds: vec![Kind::Registrar],
            charter: String::new(),
            shelves,
        }
    }

    #[test]
    fn a_well_formed_catalogue_validates() {
        let c = Catalogue {
            racks: vec![rack(
                "audio",
                vec![shelf("source", 1), shelf("delivery", 1)],
            )],
        };
        assert_eq!(c.validate(), Ok(()));
        assert_eq!(c.shelf("audio", "delivery").map(|s| s.shape), Some(1));
        assert!(c.shelf("audio", "nothing").is_none());
    }

    #[test]
    fn duplicates_and_blanks_are_refused() {
        let dup_rack = Catalogue {
            racks: vec![rack("a", vec![]), rack("a", vec![])],
        };
        assert_eq!(
            dup_rack.validate(),
            Err(CatalogueError::DuplicateRack("a".into()))
        );
        let dup_shelf = Catalogue {
            racks: vec![rack("a", vec![shelf("s", 1), shelf("s", 2)])],
        };
        assert_eq!(
            dup_shelf.validate(),
            Err(CatalogueError::DuplicateShelf {
                rack: "a".into(),
                shelf: "s".into()
            })
        );
        let blank = Catalogue {
            racks: vec![rack("", vec![])],
        };
        assert_eq!(blank.validate(), Err(CatalogueError::EmptyName));
        let mut no_kind = rack("k", vec![]);
        no_kind.kinds.clear();
        let no_kind = Catalogue {
            racks: vec![no_kind],
        };
        assert_eq!(no_kind.validate(), Err(CatalogueError::NoKind("k".into())));
    }

    #[test]
    fn round_trips_through_serde_as_evo_core_spells_it() {
        let json = r#"{"racks":[{"name":"example","family":"domain","kinds":["registrar"],"charter":"Minimal example rack.","shelves":[{"name":"echo","shape":1,"description":"Echoes inputs back."}]}]}"#;
        let c: Catalogue = serde_json::from_str(json).unwrap();
        assert_eq!(c.racks[0].family, Family::Domain);
        assert_eq!(c.racks[0].kinds, vec![Kind::Registrar]);
        assert_eq!(serde_json::to_string(&c).unwrap(), json);
    }
}

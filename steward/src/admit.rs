// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Admission: may this plugin stock what it says it stocks?
//!
//! The check evo-core makes at runtime against signed manifests, made here
//! against the catalogue and the plugin's own description: every stocking
//! names a rack and shelf the catalogue declares, with a shape equal to
//! the shelf's, and no two plugins share a name.

use alloc::string::String;

use evo_esp32_sdk::catalogue::Catalogue;
use evo_esp32_sdk::plugin::PluginDescription;

/// Why a plugin was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmissionError {
    /// A plugin of this name is already admitted.
    DuplicateName(String),
    /// The plugin stocks nothing.
    NoStocking(String),
    /// A stocking names a rack the catalogue does not declare.
    UnknownRack {
        /// The plugin.
        plugin: String,
        /// The rack.
        rack: String,
    },
    /// A stocking names a shelf the rack does not declare.
    UnknownShelf {
        /// The plugin.
        plugin: String,
        /// The rack.
        rack: String,
        /// The shelf.
        shelf: String,
    },
    /// A stocking's shape differs from the shelf's.
    ShapeMismatch {
        /// The plugin.
        plugin: String,
        /// The shelf.
        shelf: String,
        /// What the catalogue declares.
        expected: u32,
        /// What the plugin satisfies.
        found: u32,
    },
    /// The steward holds as many plugins as it was built for.
    Full,
    /// The plugin passed the checks but its `load` failed. Anything it
    /// announced while loading has been retracted.
    LoadFailed {
        /// The plugin.
        plugin: String,
        /// Its own words.
        detail: String,
    },
}

impl core::fmt::Display for AdmissionError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::DuplicateName(p) => write!(f, "plugin `{p}` is already admitted"),
            Self::NoStocking(p) => write!(f, "plugin `{p}` stocks nothing"),
            Self::UnknownRack { plugin, rack } => {
                write!(f, "plugin `{plugin}` stocks unknown rack `{rack}`")
            }
            Self::UnknownShelf { plugin, rack, shelf } => {
                write!(f, "plugin `{plugin}` stocks unknown shelf `{rack}/{shelf}`")
            }
            Self::ShapeMismatch {
                plugin,
                shelf,
                expected,
                found,
            } => write!(
                f,
                "plugin `{plugin}` satisfies shape {found} on `{shelf}`, which is shape {expected}"
            ),
            Self::Full => f.write_str("no room for another plugin"),
            Self::LoadFailed { plugin, detail } => {
                write!(f, "plugin `{plugin}` failed to load: {detail}")
            }
        }
    }
}

/// A plugin that passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admitted {
    /// Its description, as it gave it.
    pub description: PluginDescription,
    /// Its admission index: its position among the admitted, which is
    /// also its precedence in composition.
    pub index: usize,
}

/// Check `desc` against `catalogue` and the names already admitted.
pub fn check(
    catalogue: &Catalogue,
    admitted: &[&'static str],
    desc: &PluginDescription,
) -> Result<(), AdmissionError> {
    let plugin = String::from(desc.name);
    if admitted.contains(&desc.name) {
        return Err(AdmissionError::DuplicateName(plugin));
    }
    if desc.stocks.is_empty() {
        return Err(AdmissionError::NoStocking(plugin));
    }
    for s in desc.stocks {
        let rack = catalogue.rack(s.rack).ok_or_else(|| {
            AdmissionError::UnknownRack {
                plugin: plugin.clone(),
                rack: String::from(s.rack),
            }
        })?;
        let shelf = rack
            .shelves
            .iter()
            .find(|sh| sh.name == s.shelf)
            .ok_or_else(|| AdmissionError::UnknownShelf {
                plugin: plugin.clone(),
                rack: String::from(s.rack),
                shelf: String::from(s.shelf),
            })?;
        if shelf.shape != s.shape {
            return Err(AdmissionError::ShapeMismatch {
                plugin: plugin.clone(),
                shelf: alloc::format!("{}/{}", s.rack, s.shelf),
                expected: shelf.shape,
                found: s.shape,
            });
        }
    }
    Ok(())
}

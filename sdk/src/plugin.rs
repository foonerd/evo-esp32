// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! The plugin contract: what every plugin is, whatever shelf it stocks.
//!
//! A plugin describes itself (name, version, the shelves it stocks), is
//! loaded once with a [`Context`], reports its health on request, and is
//! unloaded at shutdown. It is a [`Respondent`](crate::respondent::Respondent)
//! or a [`Warden`](crate::warden::Warden) on top of that, by the shape of
//! the shelf.
//!
//! Names are reverse-DNS, as evo-core's: plugins the evo project ships
//! are `org.evoframework.*`; a vendor's are under its own namespace.

use alloc::string::String;
use serde::{Deserialize, Serialize};

use crate::context::Context;

/// How many contributions a plugin makes over its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstanceShape {
    /// One contribution, for as long as it is loaded.
    Singleton,
    /// Many contributions over time, driven by what it observes.
    Factory,
}

/// How a plugin interacts with the steward.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InteractionShape {
    /// Answers discrete requests.
    Respondent,
    /// Takes custody of sustained work.
    Warden,
}

/// One shelf a plugin stocks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Stocking {
    /// The rack.
    pub rack: &'static str,
    /// The shelf within it.
    pub shelf: &'static str,
    /// The shape the plugin satisfies. Must equal the shelf's.
    pub shape: u32,
}

/// What a plugin says about itself. Static data; it serialises into the
/// projection of loaded plugins and is never read back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PluginDescription {
    /// Reverse-DNS name, unique on the device.
    pub name: &'static str,
    /// The plugin's own version.
    pub version: &'static str,
    /// How many contributions it makes.
    pub instance: InstanceShape,
    /// How it interacts.
    pub interaction: InteractionShape,
    /// The shelves it stocks. At least one.
    pub stocks: &'static [Stocking],
}

/// Why a plugin could not load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadError(pub String);

impl core::fmt::Display for LoadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A plugin's own account of its health.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", tag = "status")]
pub enum Health {
    /// Doing its job.
    Healthy,
    /// Doing its job with a known limitation.
    Degraded {
        /// The limitation, in the plugin's words.
        detail: String,
    },
    /// Not doing its job.
    Unhealthy {
        /// Why, in the plugin's words.
        detail: String,
    },
}

/// Every plugin.
pub trait Plugin {
    /// Who it is and what it stocks. Constant for the plugin's life.
    fn describe(&self) -> PluginDescription;

    /// Called once after admission, before any request or tick.
    fn load(&mut self, ctx: &mut dyn Context) -> Result<(), LoadError>;

    /// Called once at shutdown. Nothing is called after it.
    fn unload(&mut self, ctx: &mut dyn Context);

    /// Called on a cadence the steward chooses.
    fn health(&self) -> Health {
        Health::Healthy
    }
}

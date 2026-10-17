// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! # evo-esp32-sdk
//!
//! The surface a plugin implements against on the evo-esp32 fabric: the
//! fabric vocabulary of evo-core carried to a microcontroller, where the
//! steward and every plugin are compiled into one image and talk through
//! trait calls.
//!
//! The vocabulary is evo's and is load-bearing:
//!
//! - A [`catalogue::Catalogue`] declares the device as racks, shelves and
//!   shapes. It is data, resolved when the firmware is built.
//! - A [`plugin::Plugin`] stocks a shelf. It never addresses another
//!   plugin; the only handle it holds is the [`context::Context`] the
//!   steward gives it.
//! - A [`respondent::Respondent`] answers discrete requests; a
//!   [`warden::Warden`] takes custody of sustained work and is ticked.
//! - Every contribution is keyed to a [`subject::SubjectId`]. The steward
//!   keeps the registry; plugins announce, update and retract.
//! - The steward emits [`happening::Happening`]s on a stream and serves
//!   [`projection::Projection`]s on demand. Consumers read nothing else.
//!
//! What has no equivalent on a microcontroller is dropped rather than
//! imitated: processes, sockets, runtime admission of signed bundles, hot
//! reload. Admission is the build; a new plugin is a new image.
//!
//! `no_std` with `alloc`, no `unsafe`, blocking throughout. Values that
//! cross the contract are [`Value`] (JSON), as they are on evo-core's
//! wire, so a projection serialises for the network without translation.
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod catalogue;
pub mod context;
pub mod happening;
pub mod plugin;
pub mod projection;
pub mod respondent;
pub mod subject;
pub mod warden;

/// A JSON value, the shape every contribution and projection takes.
pub use serde_json::Value;

/// Version of this SDK crate, from its manifest at build time.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Milliseconds since the steward started. The fabric's only clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Ticks(pub u64);

impl Ticks {
    /// Milliseconds elapsed from `earlier` to `self`, saturating at zero.
    pub const fn since(self, earlier: Ticks) -> u64 {
        self.0.saturating_sub(earlier.0)
    }
}

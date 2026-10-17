// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! # evo-esp32-steward
//!
//! The sole authority of an evo-esp32 device. It holds the catalogue,
//! admits the plugins that stock it, owns every subject, composes the
//! plugins' contributions into each subject's state, emits happenings on
//! a bounded stream and serves projections on demand. Plugins never see
//! it: they see the [`Context`] it hands them.
//!
//! Three things make the fabric's promises hold on a microcontroller:
//!
//! - **Plugins never coordinate.** Each is a crate that depends on the SDK
//!   and nothing else in the workspace; the compiler enforces it. Here,
//!   each is a boxed trait object the steward alone calls.
//! - **The steward is the sole authority.** Mutable state lives in
//!   [`Tables`] and nowhere else. A plugin receives a [`Context`] that
//!   borrows the tables for one call and returns.
//! - **Admission is the build.** [`Steward::admit_respondent`] and
//!   [`Steward::admit_warden`] check a plugin's
//!   stockings against the catalogue (rack present, shelf present, shape
//!   equal) and refuses otherwise. The firmware crate's dependency list is
//!   the admission list; a catalogue tool checks it before the build.
//!
//! The steward has no clock and no thread. The caller passes [`Ticks`] into
//! every call and drives [`Steward::tick`] from its loop. Composition is
//! per plugin: each plugin's contribution to a subject is kept apart, and
//! the subject's state is the merge of them in admission order, so one
//! plugin retracting leaves the others' opinions standing.
#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod admit;
mod compose;
mod context;
mod happenings;
mod steward;
mod tables;

pub use admit::{check as admission_check, AdmissionError, Admitted};
pub use context::Context;
pub use evo_esp32_sdk::Ticks;
pub use happenings::{HappeningsRing, Replay};
pub use steward::{Config, DispatchError, Steward};
pub use tables::Tables;

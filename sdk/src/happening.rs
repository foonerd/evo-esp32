// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Happenings: the transitions the steward emits.
//!
//! All outbound behaviour of the fabric is a projection on demand or a
//! happening on this stream. There is no side channel. Happenings are
//! numbered so a consumer that fell behind can say where it left off.

use alloc::string::String;
use serde::{Deserialize, Serialize};

use crate::subject::SubjectId;
use crate::Ticks;

/// Position on the happenings stream. Strictly increasing, never reused.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Default,
    Serialize,
    Deserialize,
)]
#[serde(transparent)]
pub struct Seq(pub u64);

/// What kind of transition took place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum HappeningKind {
    /// A subject came into being.
    Announced,
    /// A subject's state changed.
    Updated,
    /// A subject ceased to be.
    Retracted,
    /// A plugin was admitted and loaded.
    PluginLoaded {
        /// The plugin's name.
        plugin: String,
    },
    /// A plugin reported unhealthy.
    PluginUnhealthy {
        /// The plugin's name.
        plugin: String,
        /// Its own words.
        detail: String,
    },
    /// A warden took custody of an assignment.
    CustodyTaken {
        /// The warden's name.
        warden: String,
    },
    /// A warden released custody.
    CustodyReleased {
        /// The warden's name.
        warden: String,
        /// Why, when the work failed rather than finished or was let go.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

/// One transition on the stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Happening {
    /// Position on the stream.
    pub seq: Seq,
    /// When, on the fabric's clock.
    pub at: Ticks,
    /// The subject it concerns, where it concerns one.
    pub subject: Option<SubjectId>,
    /// What took place.
    pub kind: HappeningKind,
}

impl Serialize for Ticks {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(self.0)
    }
}

impl<'de> Deserialize<'de> for Ticks {
    fn deserialize<D: serde::Deserializer<'de>>(
        d: D,
    ) -> Result<Self, D::Error> {
        u64::deserialize(d).map(Ticks)
    }
}

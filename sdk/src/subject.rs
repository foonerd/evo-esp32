// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Subjects: the things the catalogue has opinions about.
//!
//! A subject is one real thing (a track, a source, the output, the
//! network) with one canonical identity the steward holds. Plugins do not
//! mint identities; they announce a thing under an external addressing
//! and a kind, and the steward answers with, or reconciles to, the
//! canonical id. Plugins then update and retract by that id.

use alloc::string::String;
use serde::{Deserialize, Serialize};

use crate::Value;

/// The canonical identity of one real thing, held by the steward.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct SubjectId(pub String);

impl SubjectId {
    /// An id from its text form.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The text form.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for SubjectId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// How a plugin addresses a thing in its own terms, before reconciliation.
///
/// Two plugins may address the same thing differently (a track by its path
/// and by its tags' hash); the steward reconciles them to one
/// [`SubjectId`]. The scheme names the addressing space; the key is the
/// plugin's handle within it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ExternalAddressing {
    /// The addressing space: `path`, `url`, `mac`, `uuid`, a plugin's own.
    pub scheme: String,
    /// The handle within that space.
    pub key: String,
}

impl ExternalAddressing {
    /// An addressing from scheme and key.
    pub fn new(scheme: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            scheme: scheme.into(),
            key: key.into(),
        }
    }
}

/// A plugin's announcement that a thing exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubjectAnnouncement {
    /// How the announcing plugin addresses it.
    pub addressing: ExternalAddressing,
    /// What kind of thing it is, in the catalogue's vocabulary: `track`,
    /// `source`, `output`, `network`.
    pub kind: String,
    /// Its state as the plugin knows it.
    pub state: Value,
}

/// A subject as the steward holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubjectRecord {
    /// The canonical id.
    pub id: SubjectId,
    /// Its kind.
    pub kind: String,
    /// Its current composed state.
    pub state: Value,
    /// Which plugin last wrote to it, by name.
    pub last_writer: String,
}

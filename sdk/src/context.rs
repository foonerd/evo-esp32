// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! The handle a plugin holds on the steward.
//!
//! A plugin never holds the steward itself. At load, and on every call
//! after, it receives a [`Context`]: the few things it may ask the fabric
//! to do. Everything a plugin contributes goes through here, so the
//! steward is the sole authority by construction, not by convention.

use crate::subject::{SubjectAnnouncement, SubjectId};
use crate::{Ticks, Value};

/// Why a contribution was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextError {
    /// The subject is not in the registry.
    UnknownSubject(SubjectId),
    /// The caller does not have write authority over this subject.
    ///
    /// Reserved: this version has no custody table, and every admitted
    /// plugin may write any subject. The variant exists so the contract
    /// does not change when the table arrives.
    NotPermitted,
    /// The steward's tables are full.
    Full,
}

impl core::fmt::Display for ContextError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnknownSubject(id) => write!(f, "unknown subject `{id}`"),
            Self::NotPermitted => f.write_str("not permitted"),
            Self::Full => f.write_str("the steward is full"),
        }
    }
}

/// What a plugin may ask of the steward.
pub trait Context {
    /// Announce a thing. Returns its canonical id, which may be one the
    /// steward already held under another addressing.
    fn announce(
        &mut self,
        announcement: SubjectAnnouncement,
    ) -> Result<SubjectId, ContextError>;

    /// Replace a subject's state.
    fn update(
        &mut self,
        id: &SubjectId,
        state: Value,
    ) -> Result<(), ContextError>;

    /// Retract a subject.
    fn retract(&mut self, id: &SubjectId) -> Result<(), ContextError>;

    /// The fabric's clock.
    fn now(&self) -> Ticks;
}

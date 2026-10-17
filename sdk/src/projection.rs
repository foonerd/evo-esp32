// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Projections: the only thing a consumer reads.
//!
//! The panel and the network never read a subject or address a plugin.
//! They ask the steward for a projection, keyed to a rack (what does this
//! concern hold?) or a subject (what is known about this thing?), and
//! the steward composes it from every contribution it holds.

use alloc::string::String;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

use crate::happening::Seq;
use crate::subject::{SubjectId, SubjectRecord};

/// What a projection is keyed to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "by")]
pub enum ProjectionKey {
    /// Everything in one rack.
    Rack {
        /// The rack's name.
        name: String,
    },
    /// Everything about one subject.
    Subject {
        /// The subject's id.
        id: SubjectId,
    },
}

/// A composed view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Projection {
    /// What it is keyed to.
    pub key: ProjectionKey,
    /// The stream position it is consistent with. A consumer that then
    /// subscribes from here sees every change after this view.
    pub as_of: Seq,
    /// The subjects in view.
    pub subjects: Vec<SubjectRecord>,
}

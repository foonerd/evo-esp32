// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Wardens: plugins that take custody of sustained work.
//!
//! Playback is the first warden: it is handed an assignment (play this
//! track to this output), works at it over time, reports progress through
//! its [`Context`], and is told when to stop. On a microcontroller there
//! is no thread per warden; the steward ticks every warden with custody
//! on its loop, and the warden does a bounded slice of work per tick.

use alloc::string::String;
use serde::{Deserialize, Serialize};

use crate::context::Context;
use crate::plugin::Plugin;
use crate::subject::SubjectId;
use crate::Value;

/// Names one custody for its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CustodyHandle(pub u32);

/// Work entrusted to a warden.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assignment {
    /// The subject the work is about.
    pub subject: SubjectId,
    /// What is asked, in the shelf's shape.
    pub instruction: Value,
}

/// A change to work in progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CourseCorrection {
    /// The new instruction, in the shelf's shape.
    pub instruction: Value,
}

/// Why custody could not be taken or corrected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustodyError(pub String);

impl core::fmt::Display for CustodyError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a tick of work came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// Still working; tick again.
    Working,
    /// The assignment is complete; the steward may release custody.
    Done,
    /// The warden wants ticking again no sooner than this many milliseconds.
    Idle(u32),
}

/// A plugin that takes custody of work.
pub trait Warden: Plugin {
    /// Take an assignment. Returns a handle the steward uses to refer to it.
    fn take_custody(
        &mut self,
        handle: CustodyHandle,
        assignment: Assignment,
        ctx: &mut dyn Context,
    ) -> Result<(), CustodyError>;

    /// Change the work in progress.
    fn course_correct(
        &mut self,
        handle: CustodyHandle,
        correction: CourseCorrection,
        ctx: &mut dyn Context,
    ) -> Result<(), CustodyError>;

    /// Stop and let go. Called once per handle; nothing about that handle
    /// is called after it.
    fn release_custody(&mut self, handle: CustodyHandle, ctx: &mut dyn Context);

    /// Do a bounded slice of work on one custody.
    fn tick(
        &mut self,
        handle: CustodyHandle,
        ctx: &mut dyn Context,
    ) -> Progress;
}

// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! The slot contracts: what each shelf's verbs, instructions and subject
//! states look like.
//!
//! The fabric carries [`Value`](crate::Value)s; a contract says what is in
//! them. A plugin that stocks a shelf and a consumer that reads its
//! projection both build against the types here, so the JSON on the wire
//! is one shape by construction. A new concern is a new module beside
//! [`audio`], with the same parts: the shelf names, the verbs and their
//! argument types, the instruction and correction types for wardens, and
//! the subject kinds with their state types.

pub mod audio;

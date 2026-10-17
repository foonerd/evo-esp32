// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Respondents: plugins that answer discrete requests.
//!
//! A request names a verb, optionally a subject, and carries arguments as
//! a [`Value`]. The steward dispatches it to the plugin whose stocking
//! covers the verb and returns the response to whoever asked: the panel,
//! the network projection, an appointment. Correlation ids pair the two.

use alloc::string::String;
use serde::{Deserialize, Serialize};

use crate::catalogue::ShelfRef;
use crate::context::Context;
use crate::plugin::Plugin;
use crate::subject::SubjectId;
use crate::Value;

/// Pairs a response with its request.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct CorrelationId(pub u32);

/// Something asked of a respondent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// Pairs the response.
    pub cid: CorrelationId,
    /// The shelf it is addressed to. The steward routes by this.
    pub shelf: ShelfRef,
    /// What is asked: `play`, `pause`, `seek`, `list`, in the shelf's
    /// vocabulary.
    pub verb: String,
    /// The subject it concerns, where it concerns one.
    pub subject: Option<SubjectId>,
    /// Arguments, in the verb's shape.
    pub args: Value,
}

/// Why a request was not satisfied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "fault")]
pub enum Fault {
    /// The verb is not one this respondent answers.
    UnknownVerb,
    /// The arguments are not in the verb's shape.
    BadArgs {
        /// What was wrong.
        detail: String,
    },
    /// The subject is not one this respondent knows.
    UnknownSubject,
    /// The respondent could not do it.
    Failed {
        /// Why, in its words.
        detail: String,
    },
}

/// The answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    /// The request it answers.
    pub cid: CorrelationId,
    /// The result, or why not.
    pub outcome: Result<Value, Fault>,
}

impl Response {
    /// A successful response to `req`.
    pub fn ok(req: &Request, value: Value) -> Self {
        Self {
            cid: req.cid,
            outcome: Ok(value),
        }
    }

    /// A failed response to `req`.
    pub fn fault(req: &Request, fault: Fault) -> Self {
        Self {
            cid: req.cid,
            outcome: Err(fault),
        }
    }
}

/// A plugin that answers requests.
pub trait Respondent: Plugin {
    /// Answer one request. Blocking; keep it short.
    fn handle(&mut self, req: &Request, ctx: &mut dyn Context) -> Response;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_carry_the_request_id() {
        let req = Request {
            cid: CorrelationId(7),
            shelf: ShelfRef::new("example", "echo"),
            verb: "play".into(),
            subject: None,
            args: Value::Null,
        };
        assert_eq!(Response::ok(&req, Value::Bool(true)).cid, CorrelationId(7));
        assert_eq!(
            Response::fault(&req, Fault::UnknownVerb).outcome,
            Err(Fault::UnknownVerb)
        );
    }

    #[test]
    fn faults_serialise_tagged() {
        let f = Fault::BadArgs {
            detail: "no position".into(),
        };
        assert_eq!(
            serde_json::to_string(&f).unwrap(),
            r#"{"fault":"bad_args","detail":"no position"}"#
        );
    }
}

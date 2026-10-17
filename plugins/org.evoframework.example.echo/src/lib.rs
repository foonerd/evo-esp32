// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! The smallest plugin that shows the whole contract.
//!
//! `org.evoframework.example.echo` stocks the `example/echo` shelf as a
//! singleton respondent. At load it announces one subject, `echo`, under
//! its own addressing. On the verb `echo` it answers with the arguments it
//! was given and writes them to that subject, so a consumer reading the
//! projection sees the last thing echoed. Any other verb is an
//! `UnknownVerb` fault.
//!
//! What to copy: the plugin depends on the SDK and nothing else; the only
//! handle it keeps is the subject id the steward returned; every effect
//! goes through the [`Context`] it is handed.
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

use evo_esp32_sdk::context::Context;
use evo_esp32_sdk::plugin::{
    InstanceShape, InteractionShape, LoadError, Plugin, PluginDescription,
    Stocking,
};
use evo_esp32_sdk::respondent::{Fault, Request, Respondent, Response};
use evo_esp32_sdk::subject::{
    ExternalAddressing, SubjectAnnouncement, SubjectId,
};
use evo_esp32_sdk::Value;

/// The plugin's name, as the catalogue and the steward know it.
pub const NAME: &str = "org.evoframework.example.echo";

static STOCKS: [Stocking; 1] = [Stocking {
    rack: "example",
    shelf: "echo",
    shape: 1,
}];

/// The echo plugin.
#[derive(Debug, Default)]
pub struct Echo {
    subject: Option<SubjectId>,
}

impl Echo {
    /// A plugin ready to be admitted.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Plugin for Echo {
    fn describe(&self) -> PluginDescription {
        PluginDescription {
            name: NAME,
            version: env!("CARGO_PKG_VERSION"),
            instance: InstanceShape::Singleton,
            interaction: InteractionShape::Respondent,
            stocks: &STOCKS,
        }
    }

    fn load(&mut self, ctx: &mut dyn Context) -> Result<(), LoadError> {
        let id = ctx
            .announce(SubjectAnnouncement {
                addressing: ExternalAddressing::new("echo", "last"),
                kind: "echo".into(),
                state: Value::Null,
            })
            .map_err(|e| LoadError(alloc::format!("announcing: {e}")))?;
        self.subject = Some(id);
        Ok(())
    }

    fn unload(&mut self, ctx: &mut dyn Context) {
        if let Some(id) = self.subject.take() {
            let _ = ctx.retract(&id);
        }
    }
}

impl Respondent for Echo {
    fn handle(&mut self, req: &Request, ctx: &mut dyn Context) -> Response {
        if req.verb != "echo" {
            return Response::fault(req, Fault::UnknownVerb);
        }
        if let Some(id) = &self.subject {
            if let Err(e) = ctx.update(id, req.args.clone()) {
                return Response::fault(
                    req,
                    Fault::Failed {
                        detail: alloc::format!("{e}"),
                    },
                );
            }
        }
        Response::ok(req, req.args.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_esp32_sdk::catalogue::ShelfRef;
    use evo_esp32_sdk::projection::ProjectionKey;
    use evo_esp32_sdk::respondent::CorrelationId;
    use evo_esp32_steward::{Config, Steward, Ticks};
    use serde_json::json;

    const CATALOGUE: &str = r#"
[[racks]]
name = "example"
family = "domain"
kinds = ["registrar"]
charter = "Minimal example rack."

[[racks.shelves]]
name = "echo"
shape = 1
description = "Echoes inputs back."
"#;

    #[test]
    fn the_catalogue_tool_finds_it_stocks_the_shelf() {
        let c = evo_esp32_catalogue::parse(CATALOGUE).unwrap();
        let report = evo_esp32_catalogue::check(&c, &[Echo::new().describe()]);
        assert!(report.is_clean(), "{report}");
    }

    #[test]
    fn echoes_and_the_projection_shows_the_last_echo() {
        let c = evo_esp32_catalogue::parse(CATALOGUE).unwrap();
        let mut s = Steward::new(c, Config::default()).unwrap();
        s.admit_respondent(Box::new(Echo::new()), Ticks(0)).unwrap();

        let shelf = ShelfRef::new("example", "echo");
        let req = Request {
            cid: CorrelationId(1),
            shelf: shelf.clone(),
            verb: "echo".into(),
            subject: None,
            args: json!({"n": 1}),
        };
        assert_eq!(
            s.request(&req, Ticks(1)).unwrap().outcome,
            Ok(json!({"n": 1}))
        );

        let bad = Request {
            verb: "shout".into(),
            ..req.clone()
        };
        assert_eq!(
            s.request(&bad, Ticks(2)).unwrap().outcome,
            Err(Fault::UnknownVerb)
        );

        let p = s.projection(ProjectionKey::Rack {
            name: "example".into(),
        });
        assert_eq!(p.subjects.len(), 1);
        assert_eq!(p.subjects[0].kind, "echo");
        assert_eq!(p.subjects[0].state, json!({"n": 1}));
        assert_eq!(p.subjects[0].last_writer, NAME);

        s.shutdown(Ticks(3));
        let p = s.projection(ProjectionKey::Rack {
            name: "example".into(),
        });
        assert!(p.subjects.is_empty());
    }
}

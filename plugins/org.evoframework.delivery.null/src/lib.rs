// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! A delivery respondent with nothing behind it.
//!
//! `org.evoframework.delivery.null` stocks `delivery/output`. It keeps one
//! `output` subject in the contract's [`State`] and answers `volume` and
//! `mute`; no sample reaches any hardware. A real sink (the ES8311 over
//! I2C, an I2S DAC) keeps the same verbs and the same subject and adds
//! the register writes.
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

use evo_esp32_sdk::context::Context;
use evo_esp32_sdk::contract::audio::delivery::{
    Mute, State, Volume, KIND, MUTE, VOLUME,
};
use evo_esp32_sdk::contract::audio::{from_value, shelf, to_value};
use evo_esp32_sdk::plugin::{
    InstanceShape, InteractionShape, LoadError, Plugin, PluginDescription,
    Stocking,
};
use evo_esp32_sdk::respondent::{Fault, Request, Respondent, Response};
use evo_esp32_sdk::subject::{
    ExternalAddressing, SubjectAnnouncement, SubjectId,
};

/// The plugin's name, as the catalogue and the steward know it.
pub const NAME: &str = "org.evoframework.delivery.null";

static STOCKS: [Stocking; 1] = [Stocking {
    rack: shelf::DELIVERY,
    shelf: shelf::OUTPUT,
    shape: shelf::SHAPE,
}];

/// The null sink.
#[derive(Debug)]
pub struct DeliveryNull {
    subject: Option<SubjectId>,
    state: State,
}

impl Default for DeliveryNull {
    fn default() -> Self {
        Self::new()
    }
}

impl DeliveryNull {
    /// A sink at 50 %, unmuted.
    pub fn new() -> Self {
        Self {
            subject: None,
            state: State {
                sink: "null".into(),
                volume: 50,
                muted: false,
            },
        }
    }

    fn publish(&self, ctx: &mut dyn Context) -> Result<(), Fault> {
        match &self.subject {
            Some(id) => ctx.update(id, to_value(&self.state)).map_err(|e| {
                Fault::Failed {
                    detail: alloc::format!("{e}"),
                }
            }),
            None => Ok(()),
        }
    }
}

impl Plugin for DeliveryNull {
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
                addressing: ExternalAddressing::new("output", "main"),
                kind: KIND.into(),
                state: to_value(&self.state),
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

impl Respondent for DeliveryNull {
    fn handle(&mut self, req: &Request, ctx: &mut dyn Context) -> Response {
        let applied = match req.verb.as_str() {
            VOLUME => from_value::<Volume>(&req.args).map(|v| {
                self.state.volume = v.level.min(100);
            }),
            MUTE => from_value::<Mute>(&req.args).map(|m| {
                self.state.muted = m.on;
            }),
            _ => return Response::fault(req, Fault::UnknownVerb),
        };
        if let Err(detail) = applied {
            return Response::fault(req, Fault::BadArgs { detail });
        }
        match self.publish(ctx) {
            Ok(()) => Response::ok(req, to_value(&self.state)),
            Err(f) => Response::fault(req, f),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_esp32_sdk::catalogue::{Catalogue, Family, Kind, Rack, Shelf};
    use evo_esp32_sdk::projection::ProjectionKey;
    use evo_esp32_sdk::respondent::CorrelationId;
    use evo_esp32_sdk::Ticks;
    use evo_esp32_steward::{Config, Steward};

    fn catalogue() -> Catalogue {
        Catalogue {
            racks: alloc::vec![Rack {
                name: shelf::DELIVERY.into(),
                family: Family::Domain,
                kinds: alloc::vec![Kind::Transformer],
                charter: String::new(),
                shelves: alloc::vec![Shelf {
                    name: shelf::OUTPUT.into(),
                    shape: shelf::SHAPE,
                    description: String::new(),
                }],
            }],
        }
    }

    fn ask(s: &mut Steward, verb: &str, args: serde_json::Value) -> Response {
        s.request(
            &Request {
                cid: CorrelationId(1),
                shelf: shelf::delivery_output(),
                verb: verb.into(),
                subject: None,
                args,
            },
            Ticks(0),
        )
        .unwrap()
    }

    #[test]
    fn volume_and_mute_land_on_the_subject() {
        let mut s = Steward::new(catalogue(), Config::default()).unwrap();
        s.admit_respondent(Box::new(DeliveryNull::new()), Ticks(0))
            .unwrap();
        let r = ask(&mut s, VOLUME, to_value(&Volume { level: 130 }));
        let st: State = from_value(&r.outcome.unwrap()).unwrap();
        assert_eq!(st.volume, 100, "clamped");
        ask(&mut s, MUTE, to_value(&Mute { on: true }));
        let p = s.projection(ProjectionKey::Rack {
            name: shelf::DELIVERY.into(),
        });
        let st: State = from_value(&p.subjects[0].state).unwrap();
        assert!(st.muted);
        assert_eq!(st.sink, "null");
    }

    #[test]
    fn bad_args_and_unknown_verbs_fault() {
        let mut s = Steward::new(catalogue(), Config::default()).unwrap();
        s.admit_respondent(Box::new(DeliveryNull::new()), Ticks(0))
            .unwrap();
        assert!(matches!(
            ask(&mut s, VOLUME, serde_json::json!({"level": "loud"})).outcome,
            Err(Fault::BadArgs { .. })
        ));
        assert_eq!(
            ask(&mut s, "louder", serde_json::Value::Null).outcome,
            Err(Fault::UnknownVerb)
        );
    }
}

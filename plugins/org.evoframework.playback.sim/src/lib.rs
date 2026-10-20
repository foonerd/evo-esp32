// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! A playback warden that plays against the clock and decodes nothing.
//!
//! `org.evoframework.playback.sim` stocks `playback/decode`. Handed a
//! [`Play`] it takes custody, keeps one `playback` subject in the state
//! the contract names, and on every tick advances the position by the
//! milliseconds elapsed; at the track's end it reports `Done` and the
//! steward releases it. Pause, resume and seek are course corrections.
//!
//! It exists so the fabric can be driven end to end, projections read
//! and pages drawn, before a decoder and an output exist; a real decoder
//! replaces the tick body and keeps everything else. One custody at a
//! time: a second `Play` while one is in hand is refused, as a single
//! output can carry one stream.
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

use evo_esp32_sdk::context::Context;
use evo_esp32_sdk::contract::audio::playback::{
    Correction, Play, State, Status, KIND,
};
use evo_esp32_sdk::contract::audio::{from_value, shelf, to_value};
use evo_esp32_sdk::plugin::{
    InstanceShape, InteractionShape, LoadError, Plugin, PluginDescription,
    Stocking,
};
use evo_esp32_sdk::subject::{
    ExternalAddressing, SubjectAnnouncement, SubjectId,
};
use evo_esp32_sdk::warden::{
    Assignment, CourseCorrection, CustodyError, CustodyHandle, Progress, Warden,
};
use evo_esp32_sdk::Ticks;

/// The plugin's name, as the catalogue and the steward know it.
pub const NAME: &str = "org.evoframework.playback.sim";

/// How long the warden asks to be left alone while paused.
const PAUSED_IDLE_MS: u32 = 250;
/// How often the position is written while playing.
const PLAYING_IDLE_MS: u32 = 100;

static STOCKS: [Stocking; 1] = [Stocking {
    rack: shelf::PLAYBACK,
    shelf: shelf::DECODE,
    shape: shelf::SHAPE,
}];

/// The custody in hand.
#[derive(Debug)]
struct InHand {
    handle: CustodyHandle,
    state: State,
    /// When the position was last advanced.
    last: Ticks,
}

/// The simulated playback warden.
#[derive(Debug, Default)]
pub struct PlaybackSim {
    subject: Option<SubjectId>,
    in_hand: Option<InHand>,
}

impl PlaybackSim {
    /// A plugin ready to be admitted.
    pub fn new() -> Self {
        Self::default()
    }

    fn publish(&self, ctx: &mut dyn Context, state: &State) {
        if let Some(id) = &self.subject {
            let _ = ctx.update(id, to_value(state));
        }
    }
}

impl Plugin for PlaybackSim {
    fn describe(&self) -> PluginDescription {
        PluginDescription {
            name: NAME,
            version: env!("CARGO_PKG_VERSION"),
            instance: InstanceShape::Singleton,
            interaction: InteractionShape::Warden,
            stocks: &STOCKS,
        }
    }

    fn load(&mut self, ctx: &mut dyn Context) -> Result<(), LoadError> {
        let id = ctx
            .announce(SubjectAnnouncement {
                addressing: ExternalAddressing::new("playback", "now"),
                kind: KIND.into(),
                state: to_value(&State::default()),
            })
            .map_err(|e| LoadError(alloc::format!("announcing: {e}")))?;
        self.subject = Some(id);
        Ok(())
    }

    fn unload(&mut self, ctx: &mut dyn Context) {
        self.in_hand = None;
        if let Some(id) = self.subject.take() {
            let _ = ctx.retract(&id);
        }
    }
}

impl Warden for PlaybackSim {
    fn take_custody(
        &mut self,
        handle: CustodyHandle,
        assignment: Assignment,
        ctx: &mut dyn Context,
    ) -> Result<(), CustodyError> {
        if self.in_hand.is_some() {
            return Err(CustodyError("already playing".into()));
        }
        let play: Play = from_value(&assignment.instruction)
            .map_err(|e| CustodyError(alloc::format!("instruction: {e}")))?;
        let position_ms = match play.track.duration_ms {
            Some(d) => play.start_ms.min(d),
            None => play.start_ms,
        };
        let state = State {
            status: Status::Playing,
            track: Some(play.track),
            position_ms,
            format: None,
        };
        self.publish(ctx, &state);
        self.in_hand = Some(InHand {
            handle,
            state,
            last: ctx.now(),
        });
        Ok(())
    }

    fn course_correct(
        &mut self,
        handle: CustodyHandle,
        correction: CourseCorrection,
        ctx: &mut dyn Context,
    ) -> Result<(), CustodyError> {
        let now = ctx.now();
        let Some(h) = self.in_hand.as_mut().filter(|h| h.handle == handle)
        else {
            return Err(CustodyError("no such custody".into()));
        };
        let c: Correction = from_value(&correction.instruction)
            .map_err(|e| CustodyError(alloc::format!("correction: {e}")))?;
        advance(h, now);
        match c {
            Correction::Pause => h.state.status = Status::Paused,
            Correction::Resume => h.state.status = Status::Playing,
            Correction::Seek { position_ms } => {
                let cap = h.state.track.as_ref().and_then(|t| t.duration_ms);
                h.state.position_ms = match cap {
                    Some(d) => position_ms.min(d),
                    None => position_ms,
                };
            }
        }
        let state = h.state.clone();
        self.publish(ctx, &state);
        Ok(())
    }

    fn release_custody(
        &mut self,
        handle: CustodyHandle,
        ctx: &mut dyn Context,
    ) {
        if self.in_hand.as_ref().is_some_and(|h| h.handle == handle) {
            self.in_hand = None;
            self.publish(ctx, &State::default());
        }
    }

    fn tick(
        &mut self,
        handle: CustodyHandle,
        ctx: &mut dyn Context,
    ) -> Progress {
        let now = ctx.now();
        let Some(h) = self.in_hand.as_mut().filter(|h| h.handle == handle)
        else {
            return Progress::Done;
        };
        if h.state.status == Status::Paused {
            h.last = now;
            return Progress::Idle(PAUSED_IDLE_MS);
        }
        advance(h, now);
        let state = h.state.clone();
        let at_end = state
            .track
            .as_ref()
            .and_then(|t| t.duration_ms)
            .is_some_and(|d| state.position_ms >= d);
        self.publish(ctx, &state);
        if at_end {
            Progress::Done
        } else {
            Progress::Idle(PLAYING_IDLE_MS)
        }
    }
}

/// Move the position by the time since it was last moved, capped at the
/// track's length where it has one.
fn advance(h: &mut InHand, now: Ticks) {
    if h.state.status == Status::Playing {
        let elapsed = now.since(h.last);
        let next = h.state.position_ms.saturating_add(elapsed);
        h.state.position_ms =
            match h.state.track.as_ref().and_then(|t| t.duration_ms) {
                Some(d) => next.min(d),
                None => next,
            };
    }
    h.last = now;
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_esp32_sdk::catalogue::{Catalogue, Family, Kind, Rack, Shelf};
    use evo_esp32_sdk::contract::audio::Track;
    use evo_esp32_sdk::projection::ProjectionKey;
    use evo_esp32_steward::{Config, Steward};

    fn catalogue() -> Catalogue {
        Catalogue {
            racks: alloc::vec![Rack {
                name: shelf::PLAYBACK.into(),
                family: Family::Domain,
                kinds: alloc::vec![Kind::Transformer],
                charter: String::new(),
                shelves: alloc::vec![Shelf {
                    name: shelf::DECODE.into(),
                    shape: shelf::SHAPE,
                    description: String::new(),
                }],
            }],
        }
    }

    fn track(ms: Option<u64>) -> Track {
        Track {
            uri: "file:///a".into(),
            title: "a".into(),
            artist: None,
            album: None,
            duration_ms: ms,
            sample_rate: None,
            art: None,
        }
    }

    fn state(s: &Steward) -> State {
        let p = s.projection(ProjectionKey::Rack {
            name: shelf::PLAYBACK.into(),
        });
        from_value(&p.subjects[0].state).unwrap()
    }

    fn assign(s: &mut Steward, t: Track, now: u64) -> CustodyHandle {
        s.assign(
            &shelf::playback_decode(),
            Assignment {
                subject: SubjectId::new("playback/now"),
                instruction: to_value(&Play {
                    track: t,
                    start_ms: 0,
                }),
            },
            Ticks(now),
        )
        .unwrap()
    }

    #[test]
    fn plays_to_the_end_and_releases() {
        let mut s = Steward::new(catalogue(), Config::default()).unwrap();
        s.admit_warden(Box::new(PlaybackSim::new()), Ticks(0))
            .unwrap();
        assert_eq!(state(&s).status, Status::Stopped);
        assign(&mut s, track(Some(1000)), 0);
        assert_eq!(state(&s).status, Status::Playing);
        s.tick(Ticks(400));
        assert_eq!(state(&s).position_ms, 400);
        s.tick(Ticks(900));
        assert_eq!(state(&s).position_ms, 900);
        assert_eq!(s.custodies().count(), 1);
        s.tick(Ticks(1500));
        let st = state(&s);
        assert_eq!(st.status, Status::Stopped, "released at the end");
        assert_eq!(s.custodies().count(), 0);
    }

    #[test]
    fn pause_holds_resume_continues_seek_moves() {
        let mut s = Steward::new(catalogue(), Config::default()).unwrap();
        s.admit_warden(Box::new(PlaybackSim::new()), Ticks(0))
            .unwrap();
        let h = assign(&mut s, track(Some(10_000)), 0);
        s.tick(Ticks(1000));
        s.correct(
            h,
            CourseCorrection {
                instruction: to_value(&Correction::Pause),
            },
            Ticks(1000),
        )
        .unwrap();
        s.tick(Ticks(3000));
        assert_eq!(state(&s).position_ms, 1000);
        assert_eq!(state(&s).status, Status::Paused);
        s.correct(
            h,
            CourseCorrection {
                instruction: to_value(&Correction::Resume),
            },
            Ticks(3000),
        )
        .unwrap();
        s.tick(Ticks(3500));
        assert_eq!(state(&s).position_ms, 1500);
        s.correct(
            h,
            CourseCorrection {
                instruction: to_value(&Correction::Seek { position_ms: 9900 }),
            },
            Ticks(3500),
        )
        .unwrap();
        assert_eq!(state(&s).position_ms, 9900);
        s.tick(Ticks(3700));
        assert_eq!(
            state(&s).status,
            Status::Stopped,
            "seek near the end, then the end"
        );
    }

    #[test]
    fn a_stream_has_no_end() {
        let mut s = Steward::new(catalogue(), Config::default()).unwrap();
        s.admit_warden(Box::new(PlaybackSim::new()), Ticks(0))
            .unwrap();
        assign(&mut s, track(None), 0);
        s.tick(Ticks(60_000));
        assert_eq!(state(&s).position_ms, 60_000);
        assert_eq!(s.custodies().count(), 1);
    }

    #[test]
    fn a_second_play_is_refused_and_release_stops() {
        let mut s = Steward::new(catalogue(), Config::default()).unwrap();
        s.admit_warden(Box::new(PlaybackSim::new()), Ticks(0))
            .unwrap();
        let h = assign(&mut s, track(Some(5000)), 0);
        assert!(s
            .assign(
                &shelf::playback_decode(),
                Assignment {
                    subject: SubjectId::new("playback/now"),
                    instruction: to_value(&Play {
                        track: track(Some(5000)),
                        start_ms: 0
                    }),
                },
                Ticks(10),
            )
            .is_err());
        s.release(h, Ticks(20)).unwrap();
        assert_eq!(state(&s).status, Status::Stopped);
        assert_eq!(s.custodies().count(), 0);
    }

    #[test]
    fn a_bad_instruction_is_refused() {
        let mut s = Steward::new(catalogue(), Config::default()).unwrap();
        s.admit_warden(Box::new(PlaybackSim::new()), Ticks(0))
            .unwrap();
        assert!(s
            .assign(
                &shelf::playback_decode(),
                Assignment {
                    subject: SubjectId::new("playback/now"),
                    instruction: serde_json::json!({"track": 5}),
                },
                Ticks(0),
            )
            .is_err());
    }
}

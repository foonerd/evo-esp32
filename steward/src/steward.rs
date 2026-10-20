// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The steward.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use evo_esp32_sdk::catalogue::{Catalogue, CatalogueError, ShelfRef};
use evo_esp32_sdk::happening::{HappeningKind, Seq};
use evo_esp32_sdk::plugin::{Health, LoadError, Plugin, PluginDescription};
use evo_esp32_sdk::projection::{Projection, ProjectionKey};
use evo_esp32_sdk::respondent::{Request, Respondent, Response};
use evo_esp32_sdk::subject::SubjectId;
use evo_esp32_sdk::warden::{
    Assignment, CourseCorrection, CustodyError, CustodyHandle, Progress, Warden,
};
use evo_esp32_sdk::Ticks;

use crate::admit::{self, AdmissionError, Admitted};
use crate::context::Context;
use crate::happenings::Replay;
use crate::tables::Tables;

/// Sizes the steward is built with. Nothing grows past them at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// Plugins the steward will admit.
    pub max_plugins: usize,
    /// Subjects the tables will hold.
    pub max_subjects: usize,
    /// Happenings the ring keeps for replay.
    pub happenings_capacity: usize,
    /// Custodies open at once across all wardens.
    pub max_custodies: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_plugins: 16,
            max_subjects: 64,
            happenings_capacity: 128,
            max_custodies: 8,
        }
    }
}

/// Why a request or an assignment could not be delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchError {
    /// No admitted plugin of the right interaction shape stocks the shelf.
    NoPluginFor(ShelfRef),
    /// The handle names no open custody.
    UnknownCustody(CustodyHandle),
    /// Every custody slot is taken.
    CustodiesFull,
    /// The warden refused.
    Refused(CustodyError),
}

impl core::fmt::Display for DispatchError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoPluginFor(s) => write!(f, "nothing stocks `{s}`"),
            Self::UnknownCustody(h) => write!(f, "no custody {}", h.0),
            Self::CustodiesFull => f.write_str("no free custody"),
            Self::Refused(e) => write!(f, "warden refused: {e}"),
        }
    }
}

enum Slot {
    Respondent(Box<dyn Respondent>),
    Warden(Box<dyn Warden>),
}

impl Slot {
    fn plugin(&self) -> &dyn Plugin {
        match self {
            Slot::Respondent(p) => p.as_ref(),
            Slot::Warden(p) => p.as_ref(),
        }
    }

    fn plugin_mut(&mut self) -> &mut dyn Plugin {
        match self {
            Slot::Respondent(p) => p.as_mut(),
            Slot::Warden(p) => p.as_mut(),
        }
    }
}

struct Loaded {
    description: PluginDescription,
    slot: Slot,
}

impl Loaded {
    fn stocks(&self, shelf: &ShelfRef) -> bool {
        self.description
            .stocks
            .iter()
            .any(|s| s.rack == shelf.rack && s.shelf == shelf.shelf)
    }

    fn stocks_rack(&self, rack: &str) -> bool {
        self.description.stocks.iter().any(|s| s.rack == rack)
    }
}

struct Custody {
    handle: CustodyHandle,
    plugin: usize,
    due: Ticks,
}

/// The steward: the sole authority of the device.
pub struct Steward {
    catalogue: Catalogue,
    config: Config,
    plugins: Vec<Loaded>,
    tables: Tables,
    custodies: Vec<Custody>,
    next_handle: u32,
}

impl Steward {
    /// A steward for `catalogue`, sized by `config`. Refuses a catalogue
    /// that is not well formed.
    pub fn new(
        catalogue: Catalogue,
        config: Config,
    ) -> Result<Self, CatalogueError> {
        catalogue.validate()?;
        Ok(Self {
            catalogue,
            config,
            plugins: Vec::new(),
            tables: Tables::new(
                config.max_subjects,
                config.happenings_capacity,
            ),
            custodies: Vec::new(),
            next_handle: 1,
        })
    }

    /// The catalogue as admitted against.
    pub fn catalogue(&self) -> &Catalogue {
        &self.catalogue
    }

    /// Every admitted plugin, in admission order.
    pub fn plugins(&self) -> impl Iterator<Item = &PluginDescription> {
        self.plugins.iter().map(|l| &l.description)
    }

    /// Admit and load a respondent.
    pub fn admit_respondent(
        &mut self,
        plugin: Box<dyn Respondent>,
        now: Ticks,
    ) -> Result<Admitted, AdmissionError> {
        self.admit(Slot::Respondent(plugin), now)
    }

    /// Admit and load a warden.
    pub fn admit_warden(
        &mut self,
        plugin: Box<dyn Warden>,
        now: Ticks,
    ) -> Result<Admitted, AdmissionError> {
        self.admit(Slot::Warden(plugin), now)
    }

    fn admit(
        &mut self,
        mut slot: Slot,
        now: Ticks,
    ) -> Result<Admitted, AdmissionError> {
        if self.plugins.len() >= self.config.max_plugins {
            return Err(AdmissionError::Full);
        }
        let description = slot.plugin().describe();
        admit::check(&self.catalogue, &self.tables.plugin_names, &description)?;
        let index = self.plugins.len();
        self.tables.plugin_names.push(description.name);
        let load = {
            let mut ctx = Context::new(&mut self.tables, index, now);
            slot.plugin_mut().load(&mut ctx)
        };
        if let Err(LoadError(why)) = load {
            self.tables.plugin_names.pop();
            self.tables.retract_all(index, now);
            return Err(AdmissionError::LoadFailed {
                plugin: String::from(description.name),
                detail: why,
            });
        }
        self.tables.ring.push(
            now,
            None,
            HappeningKind::PluginLoaded {
                plugin: String::from(description.name),
            },
        );
        self.plugins.push(Loaded {
            description: description.clone(),
            slot,
        });
        Ok(Admitted { description, index })
    }

    /// Deliver a request to the first respondent stocking its shelf.
    pub fn request(
        &mut self,
        req: &Request,
        now: Ticks,
    ) -> Result<Response, DispatchError> {
        let index = self
            .plugins
            .iter()
            .position(|l| {
                matches!(l.slot, Slot::Respondent(_)) && l.stocks(&req.shelf)
            })
            .ok_or_else(|| DispatchError::NoPluginFor(req.shelf.clone()))?;
        let mut ctx = Context::new(&mut self.tables, index, now);
        match &mut self.plugins[index].slot {
            Slot::Respondent(p) => Ok(p.handle(req, &mut ctx)),
            Slot::Warden(_) => unreachable!("position matched a respondent"),
        }
    }

    /// Hand an assignment to the first warden stocking `shelf`.
    pub fn assign(
        &mut self,
        shelf: &ShelfRef,
        assignment: Assignment,
        now: Ticks,
    ) -> Result<CustodyHandle, DispatchError> {
        if self.custodies.len() >= self.config.max_custodies {
            return Err(DispatchError::CustodiesFull);
        }
        let index = self
            .plugins
            .iter()
            .position(|l| matches!(l.slot, Slot::Warden(_)) && l.stocks(shelf))
            .ok_or_else(|| DispatchError::NoPluginFor(shelf.clone()))?;
        let handle = CustodyHandle(self.next_handle);
        self.next_handle += 1;
        let taken = {
            let mut ctx = Context::new(&mut self.tables, index, now);
            match &mut self.plugins[index].slot {
                Slot::Warden(w) => w.take_custody(handle, assignment, &mut ctx),
                Slot::Respondent(_) => {
                    unreachable!("position matched a warden")
                }
            }
        };
        taken.map_err(DispatchError::Refused)?;
        self.custodies.push(Custody {
            handle,
            plugin: index,
            due: now,
        });
        self.tables.ring.push(
            now,
            None,
            HappeningKind::CustodyTaken {
                warden: String::from(self.plugins[index].description.name),
            },
        );
        Ok(handle)
    }

    /// Change the work under `handle`.
    pub fn correct(
        &mut self,
        handle: CustodyHandle,
        correction: CourseCorrection,
        now: Ticks,
    ) -> Result<(), DispatchError> {
        let c = self
            .custodies
            .iter_mut()
            .find(|c| c.handle == handle)
            .ok_or(DispatchError::UnknownCustody(handle))?;
        c.due = now;
        let index = c.plugin;
        let mut ctx = Context::new(&mut self.tables, index, now);
        match &mut self.plugins[index].slot {
            Slot::Warden(w) => w
                .course_correct(handle, correction, &mut ctx)
                .map_err(DispatchError::Refused),
            Slot::Respondent(_) => unreachable!("custody belongs to a warden"),
        }
    }

    /// Release the work under `handle`.
    pub fn release(
        &mut self,
        handle: CustodyHandle,
        now: Ticks,
    ) -> Result<(), DispatchError> {
        let pos = self
            .custodies
            .iter()
            .position(|c| c.handle == handle)
            .ok_or(DispatchError::UnknownCustody(handle))?;
        let index = self.custodies.remove(pos).plugin;
        self.release_on(index, handle, now, None);
        Ok(())
    }

    fn release_on(
        &mut self,
        index: usize,
        handle: CustodyHandle,
        now: Ticks,
        reason: Option<String>,
    ) {
        {
            let mut ctx = Context::new(&mut self.tables, index, now);
            if let Slot::Warden(w) = &mut self.plugins[index].slot {
                w.release_custody(handle, &mut ctx);
            }
        }
        self.tables.ring.push(
            now,
            None,
            HappeningKind::CustodyReleased {
                warden: String::from(self.plugins[index].description.name),
                reason,
            },
        );
    }

    /// Tick every custody that is due. Returns when the next one is due,
    /// if any is open, so the caller can sleep until then.
    pub fn tick(&mut self, now: Ticks) -> Option<Ticks> {
        let mut i = 0;
        while i < self.custodies.len() {
            if self.custodies[i].due > now {
                i += 1;
                continue;
            }
            let (handle, index) =
                (self.custodies[i].handle, self.custodies[i].plugin);
            let progress = {
                let mut ctx = Context::new(&mut self.tables, index, now);
                match &mut self.plugins[index].slot {
                    Slot::Warden(w) => w.tick(handle, &mut ctx),
                    Slot::Respondent(_) => {
                        unreachable!("custody belongs to a warden")
                    }
                }
            };
            match progress {
                Progress::Working => {
                    self.custodies[i].due = now;
                    i += 1;
                }
                Progress::Idle(ms) => {
                    self.custodies[i].due = Ticks(now.0 + u64::from(ms));
                    i += 1;
                }
                Progress::Done => {
                    self.custodies.remove(i);
                    self.release_on(index, handle, now, None);
                }
                Progress::Failed(reason) => {
                    self.custodies.remove(i);
                    self.release_on(index, handle, now, Some(reason));
                }
            }
        }
        self.custodies.iter().map(|c| c.due).min()
    }

    /// Open custodies, oldest first.
    pub fn custodies(&self) -> impl Iterator<Item = CustodyHandle> + '_ {
        self.custodies.iter().map(|c| c.handle)
    }

    /// Ask every plugin for its health; emit a happening for each that is
    /// unhealthy.
    pub fn health(&mut self, now: Ticks) -> Vec<(&'static str, Health)> {
        let mut out = Vec::with_capacity(self.plugins.len());
        for l in &self.plugins {
            let h = l.slot.plugin().health();
            if let Health::Unhealthy { detail } = &h {
                self.tables.ring.push(
                    now,
                    None,
                    HappeningKind::PluginUnhealthy {
                        plugin: String::from(l.description.name),
                        detail: detail.clone(),
                    },
                );
            }
            out.push((l.description.name, h));
        }
        out
    }

    /// A composed view keyed to a rack or a subject.
    pub fn projection(&self, key: ProjectionKey) -> Projection {
        let as_of = self.tables.ring.latest().unwrap_or(Seq(0));
        let subjects = match &key {
            ProjectionKey::Rack { name } => {
                let stockers: Vec<usize> = self
                    .plugins
                    .iter()
                    .enumerate()
                    .filter(|(_, l)| l.stocks_rack(name))
                    .map(|(i, _)| i)
                    .collect();
                self.tables.records_touched_by(&stockers)
            }
            ProjectionKey::Subject { id } => {
                self.tables.record(id).into_iter().collect()
            }
        };
        Projection {
            key,
            as_of,
            subjects,
        }
    }

    /// The record of one subject, if held.
    pub fn subject(
        &self,
        id: &SubjectId,
    ) -> Option<evo_esp32_sdk::subject::SubjectRecord> {
        self.tables.record(id)
    }

    /// Everything on the happenings stream after `since`.
    pub fn happenings_since(&self, since: Seq) -> Replay {
        self.tables.ring.since(since)
    }

    /// Release every custody and unload every plugin, newest first.
    pub fn shutdown(&mut self, now: Ticks) {
        while let Some(c) = self.custodies.pop() {
            self.release_on(c.plugin, c.handle, now, None);
        }
        for index in (0..self.plugins.len()).rev() {
            let mut ctx = Context::new(&mut self.tables, index, now);
            self.plugins[index].slot.plugin_mut().unload(&mut ctx);
            self.tables.retract_all(index, now);
        }
        self.plugins.clear();
        self.tables.plugin_names.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use evo_esp32_sdk::catalogue::{Family, Kind, Rack, Shelf};
    use evo_esp32_sdk::plugin::{InstanceShape, InteractionShape, Stocking};
    use evo_esp32_sdk::respondent::{CorrelationId, Fault};
    use evo_esp32_sdk::subject::{ExternalAddressing, SubjectAnnouncement};
    use evo_esp32_sdk::Value;
    use serde_json::json;

    fn catalogue() -> Catalogue {
        Catalogue {
            racks: vec![Rack {
                name: "audio".into(),
                family: Family::Domain,
                kinds: vec![Kind::Producer, Kind::Transformer],
                charter: String::new(),
                shelves: vec![
                    Shelf {
                        name: "source".into(),
                        shape: 1,
                        description: String::new(),
                    },
                    Shelf {
                        name: "playback".into(),
                        shape: 2,
                        description: String::new(),
                    },
                ],
            }],
        }
    }

    struct Echo;
    static ECHO_STOCKS: [Stocking; 1] = [Stocking {
        rack: "audio",
        shelf: "source",
        shape: 1,
    }];
    impl Plugin for Echo {
        fn describe(&self) -> PluginDescription {
            PluginDescription {
                name: "test.echo",
                version: "0",
                instance: InstanceShape::Singleton,
                interaction: InteractionShape::Respondent,
                stocks: &ECHO_STOCKS,
            }
        }
        fn load(
            &mut self,
            ctx: &mut dyn evo_esp32_sdk::context::Context,
        ) -> Result<(), LoadError> {
            ctx.announce(SubjectAnnouncement {
                addressing: ExternalAddressing::new("echo", "last"),
                kind: "echo".into(),
                state: Value::Null,
            })
            .map(|_| ())
            .map_err(|e| LoadError(alloc::format!("{e}")))
        }
        fn unload(&mut self, _ctx: &mut dyn evo_esp32_sdk::context::Context) {}
    }
    impl Respondent for Echo {
        fn handle(
            &mut self,
            req: &Request,
            ctx: &mut dyn evo_esp32_sdk::context::Context,
        ) -> Response {
            if req.verb != "echo" {
                return Response::fault(req, Fault::UnknownVerb);
            }
            let _ = ctx.update(&SubjectId::new("echo/1"), req.args.clone());
            Response::ok(req, req.args.clone())
        }
    }

    struct Player {
        ticks_left: u32,
    }
    static PLAYER_STOCKS: [Stocking; 1] = [Stocking {
        rack: "audio",
        shelf: "playback",
        shape: 2,
    }];
    impl Plugin for Player {
        fn describe(&self) -> PluginDescription {
            PluginDescription {
                name: "test.player",
                version: "0",
                instance: InstanceShape::Singleton,
                interaction: InteractionShape::Warden,
                stocks: &PLAYER_STOCKS,
            }
        }
        fn load(
            &mut self,
            _ctx: &mut dyn evo_esp32_sdk::context::Context,
        ) -> Result<(), LoadError> {
            Ok(())
        }
        fn unload(&mut self, _ctx: &mut dyn evo_esp32_sdk::context::Context) {}
    }
    impl Warden for Player {
        fn take_custody(
            &mut self,
            _h: CustodyHandle,
            a: Assignment,
            ctx: &mut dyn evo_esp32_sdk::context::Context,
        ) -> Result<(), CustodyError> {
            if a.instruction.get("ticks").is_none() {
                return Err(CustodyError("no ticks".into()));
            }
            self.ticks_left = a.instruction["ticks"].as_u64().unwrap() as u32;
            let _ = ctx.update(&a.subject, json!({"status": "play"}));
            Ok(())
        }
        fn course_correct(
            &mut self,
            _h: CustodyHandle,
            c: CourseCorrection,
            _ctx: &mut dyn evo_esp32_sdk::context::Context,
        ) -> Result<(), CustodyError> {
            self.ticks_left = c.instruction["ticks"].as_u64().unwrap() as u32;
            Ok(())
        }
        fn release_custody(
            &mut self,
            _h: CustodyHandle,
            _ctx: &mut dyn evo_esp32_sdk::context::Context,
        ) {
            self.ticks_left = 0;
        }
        fn tick(
            &mut self,
            _h: CustodyHandle,
            _ctx: &mut dyn evo_esp32_sdk::context::Context,
        ) -> Progress {
            if self.ticks_left == 0 {
                return Progress::Done;
            }
            self.ticks_left -= 1;
            Progress::Idle(10)
        }
    }

    struct Impostor;
    static IMPOSTOR_STOCKS: [Stocking; 1] = [Stocking {
        rack: "audio",
        shelf: "source",
        shape: 9,
    }];
    impl Plugin for Impostor {
        fn describe(&self) -> PluginDescription {
            PluginDescription {
                name: "test.impostor",
                version: "0",
                instance: InstanceShape::Singleton,
                interaction: InteractionShape::Respondent,
                stocks: &IMPOSTOR_STOCKS,
            }
        }
        fn load(
            &mut self,
            _ctx: &mut dyn evo_esp32_sdk::context::Context,
        ) -> Result<(), LoadError> {
            Ok(())
        }
        fn unload(&mut self, _ctx: &mut dyn evo_esp32_sdk::context::Context) {}
    }
    impl Respondent for Impostor {
        fn handle(
            &mut self,
            req: &Request,
            _ctx: &mut dyn evo_esp32_sdk::context::Context,
        ) -> Response {
            Response::fault(req, Fault::UnknownVerb)
        }
    }

    fn steward() -> Steward {
        Steward::new(catalogue(), Config::default()).unwrap()
    }

    #[test]
    fn admission_checks_the_catalogue_shape_for_shape() {
        let mut s = steward();
        let a = s.admit_respondent(Box::new(Echo), Ticks(0)).unwrap();
        assert_eq!(a.index, 0);
        assert!(matches!(
            s.admit_respondent(Box::new(Impostor), Ticks(0)),
            Err(AdmissionError::ShapeMismatch {
                expected: 1,
                found: 9,
                ..
            })
        ));
        assert!(matches!(
            s.admit_respondent(Box::new(Echo), Ticks(0)),
            Err(AdmissionError::DuplicateName(_))
        ));
        assert_eq!(s.plugins().count(), 1);
    }

    #[test]
    fn a_request_reaches_the_respondent_and_its_contribution_shows() {
        let mut s = steward();
        s.admit_respondent(Box::new(Echo), Ticks(0)).unwrap();
        let req = Request {
            cid: CorrelationId(3),
            shelf: ShelfRef::new("audio", "source"),
            verb: "echo".into(),
            subject: None,
            args: json!({"hello": "world"}),
        };
        let resp = s.request(&req, Ticks(5)).unwrap();
        assert_eq!(resp.outcome, Ok(json!({"hello": "world"})));
        let p = s.projection(ProjectionKey::Subject {
            id: SubjectId::new("echo/1"),
        });
        assert_eq!(p.subjects[0].state, json!({"hello": "world"}));
        assert_eq!(p.subjects[0].last_writer, "test.echo");
        let nothing = Request {
            shelf: ShelfRef::new("audio", "playback"),
            ..req
        };
        assert!(matches!(
            s.request(&nothing, Ticks(6)),
            Err(DispatchError::NoPluginFor(_))
        ));
    }

    #[test]
    fn a_warden_is_ticked_until_done_and_then_released() {
        let mut s = steward();
        s.admit_respondent(Box::new(Echo), Ticks(0)).unwrap();
        s.admit_warden(Box::new(Player { ticks_left: 0 }), Ticks(0))
            .unwrap();
        let shelf = ShelfRef::new("audio", "playback");
        let subject = SubjectId::new("echo/1");
        assert!(matches!(
            s.assign(
                &shelf,
                Assignment {
                    subject: subject.clone(),
                    instruction: json!({}),
                },
                Ticks(1)
            ),
            Err(DispatchError::Refused(_))
        ));
        let h = s
            .assign(
                &shelf,
                Assignment {
                    subject: subject.clone(),
                    instruction: json!({"ticks": 2}),
                },
                Ticks(1),
            )
            .unwrap();
        assert_eq!(
            s.subject(&subject).unwrap().state,
            json!({"status": "play"})
        );
        // Due at once; the warden asks for 10 ms between ticks.
        assert_eq!(s.tick(Ticks(1)), Some(Ticks(11)));
        assert_eq!(s.tick(Ticks(5)), Some(Ticks(11)), "not due yet");
        assert_eq!(s.tick(Ticks(11)), Some(Ticks(21)));
        assert_eq!(s.tick(Ticks(21)), None, "done and released");
        assert_eq!(s.custodies().count(), 0);
        assert!(matches!(
            s.release(h, Ticks(22)),
            Err(DispatchError::UnknownCustody(_))
        ));
        let custody_events = s
            .happenings_since(Seq(0))
            .happenings
            .into_iter()
            .filter(|h| {
                matches!(
                    h.kind,
                    HappeningKind::CustodyTaken { .. }
                        | HappeningKind::CustodyReleased { .. }
                )
            })
            .count();
        assert_eq!(custody_events, 2);
    }

    #[test]
    fn rack_projection_is_what_the_racks_plugins_touched() {
        let mut s = steward();
        s.admit_respondent(Box::new(Echo), Ticks(0)).unwrap();
        let p = s.projection(ProjectionKey::Rack {
            name: "audio".into(),
        });
        assert_eq!(p.subjects.len(), 1);
        assert_eq!(p.as_of, Seq(2), "announce, then plugin loaded");
        let other = s.projection(ProjectionKey::Rack {
            name: "nothing".into(),
        });
        assert!(other.subjects.is_empty());
    }

    #[test]
    fn shutdown_unloads_everything_and_retracts() {
        let mut s = steward();
        s.admit_respondent(Box::new(Echo), Ticks(0)).unwrap();
        s.admit_warden(Box::new(Player { ticks_left: 0 }), Ticks(0))
            .unwrap();
        s.assign(
            &ShelfRef::new("audio", "playback"),
            Assignment {
                subject: SubjectId::new("echo/1"),
                instruction: json!({"ticks": 5}),
            },
            Ticks(1),
        )
        .unwrap();
        s.shutdown(Ticks(9));
        assert_eq!(s.plugins().count(), 0);
        assert_eq!(s.custodies().count(), 0);
        assert!(s.subject(&SubjectId::new("echo/1")).is_none());
    }

    #[test]
    fn a_malformed_catalogue_is_refused_at_construction() {
        let mut c = catalogue();
        c.racks[0].kinds.clear();
        assert!(Steward::new(c, Config::default()).is_err());
    }
}

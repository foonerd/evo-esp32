// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The steward's state. Everything mutable lives here and nowhere else.
//!
//! Subjects are kept in one list: a device has tens of them, not
//! thousands, and a linear scan is cheaper than a map's allocation. Each
//! subject keeps every plugin's contribution apart, keyed by the plugin's
//! admission index, so a plugin retracting leaves the others' opinions
//! standing and the composed state is recomputed from what remains.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use evo_esp32_sdk::context::ContextError;
use evo_esp32_sdk::happening::HappeningKind;
use evo_esp32_sdk::subject::{
    ExternalAddressing, SubjectAnnouncement, SubjectId, SubjectRecord,
};
use evo_esp32_sdk::{Ticks, Value};

use crate::compose::compose;
use crate::happenings::HappeningsRing;

/// One subject as the tables hold it.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub(crate) id: SubjectId,
    pub(crate) kind: String,
    /// Every addressing that resolves to this subject, with the plugin
    /// that offered it.
    pub(crate) addressings: Vec<(usize, ExternalAddressing)>,
    /// Each contributing plugin's opinion, in admission order.
    pub(crate) contributions: Vec<(usize, Value)>,
    /// Admission index of the plugin that last wrote.
    pub(crate) last_writer: usize,
}

impl Entry {
    fn record(&self, plugin_names: &[&'static str]) -> SubjectRecord {
        SubjectRecord {
            id: self.id.clone(),
            kind: self.kind.clone(),
            state: compose(self.contributions.iter().map(|(_, v)| v)),
            last_writer: String::from(
                plugin_names.get(self.last_writer).copied().unwrap_or("?"),
            ),
        }
    }
}

/// The steward's mutable state.
#[derive(Debug)]
pub struct Tables {
    pub(crate) subjects: Vec<Entry>,
    pub(crate) ring: HappeningsRing,
    pub(crate) plugin_names: Vec<&'static str>,
    max_subjects: usize,
    minted: u64,
}

impl Tables {
    /// Tables for at most `max_subjects` subjects and a happenings ring of
    /// `happenings_capacity`.
    pub fn new(max_subjects: usize, happenings_capacity: usize) -> Self {
        Self {
            subjects: Vec::new(),
            ring: HappeningsRing::new(happenings_capacity),
            plugin_names: Vec::new(),
            max_subjects: max_subjects.max(1),
            minted: 0,
        }
    }

    /// How many subjects are held.
    pub fn subject_count(&self) -> usize {
        self.subjects.len()
    }

    pub(crate) fn find(&self, id: &SubjectId) -> Option<usize> {
        self.subjects.iter().position(|e| &e.id == id)
    }

    fn find_by_addressing(&self, a: &ExternalAddressing) -> Option<usize> {
        self.subjects
            .iter()
            .position(|e| e.addressings.iter().any(|(_, x)| x == a))
    }

    /// The record for `id`, composed.
    pub fn record(&self, id: &SubjectId) -> Option<SubjectRecord> {
        self.find(id)
            .map(|i| self.subjects[i].record(&self.plugin_names))
    }

    /// Every record, in the order subjects were first announced.
    pub fn records(&self) -> Vec<SubjectRecord> {
        self.subjects
            .iter()
            .map(|e| e.record(&self.plugin_names))
            .collect()
    }

    /// Records of subjects any of `plugins` has contributed to.
    pub(crate) fn records_touched_by(
        &self,
        plugins: &[usize],
    ) -> Vec<SubjectRecord> {
        self.subjects
            .iter()
            .filter(|e| {
                e.contributions.iter().any(|(p, _)| plugins.contains(p))
            })
            .map(|e| e.record(&self.plugin_names))
            .collect()
    }

    /// Announce on behalf of `plugin`. An addressing already held resolves
    /// to its subject and the plugin's contribution is set; otherwise a
    /// subject is minted.
    pub(crate) fn announce(
        &mut self,
        plugin: usize,
        now: Ticks,
        a: SubjectAnnouncement,
    ) -> Result<SubjectId, ContextError> {
        if let Some(i) = self.find_by_addressing(&a.addressing) {
            let e = &mut self.subjects[i];
            set_contribution(&mut e.contributions, plugin, a.state);
            e.last_writer = plugin;
            let id = e.id.clone();
            self.ring
                .push(now, Some(id.clone()), HappeningKind::Updated);
            return Ok(id);
        }
        if self.subjects.len() >= self.max_subjects {
            return Err(ContextError::Full);
        }
        self.minted += 1;
        let id = SubjectId::new(format!("{}/{}", a.kind, self.minted));
        self.subjects.push(Entry {
            id: id.clone(),
            kind: a.kind,
            addressings: alloc::vec![(plugin, a.addressing)],
            contributions: alloc::vec![(plugin, a.state)],
            last_writer: plugin,
        });
        self.ring
            .push(now, Some(id.clone()), HappeningKind::Announced);
        Ok(id)
    }

    /// Replace `plugin`'s contribution to `id`.
    pub(crate) fn update(
        &mut self,
        plugin: usize,
        now: Ticks,
        id: &SubjectId,
        state: Value,
    ) -> Result<(), ContextError> {
        let i = self
            .find(id)
            .ok_or_else(|| ContextError::UnknownSubject(id.clone()))?;
        let e = &mut self.subjects[i];
        set_contribution(&mut e.contributions, plugin, state);
        e.last_writer = plugin;
        self.ring
            .push(now, Some(id.clone()), HappeningKind::Updated);
        Ok(())
    }

    /// Withdraw `plugin`'s contribution to `id`. The subject goes when the
    /// last contribution does.
    pub(crate) fn retract(
        &mut self,
        plugin: usize,
        now: Ticks,
        id: &SubjectId,
    ) -> Result<(), ContextError> {
        let i = self
            .find(id)
            .ok_or_else(|| ContextError::UnknownSubject(id.clone()))?;
        let e = &mut self.subjects[i];
        e.contributions.retain(|(p, _)| *p != plugin);
        e.addressings.retain(|(p, _)| *p != plugin);
        if e.contributions.is_empty() {
            self.subjects.remove(i);
            self.ring
                .push(now, Some(id.clone()), HappeningKind::Retracted);
        } else {
            e.last_writer = plugin;
            self.ring
                .push(now, Some(id.clone()), HappeningKind::Updated);
        }
        Ok(())
    }

    /// Withdraw every contribution `plugin` made, as at unload.
    pub(crate) fn retract_all(&mut self, plugin: usize, now: Ticks) {
        let ids: Vec<SubjectId> = self
            .subjects
            .iter()
            .filter(|e| e.contributions.iter().any(|(p, _)| *p == plugin))
            .map(|e| e.id.clone())
            .collect();
        for id in ids {
            let _ = self.retract(plugin, now, &id);
        }
    }
}

fn set_contribution(
    contributions: &mut Vec<(usize, Value)>,
    plugin: usize,
    state: Value,
) {
    match contributions.iter_mut().find(|(p, _)| *p == plugin) {
        Some((_, v)) => *v = state,
        None => {
            contributions.push((plugin, state));
            contributions.sort_by_key(|(p, _)| *p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tables() -> Tables {
        let mut t = Tables::new(4, 16);
        t.plugin_names = alloc::vec!["source", "metadata"];
        t
    }

    fn announce(kind: &str, key: &str, state: Value) -> SubjectAnnouncement {
        SubjectAnnouncement {
            addressing: ExternalAddressing::new("path", key),
            kind: kind.into(),
            state,
        }
    }

    #[test]
    fn the_same_addressing_resolves_to_one_subject() {
        let mut t = tables();
        let a = t
            .announce(
                0,
                Ticks(1),
                announce("track", "/a.flac", json!({"title": "A"})),
            )
            .unwrap();
        let b = t
            .announce(
                1,
                Ticks(2),
                announce("track", "/a.flac", json!({"artist": "X"})),
            )
            .unwrap();
        assert_eq!(a, b);
        assert_eq!(t.subject_count(), 1);
        let r = t.record(&a).unwrap();
        assert_eq!(r.state, json!({"title": "A", "artist": "X"}));
        assert_eq!(r.last_writer, "metadata");
        assert_eq!(a.as_str(), "track/1");
    }

    #[test]
    fn retracting_one_contribution_leaves_the_others() {
        let mut t = tables();
        let id = t
            .announce(
                0,
                Ticks(1),
                announce("track", "/a", json!({"title": "A"})),
            )
            .unwrap();
        t.update(1, Ticks(2), &id, json!({"artist": "X"})).unwrap();
        t.retract(0, Ticks(3), &id).unwrap();
        assert_eq!(t.record(&id).unwrap().state, json!({"artist": "X"}));
        t.retract(1, Ticks(4), &id).unwrap();
        assert!(t.record(&id).is_none());
        let kinds: Vec<_> = t
            .ring
            .since(evo_esp32_sdk::happening::Seq(0))
            .happenings
            .into_iter()
            .map(|h| h.kind)
            .collect();
        assert_eq!(
            kinds,
            [
                HappeningKind::Announced,
                HappeningKind::Updated,
                HappeningKind::Updated,
                HappeningKind::Retracted
            ]
        );
    }

    #[test]
    fn the_tables_refuse_when_full_and_unknown_ids() {
        let mut t = Tables::new(1, 4);
        t.plugin_names = alloc::vec!["p"];
        t.announce(0, Ticks(0), announce("k", "a", Value::Null))
            .unwrap();
        assert_eq!(
            t.announce(0, Ticks(0), announce("k", "b", Value::Null)),
            Err(ContextError::Full)
        );
        let ghost = SubjectId::new("k/9");
        assert_eq!(
            t.update(0, Ticks(0), &ghost, Value::Null),
            Err(ContextError::UnknownSubject(ghost.clone()))
        );
    }

    #[test]
    fn retract_all_clears_a_plugin_out() {
        let mut t = tables();
        let a = t
            .announce(0, Ticks(0), announce("k", "a", json!(1)))
            .unwrap();
        let b = t
            .announce(0, Ticks(0), announce("k", "b", json!(2)))
            .unwrap();
        t.update(1, Ticks(0), &b, json!({"m": true})).unwrap();
        t.retract_all(0, Ticks(1));
        assert!(t.record(&a).is_none());
        assert_eq!(t.record(&b).unwrap().state, json!({"m": true}));
    }
}

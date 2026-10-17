// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The handle a plugin holds for one call.
//!
//! Borrows the tables, knows which plugin it was issued to and what time
//! it is, and implements the SDK's [`Context`] against that. It cannot
//! outlive the call, so no plugin keeps a way into the steward.

use evo_esp32_sdk::context::{Context as SdkContext, ContextError};
use evo_esp32_sdk::subject::{SubjectAnnouncement, SubjectId};
use evo_esp32_sdk::{Ticks, Value};

use crate::tables::Tables;

/// The steward's implementation of the SDK's context, for one plugin,
/// for one call.
pub struct Context<'a> {
    tables: &'a mut Tables,
    plugin: usize,
    now: Ticks,
}

impl<'a> Context<'a> {
    pub(crate) fn new(
        tables: &'a mut Tables,
        plugin: usize,
        now: Ticks,
    ) -> Self {
        Self {
            tables,
            plugin,
            now,
        }
    }
}

impl SdkContext for Context<'_> {
    fn announce(
        &mut self,
        announcement: SubjectAnnouncement,
    ) -> Result<SubjectId, ContextError> {
        self.tables.announce(self.plugin, self.now, announcement)
    }

    fn update(
        &mut self,
        id: &SubjectId,
        state: Value,
    ) -> Result<(), ContextError> {
        self.tables.update(self.plugin, self.now, id, state)
    }

    fn retract(&mut self, id: &SubjectId) -> Result<(), ContextError> {
        self.tables.retract(self.plugin, self.now, id)
    }

    fn now(&self) -> Ticks {
        self.now
    }
}

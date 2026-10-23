// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Where each thread stands on the one core.
//!
//! Three tiers under the drivers (the feeder at 24, the Wi-Fi driver
//! at 23, lwIP at 18): the threads that feed the decoder and the sink,
//! the main loop, and everything else. The main loop is touch, face and
//! the decoder's tick on the main task. Above it, a read-ahead reader,
//! a station's fetcher or a room's receiver takes the core the moment
//! its bytes arrive and gives it back once they are in the ring: the
//! producer stays above the consumer that polls for it (measured the
//! other way round on 2026-10-22: the loop above its reader starved it,
//! 75–129 KB/s and the DMA dry for 4–6 s). Below it, an open's TLS
//! handshake, a share's listing, a search for servers or a peer's
//! session gets the core only when the loop sleeps: level with the loop
//! (measured the same day) the kernel sliced the core between them and
//! a paint stage of 20 ms took 450–630 ms while a station opened; below
//! the loop (0.1.3–0.1.5) they held the loop off for their whole run.
//! The art worker is background work too and sits with them. Below
//! them (at 1, in 0.1.6) it ran only when every other task slept, and
//! under a 96 kHz FLAC that is almost never: a 35 KB cover took 150 s
//! to read off the card and a logo's TLS handshake timed out.
//!
//! Plugins say which of the two a thread is through
//! [`evo_esp32_sdk::thread`]; this module places it: the priority, the
//! stack in PSRAM (internal SRAM is the drivers' and the DMA rings'),
//! the name the kernel shows in the heartbeat's core line.

use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::sync::Mutex;

use esp_idf_svc::hal::task::thread::{MallocCap, ThreadSpawnConfiguration};
use evo_esp32_sdk::thread::{Placement, Role};

/// The main task: touch, face and the decoder's tick. ESP-IDF starts
/// it at 1; it is raised once the art worker exists.
pub const MAIN_PRIORITY: u8 = 5;
/// Threads that feed the decoder or the sink: one above the loop.
pub const FEED_PRIORITY: u8 = 6;
/// Threads that open, list, search, talk to peers and fetch art: one
/// below.
pub const BACKGROUND_PRIORITY: u8 = 4;

/// The threads' placement on this board.
pub struct Placing;

/// What every thread spawned here gets whether placed or not: the
/// stack in PSRAM, and the same for the threads it spawns in turn.
pub fn default_configuration() -> ThreadSpawnConfiguration {
    ThreadSpawnConfiguration {
        stack_alloc_caps: MallocCap::Spiram | MallocCap::Cap8bit,
        inherit: true,
        ..Default::default()
    }
}

/// The kernel keeps a pointer to the name, so each distinct name is
/// made once and kept for the life of the firmware.
fn kernel_name(name: &'static str) -> Option<&'static CStr> {
    static NAMES: Mutex<BTreeMap<&'static str, &'static CStr>> =
        Mutex::new(BTreeMap::new());
    let mut names = NAMES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(known) = names.get(name) {
        return Some(known);
    }
    let made: &'static CStr =
        Box::leak(CString::new(name).ok()?.into_boxed_c_str());
    names.insert(name, made);
    Some(made)
}

impl Placement for Placing {
    fn before(&self, role: Role, name: &'static str) -> Result<(), String> {
        let priority = match role {
            Role::Feed => FEED_PRIORITY,
            Role::Background => BACKGROUND_PRIORITY,
        };
        ThreadSpawnConfiguration {
            name: kernel_name(name),
            priority,
            ..default_configuration()
        }
        .set()
        .map_err(|e| format!("placing {name}: {e}"))
    }

    fn after(&self) {
        if let Err(e) = default_configuration().set() {
            log::warn!("threads: the default placement did not come back: {e}");
        }
    }
}

// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Where each thread stands on the one core.
//!
//! Three tiers under the drivers (the feeder at 24, the Wi-Fi driver
//! at 23, lwIP at 18): the threads that feed the decoder and the sink
//! and the decoder itself, the main loop, and everything else. The
//! main loop is touch, face and the decoder's hand-over on the main
//! task. Above it, a read-ahead reader,
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
use std::sync::atomic::AtomicBool;
use std::sync::Mutex;

use esp_idf_svc::hal::task::thread::{MallocCap, ThreadSpawnConfiguration};
use evo_esp32_sdk::thread::{Placement, Role};

/// The sink's feeder, which moves frames from the reservoir into the
/// I2S DMA ring: above the Wi-Fi driver's 23, the top the kernel allows.
pub const FEEDER_PRIORITY: u8 = 24;
/// The main task: touch, face and the decoder's hand-over. ESP-IDF
/// starts it at 1; it is raised once the art worker exists.
pub const MAIN_PRIORITY: u8 = 5;
/// The decoder: above the loop, with the readers. Audio is the
/// product; the face takes what the audio leaves. Level with the loop
/// (tried 2026-11-02) a 96 kHz FLAC at 55% of the core was starved
/// whenever the loop was busy — a list painted, a listing read — and
/// the sound broke up. What the decoder owes the face is not its
/// priority but a breath: once the sink holds a comfortable lead it
/// sleeps between blocks (the plugin's `COMFORT_MS`), so a paint goes
/// out while the rest of the lead fills.
pub const DECODE_PRIORITY: u8 = FEED_PRIORITY;
/// Threads that feed the decoder or the sink: one above the loop.
pub const FEED_PRIORITY: u8 = 6;
/// The touch reader: above the feeders. It wakes on the controller's
/// edge for a nine-byte read and sleeps again; a finger is not kept
/// waiting on a ring being filled.
pub const INPUT_PRIORITY: u8 = 7;
/// Threads that open, list, search, talk to peers and fetch art: one
/// below.
pub const BACKGROUND_PRIORITY: u8 = 4;
/// The network's join thread, below everything: the join's key
/// derivation is a few hundred milliseconds of arithmetic the decoder
/// is not to wait for.
pub const JOIN_PRIORITY: u8 = 1;

/// Raised by the loop while the panel is painted. The panel and the
/// card share SPI2, and the card's driver holds the bus for a whole
/// command: a paint stage of twenty milliseconds queued behind a
/// refill's chunks took 300 ms at a track start (2026-11-02). The card's
/// readers — the ring once its eager zone is in hand, the art — wait
/// for this to clear before each read, for a bounded time.
pub static PAINTING: AtomicBool = AtomicBool::new(false);
/// The longest a card reader here waits on [`PAINTING`].
pub const PAINTING_WAIT: std::time::Duration =
    std::time::Duration::from_millis(60);

/// Wait for the paint to finish, up to [`PAINTING_WAIT`].
pub fn let_the_panel_paint() {
    let until = std::time::Instant::now() + PAINTING_WAIT;
    while PAINTING.load(std::sync::atomic::Ordering::Acquire)
        && std::time::Instant::now() < until
    {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

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
            Role::Decode => DECODE_PRIORITY,
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

/// A thread that exits must give its memory back. Without
/// `evo_esp32_platform_esp_idf::tls` every exit kept the thread's
/// handle, 168 bytes of internal SRAM; this spawns a thread that does
/// nothing, joins it, and reads the internal heap around it. The first
/// thread pays for the runtime's thread-local keys, so the second is
/// the one measured. Noise from other tasks is a few bytes; a kept
/// handle is far more.
pub fn check_exit_returns_memory() {
    fn free() -> usize {
        // SAFETY: a plain heap query.
        unsafe {
            esp_idf_svc::sys::heap_caps_get_free_size(
                esp_idf_svc::sys::MALLOC_CAP_INTERNAL,
            )
        }
    }
    fn once() -> Option<isize> {
        let before = free();
        std::thread::Builder::new()
            .name("probe".into())
            .stack_size(4096)
            .spawn(|| {})
            .ok()?
            .join()
            .ok()?;
        // The kernel's idle task frees the task's own block after the join.
        std::thread::sleep(std::time::Duration::from_millis(20));
        Some(before as isize - free() as isize)
    }
    const KEPT: isize = 128;
    match (once(), once()) {
        (Some(_), Some(kept)) if kept < KEPT => {
            log::info!("threads: a thread that exits gives its memory back")
        }
        (Some(_), Some(kept)) => log::warn!(
            "threads: a thread that exits keeps {kept} B of internal SRAM; every track will cost that"
        ),
        _ => log::warn!("threads: the exit check could not run"),
    }
}

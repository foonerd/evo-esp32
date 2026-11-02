// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Where the decoder's time goes, for a distribution's heartbeat: the
//! microseconds in the codec (reading the next packet and decoding it,
//! which on the card includes a wait for the read-ahead ring) and in
//! the sink's `write`, and the packets decoded, since boot. A few
//! atomic adds a packet; the distribution reads and differences them.

use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::time::Instant;

/// Microseconds in `next_frames`: the demuxer, the codec, and any wait
/// on the source.
pub static DECODE_US: AtomicU32 = AtomicU32::new(0);
/// Microseconds in the sink's `write`.
pub static SINK_WRITE_US: AtomicU32 = AtomicU32::new(0);
/// Packets handed to the codec.
pub static PACKETS: AtomicU32 = AtomicU32::new(0);

pub(crate) fn add(to: &AtomicU32, since: Instant) {
    to.fetch_add(since.elapsed().as_micros() as u32, Relaxed);
}

/// The counters now, for differencing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Account {
    /// Microseconds in `next_frames`.
    pub decode_us: u32,
    /// Microseconds in the sink's `write`.
    pub sink_write_us: u32,
    /// Packets handed to the codec.
    pub packets: u32,
}

impl Account {
    /// The counters now.
    pub fn now() -> Self {
        Self {
            decode_us: DECODE_US.load(Relaxed),
            sink_write_us: SINK_WRITE_US.load(Relaxed),
            packets: PACKETS.load(Relaxed),
        }
    }

    /// What was added since `earlier`.
    pub fn since(self, earlier: Self) -> Self {
        Self {
            decode_us: self.decode_us.wrapping_sub(earlier.decode_us),
            sink_write_us: self
                .sink_write_us
                .wrapping_sub(earlier.sink_write_us),
            packets: self.packets.wrapping_sub(earlier.packets),
        }
    }
}

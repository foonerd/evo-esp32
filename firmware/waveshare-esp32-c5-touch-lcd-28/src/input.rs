// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Touch, read off the main loop.
//!
//! The controller pulls its interrupt line low for each report it has
//! ready. A thread above the loop wakes on that edge, reads the report
//! (three transactions on the I2C bus, under a millisecond) and queues
//! it with the time it was read; the loop drains the queue each turn and
//! feeds the tracker every sample in order, each at its own time. While
//! a finger is down the thread also reads on its own every
//! [`LIFT_POLL_MS`], so a lift that brings no edge is still seen.
//!
//! Before this the loop read one report a turn, when its turn came: a
//! finger was sampled every 85–120 ms under a 96 kHz track, and a tap
//! shorter than a paint stage (up to 400 ms) could fall between two
//! reads and be lost. The thread reads at the controller's own rate
//! whatever the loop is doing; what the loop still bounds is how often
//! the intents are acted on.

use std::collections::VecDeque;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cst3530::Cst3530;
use esp_idf_svc::hal::delay::{TickType, BLOCK};
use esp_idf_svc::hal::gpio::{InterruptType, PinDriver};
use esp_idf_svc::hal::task::notification::Notification;
use esp_idf_svc::hal::task::thread::{MallocCap, ThreadSpawnConfiguration};
use log::warn;

/// The controller on the board's bus.
type Controller = Cst3530<board::Device>;
/// The interrupt line.
type Int = PinDriver<'static, esp_idf_svc::hal::gpio::Input>;

/// One report, in the controller's frame, at the time it was read
/// (milliseconds since the firmware started).
#[derive(Debug, Clone, Copy)]
pub struct Touch {
    pub x: u16,
    pub y: u16,
    pub touching: bool,
    pub at_ms: u64,
}

/// How often a finger that is down is read when no edge comes.
const LIFT_POLL_MS: u64 = 30;
/// Reports held for the loop: at the controller's rate, longer than the
/// longest paint stage. When full, the newest report replaces the one
/// before it, so the first sample of a press (what the finger landed
/// on) and the latest are both kept.
const DEPTH: usize = 64;
/// The thread's stack, in PSRAM: a report, a log line, the driver.
const STACK: usize = 8 * 1024;

#[derive(Default)]
struct Queue {
    samples: Mutex<VecDeque<Touch>>,
    /// Reports that replaced an earlier one because the queue was full.
    coalesced: AtomicU32,
}

/// The reader thread's end the loop holds.
pub struct Input {
    queue: Arc<Queue>,
}

impl Input {
    /// Start the reader on `controller`, woken by `int`. Times are
    /// measured from `started`, the loop's own epoch.
    pub fn start(
        controller: Controller,
        int: Int,
        started: Instant,
    ) -> Result<Self, std::io::Error> {
        let queue = Arc::new(Queue::default());
        let before = ThreadSpawnConfiguration::get();
        ThreadSpawnConfiguration {
            name: Some(c"touch"),
            stack_size: STACK,
            priority: crate::threads::INPUT_PRIORITY,
            stack_alloc_caps: MallocCap::Spiram | MallocCap::Cap8bit,
            ..Default::default()
        }
        .set()
        .map_err(std::io::Error::other)?;
        let theirs = queue.clone();
        std::thread::Builder::new()
            .stack_size(STACK)
            .spawn(move || run(controller, int, started, &theirs))?;
        before
            .unwrap_or_default()
            .set()
            .map_err(std::io::Error::other)?;
        Ok(Self { queue })
    }

    /// Move every report read since the last call into `into`, oldest
    /// first.
    pub fn drain(&self, into: &mut Vec<Touch>) {
        let mut samples =
            self.queue.samples.lock().unwrap_or_else(|e| e.into_inner());
        into.extend(samples.drain(..));
    }

    /// Reports that replaced an earlier one since the last call: the
    /// loop was away longer than the queue holds.
    pub fn coalesced(&self) -> u32 {
        self.queue.coalesced.swap(0, Ordering::Relaxed)
    }
}

impl Queue {
    fn push(&self, t: Touch) {
        let mut samples =
            self.samples.lock().unwrap_or_else(|e| e.into_inner());
        if samples.len() >= DEPTH {
            samples.pop_back();
            self.coalesced.fetch_add(1, Ordering::Relaxed);
        }
        samples.push_back(t);
    }
}

fn run(
    mut controller: Controller,
    mut int: Int,
    started: Instant,
    queue: &Queue,
) {
    let notification = Notification::new();
    let notifier = notification.notifier();
    if let Err(e) = int.set_interrupt_type(InterruptType::NegEdge) {
        warn!("touch: no interrupt on INT: {e}; touch is off");
        return;
    }
    // SAFETY: the callback does nothing but notify this task, which the
    // HAL allows from an ISR; the pin driver, which owns the
    // subscription, lives as long as this thread.
    let subscribed = unsafe {
        int.subscribe(move || {
            notifier.notify_and_yield(NonZeroU32::new(1).unwrap());
        })
    };
    if let Err(e) = subscribed {
        warn!("touch: no interrupt on INT: {e}; touch is off");
        return;
    }
    let poll = TickType::from(Duration::from_millis(LIFT_POLL_MS)).0;
    let mut down = false;
    let mut errors = 0u32;
    loop {
        // The driver disarms the interrupt as it fires; armed again
        // before each wait, so an edge during the read is not lost.
        if let Err(e) = int.enable_interrupt() {
            warn!("touch: the interrupt would not arm: {e}; touch is off");
            return;
        }
        let woke = notification.wait(if down { poll } else { BLOCK });
        if woke.is_none() && !down {
            continue;
        }
        match controller.report() {
            Ok(r) => {
                down = r.touching();
                queue.push(Touch {
                    x: r.x,
                    y: r.y,
                    touching: down,
                    at_ms: started.elapsed().as_millis() as u64,
                });
                errors = 0;
            }
            Err(e) => {
                // A bus error mid-press: the finger is read again on the
                // next edge or poll; a run of them is said once.
                errors += 1;
                if errors == 3 {
                    warn!("touch: the controller is not answering: {e:?}");
                }
            }
        }
    }
}

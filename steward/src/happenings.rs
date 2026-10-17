// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The bounded happenings stream.
//!
//! A ring of fixed capacity. When it is full the oldest happening is
//! dropped and the ring remembers how far it has dropped, so a consumer
//! replaying from a position it held learns whether it missed anything.
//! No allocation after construction; the ring is sized once by the
//! distribution.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use evo_esp32_sdk::happening::{Happening, HappeningKind, Seq};
use evo_esp32_sdk::subject::SubjectId;
use evo_esp32_sdk::Ticks;

/// What a consumer gets back when it asks for everything after a position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replay {
    /// The happenings after the position, oldest first.
    pub happenings: Vec<Happening>,
    /// True when some happenings between the position and the first one
    /// returned were dropped from the ring. The consumer should re-read
    /// the projection it cares about.
    pub gap: bool,
}

/// The stream.
#[derive(Debug)]
pub struct HappeningsRing {
    ring: VecDeque<Happening>,
    capacity: usize,
    next: Seq,
}

impl HappeningsRing {
    /// A ring holding at most `capacity` happenings. At least one.
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            ring: VecDeque::with_capacity(capacity),
            capacity,
            next: Seq(1),
        }
    }

    /// The position the next happening will take.
    pub fn next_seq(&self) -> Seq {
        self.next
    }

    /// The position of the newest happening on the stream, if any.
    pub fn latest(&self) -> Option<Seq> {
        self.ring.back().map(|h| h.seq)
    }

    /// How many happenings the ring holds.
    pub fn len(&self) -> usize {
        self.ring.len()
    }

    /// Whether the ring holds nothing.
    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }

    /// Record a happening, dropping the oldest if the ring is full.
    pub fn push(
        &mut self,
        at: Ticks,
        subject: Option<SubjectId>,
        kind: HappeningKind,
    ) -> Seq {
        let seq = self.next;
        self.next = Seq(seq.0 + 1);
        if self.ring.len() == self.capacity {
            self.ring.pop_front();
        }
        self.ring.push_back(Happening {
            seq,
            at,
            subject,
            kind,
        });
        seq
    }

    /// Everything after `since`. `Seq(0)` means from the beginning.
    pub fn since(&self, since: Seq) -> Replay {
        let oldest = self.ring.front().map(|h| h.seq);
        let gap = match oldest {
            // The consumer's position is older than anything held, and
            // something was emitted since: at least one was dropped.
            Some(oldest) => since.0 + 1 < oldest.0,
            None => since.0 + 1 < self.next.0,
        };
        let happenings = self
            .ring
            .iter()
            .filter(|h| h.seq > since)
            .cloned()
            .collect();
        Replay { happenings, gap }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill(ring: &mut HappeningsRing, n: u64) {
        for i in 0..n {
            ring.push(Ticks(i), None, HappeningKind::Updated);
        }
    }

    #[test]
    fn sequences_are_strictly_increasing_and_never_reused() {
        let mut r = HappeningsRing::new(2);
        assert_eq!(r.push(Ticks(0), None, HappeningKind::Announced), Seq(1));
        assert_eq!(r.push(Ticks(0), None, HappeningKind::Announced), Seq(2));
        assert_eq!(r.push(Ticks(0), None, HappeningKind::Announced), Seq(3));
        assert_eq!(r.len(), 2);
        assert_eq!(r.latest(), Some(Seq(3)));
        assert_eq!(r.next_seq(), Seq(4));
    }

    #[test]
    fn replay_from_a_held_position_has_no_gap() {
        let mut r = HappeningsRing::new(8);
        fill(&mut r, 5);
        let rp = r.since(Seq(3));
        assert!(!rp.gap);
        assert_eq!(
            rp.happenings.iter().map(|h| h.seq.0).collect::<Vec<_>>(),
            [4, 5]
        );
    }

    #[test]
    fn replay_from_a_dropped_position_reports_the_gap() {
        let mut r = HappeningsRing::new(3);
        fill(&mut r, 6);
        // Holds 4, 5, 6. A consumer at 2 missed 3.
        let rp = r.since(Seq(2));
        assert!(rp.gap);
        assert_eq!(rp.happenings.len(), 3);
        // A consumer at 3 missed nothing: 4 is the oldest held.
        assert!(!r.since(Seq(3)).gap);
    }

    #[test]
    fn empty_ring_from_the_beginning_is_not_a_gap() {
        let r = HappeningsRing::new(4);
        let rp = r.since(Seq(0));
        assert!(!rp.gap);
        assert!(rp.happenings.is_empty());
        assert!(r.is_empty());
    }
}

// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! What the player plays next.
//!
//! A distribution's loop owns a [`Queue`] of items (tracks, by whatever
//! the distribution keeps for one) and asks it what comes next. The
//! queue holds its items in the order they were given and, over that,
//! a play order: the same order, or a shuffle of it. Shuffling keeps the
//! item in hand where it is and deals the rest; turning shuffle off puts
//! the hand back where the item sits in the given order, so the listener
//! never loses their place. At the end, [`Repeat::All`] starts again (a
//! fresh deal when shuffled), [`Repeat::One`] plays the same item again,
//! [`Repeat::Off`] stops. Previous and next move through the play order;
//! at the edges they wrap under `All` and stop otherwise.
//!
//! The randomness is the caller's: [`Queue::new`] takes a seed and the
//! queue runs a small generator of its own, so a bench run is repeatable
//! from the seed it logs and the crate has no platform in it.

#![no_std]

extern crate alloc;

use alloc::vec::Vec;

/// What happens at the end of the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Repeat {
    /// It stops.
    #[default]
    Off,
    /// It starts again.
    All,
    /// The item in hand plays again.
    One,
}

/// The queue: items, a play order over them, and the hand.
#[derive(Debug, Clone)]
pub struct Queue<T> {
    items: Vec<T>,
    /// Indices into `items` in the order they play.
    order: Vec<usize>,
    /// Where in `order` the item in hand is; none before the first
    /// `next` and after the end.
    at: Option<usize>,
    shuffle: bool,
    repeat: Repeat,
    rng: u64,
}

impl<T> Queue<T> {
    /// An empty queue in the given order, seeded for its shuffles.
    pub fn new(seed: u64) -> Self {
        Self {
            items: Vec::new(),
            order: Vec::new(),
            at: None,
            shuffle: false,
            repeat: Repeat::Off,
            rng: seed | 1,
        }
    }

    /// Replace the items. The hand is before the first: with
    /// `start_at` the item at that index comes first (then the rest in
    /// order, or shuffled after it); without, the first in order (or a
    /// full deal).
    pub fn set(&mut self, items: Vec<T>, start_at: Option<usize>) {
        self.items = items;
        self.order = (0..self.items.len()).collect();
        self.at = None;
        match start_at {
            Some(i) if i < self.items.len() => {
                // The chosen one first, the rest as the mode says.
                self.order.rotate_left(i);
                if self.shuffle {
                    self.deal(1);
                }
            }
            _ => {
                if self.shuffle {
                    self.deal(0);
                }
            }
        }
    }

    /// The items, in the order they were given.
    pub fn items(&self) -> &[T] {
        &self.items
    }

    /// How many items.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// No items.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Whether the play order is shuffled.
    pub fn shuffle(&self) -> bool {
        self.shuffle
    }

    /// What happens at the end.
    pub fn repeat(&self) -> Repeat {
        self.repeat
    }

    /// Set the mode. Turning shuffle on deals the items after the hand;
    /// turning it off restores the given order with the hand on the
    /// same item.
    pub fn set_shuffle(&mut self, on: bool) {
        if on == self.shuffle {
            return;
        }
        self.shuffle = on;
        let current = self.at.map(|a| self.order[a]);
        if on {
            match self.at {
                Some(a) => self.deal(a + 1),
                None => self.deal(0),
            }
        } else {
            self.order = (0..self.items.len()).collect();
            self.at = current;
        }
    }

    /// Set what happens at the end.
    pub fn set_repeat(&mut self, repeat: Repeat) {
        self.repeat = repeat;
    }

    /// The item in hand.
    pub fn current(&self) -> Option<&T> {
        self.at.map(|a| &self.items[self.order[a]])
    }

    /// The index, in the given order, of the item in hand.
    pub fn current_index(&self) -> Option<usize> {
        self.at.map(|a| self.order[a])
    }

    /// Move the hand to the next item and return it; none at the end
    /// under [`Repeat::Off`]. Under [`Repeat::One`] the hand stays.
    /// `on_its_own` says the move was not asked for by the listener (a
    /// track ended): then `One` repeats, where a listener's next always
    /// moves on.
    pub fn next(&mut self, on_its_own: bool) -> Option<&T> {
        if self.items.is_empty() {
            return None;
        }
        if on_its_own && self.repeat == Repeat::One && self.at.is_some() {
            return self.current();
        }
        let following = match self.at {
            None => 0,
            Some(a) => a + 1,
        };
        if following < self.order.len() {
            self.at = Some(following);
            return self.current();
        }
        match self.repeat {
            Repeat::Off => {
                self.at = None;
                None
            }
            Repeat::All | Repeat::One => {
                if self.shuffle {
                    self.deal(0);
                }
                self.at = Some(0);
                self.current()
            }
        }
    }

    /// Move the hand to the previous item and return it; at the start
    /// it stays on the first, or wraps to the last under
    /// [`Repeat::All`].
    pub fn previous(&mut self) -> Option<&T> {
        if self.items.is_empty() {
            return None;
        }
        self.at = Some(match self.at {
            Some(a) if a > 0 => a - 1,
            Some(_) | None => {
                if self.repeat == Repeat::All {
                    self.order.len() - 1
                } else {
                    0
                }
            }
        });
        self.current()
    }

    /// Put the hand back before the first item: the next `next` starts
    /// the queue again.
    pub fn rewind(&mut self) {
        self.at = None;
    }

    /// Shuffle `order[from..]` in place: Fisher-Yates over the tail.
    fn deal(&mut self, from: usize) {
        let n = self.order.len();
        if from >= n {
            return;
        }
        for i in (from + 1..n).rev() {
            let span = i - from + 1;
            let r = from + (self.random() % span as u64) as usize;
            self.order.swap(i, r);
        }
    }

    /// xorshift64*: small, good enough to deal a card's worth of tracks.
    fn random(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn make(n: usize, shuffle: bool, repeat: Repeat) -> Queue<usize> {
        let mut q = Queue::new(7);
        q.set_shuffle(shuffle);
        q.set_repeat(repeat);
        q.set((0..n).collect(), None);
        q
    }

    fn run(q: &mut Queue<usize>, steps: usize) -> Vec<usize> {
        let mut out = Vec::new();
        for _ in 0..steps {
            match q.next(true) {
                Some(&i) => out.push(i),
                None => break,
            }
        }
        out
    }

    #[test]
    fn in_order_once_then_stops() {
        let mut q = make(3, false, Repeat::Off);
        assert_eq!(run(&mut q, 10), [0, 1, 2]);
        assert_eq!(q.current(), None, "past the end");
        assert_eq!(
            q.next(false),
            Some(&0),
            "a next after the end starts again"
        );
    }

    #[test]
    fn repeat_all_wraps_and_repeat_one_holds() {
        let mut q = make(3, false, Repeat::All);
        assert_eq!(run(&mut q, 7), [0, 1, 2, 0, 1, 2, 0]);
        let mut q = make(3, false, Repeat::One);
        assert_eq!(q.next(false), Some(&0));
        assert_eq!(q.next(true), Some(&0), "ended on its own: the same again");
        assert_eq!(q.next(false), Some(&1), "the listener's next moves on");
    }

    #[test]
    fn a_shuffle_is_a_permutation_and_a_fresh_deal_each_lap() {
        let mut q = make(8, true, Repeat::All);
        let first: Vec<usize> = run(&mut q, 8);
        let mut sorted = first.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..8).collect::<Vec<_>>());
        let second: Vec<usize> = run(&mut q, 8);
        assert_ne!(first, second, "seed 7 deals differently the second time");
        let same: Vec<usize> = run(&mut make(8, true, Repeat::All), 8);
        assert_eq!(first, same, "the same seed deals the same");
    }

    #[test]
    fn a_tap_makes_the_folder_the_queue_from_that_track() {
        let mut q = make(5, false, Repeat::Off);
        q.set(vec![10, 11, 12, 13, 14], Some(3));
        assert_eq!(run(&mut q, 10), [13, 14, 10, 11, 12], "round from the tap");
        let mut q = make(5, true, Repeat::Off);
        q.set(vec![10, 11, 12, 13, 14], Some(3));
        let order = run(&mut q, 10);
        assert_eq!(order[0], 13, "the tapped one first, then a deal");
        assert_eq!(order.len(), 5);
    }

    #[test]
    fn shuffle_toggled_keeps_the_place() {
        let mut q = make(6, false, Repeat::All);
        assert_eq!(run(&mut q, 3), [0, 1, 2]);
        q.set_shuffle(true);
        assert_eq!(q.current(), Some(&2), "still in hand");
        let rest: Vec<usize> = run(&mut q, 3);
        let mut sorted = rest.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, [3, 4, 5], "the unplayed ones, dealt");
        q.set_shuffle(false);
        assert_eq!(q.current_index(), Some(*rest.last().unwrap()));
        let after = q.next(true).copied();
        assert_eq!(after, Some((rest.last().unwrap() + 1) % 6), "on in order");
    }

    #[test]
    fn previous_steps_back_and_wraps_only_under_all() {
        let mut q = make(3, false, Repeat::Off);
        assert_eq!(q.previous(), Some(&0), "at the start it stays");
        assert_eq!(run(&mut q, 2), [1, 2]);
        assert_eq!(q.previous(), Some(&1));
        assert_eq!(q.previous(), Some(&0));
        assert_eq!(q.previous(), Some(&0), "and stays");
        let mut q = make(3, false, Repeat::All);
        assert_eq!(q.previous(), Some(&2), "wraps to the last");
    }

    #[test]
    fn empty_is_quiet() {
        let mut q: Queue<usize> = Queue::new(1);
        assert_eq!(q.next(true), None);
        assert_eq!(q.previous(), None);
        q.set_shuffle(true);
        assert!(q.is_empty());
    }
}

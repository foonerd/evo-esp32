// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Messages between threads that stand at different priorities.
//!
//! `std::sync::mpsc` hands a message over in two steps: the sender
//! claims the next slot, then fills it. A receiver that reads between
//! the two sees a message that is not there yet and spins for it,
//! yielding the core each turn. Under a priority scheduler on one core
//! a yield from the highest-priority ready task comes straight back to
//! it, so a receiver that outranks its sender spins for good: the sender
//! never runs again, the slot is never filled, and the task watchdog is
//! what ends it. That is how the main task hung on 2026-11-01: the
//! sink's feeder, at the top priority, in `try_recv`; the main task one
//! instruction short of filling the slot it had claimed.
//!
//! A mailbox keeps its messages under a mutex, which the kernel lends
//! the holder the waiter's priority for, so a hand-over always
//! completes; a receiver that finds the box empty waits on a condition
//! variable rather than spin. The API is the channel's — [`Sender`],
//! [`Receiver`], `send`, `try_recv`, `recv`, `recv_timeout` — so a
//! channel becomes a mailbox by its import; every thread hand-over in
//! the project is one.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// A mailbox: any number of senders, one receiver, no limit on what
/// waits inside.
pub fn mailbox<T>() -> (Sender<T>, Receiver<T>) {
    with_room(usize::MAX)
}

/// A mailbox that holds at most `room` messages: a `send` into a full
/// one waits until the receiver takes one. Back-pressure for a producer
/// that must not run ahead of its consumer.
pub fn bounded<T>(room: usize) -> (Sender<T>, Receiver<T>) {
    with_room(room.max(1))
}

fn with_room<T>(room: usize) -> (Sender<T>, Receiver<T>) {
    let shared = Arc::new(Shared {
        held: Mutex::new(VecDeque::new()),
        posted: Condvar::new(),
        taken: Condvar::new(),
        room,
        senders: AtomicUsize::new(1),
        open: AtomicBool::new(true),
    });
    (Sender(shared.clone()), Receiver(shared))
}

struct Shared<T> {
    held: Mutex<VecDeque<T>>,
    /// Signalled on every send and when the last sender goes.
    posted: Condvar,
    /// Signalled on every receive and when the receiver goes.
    taken: Condvar,
    room: usize,
    senders: AtomicUsize,
    /// Cleared when the receiver is dropped.
    open: AtomicBool,
}

impl<T> Shared<T> {
    fn held(&self) -> MutexGuard<'_, VecDeque<T>> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The sending half; clone it for every thread that sends.
pub struct Sender<T>(Arc<Shared<T>>);

impl<T> Sender<T> {
    /// Post `message`; back as `Err` when the receiver is gone. Into a
    /// full bounded mailbox, waits for room.
    pub fn send(&self, message: T) -> Result<(), SendError<T>> {
        let mut held = self.0.held();
        while held.len() >= self.0.room {
            if !self.0.open.load(Ordering::Acquire) {
                return Err(SendError(message));
            }
            held = self
                .0
                .taken
                .wait(held)
                .unwrap_or_else(PoisonError::into_inner);
        }
        if !self.0.open.load(Ordering::Acquire) {
            return Err(SendError(message));
        }
        held.push_back(message);
        // Notified with the lock held, so a receiver between its check
        // and its wait cannot miss this.
        self.0.posted.notify_one();
        Ok(())
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.0.senders.fetch_add(1, Ordering::Relaxed);
        Sender(self.0.clone())
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        if self.0.senders.fetch_sub(1, Ordering::AcqRel) == 1 {
            // The last one: a receiver waiting for more is told.
            let _held = self.0.held();
            self.0.posted.notify_all();
        }
    }
}

/// The receiving half.
pub struct Receiver<T>(Arc<Shared<T>>);

impl<T> Receiver<T> {
    /// The oldest message waiting, without waiting for one.
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        let mut held = self.0.held();
        match held.pop_front() {
            Some(message) => {
                self.0.taken.notify_one();
                Ok(message)
            }
            None if self.0.senders.load(Ordering::Acquire) == 0 => {
                Err(TryRecvError::Disconnected)
            }
            None => Err(TryRecvError::Empty),
        }
    }

    /// The oldest message, waiting for one as long as a sender lives.
    pub fn recv(&self) -> Result<T, RecvError> {
        let mut held = self.0.held();
        loop {
            if let Some(message) = held.pop_front() {
                self.0.taken.notify_one();
                return Ok(message);
            }
            if self.0.senders.load(Ordering::Acquire) == 0 {
                return Err(RecvError);
            }
            held = self
                .0
                .posted
                .wait(held)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// The oldest message, waiting up to `wait` for one.
    pub fn recv_timeout(&self, wait: Duration) -> Result<T, RecvTimeoutError> {
        let deadline = Instant::now() + wait;
        let mut held = self.0.held();
        loop {
            if let Some(message) = held.pop_front() {
                self.0.taken.notify_one();
                return Ok(message);
            }
            if self.0.senders.load(Ordering::Acquire) == 0 {
                return Err(RecvTimeoutError::Disconnected);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(RecvTimeoutError::Timeout);
            }
            held = self
                .0
                .posted
                .wait_timeout(held, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        // Under the lock, so a sender between its check and its wait for
        // room cannot miss this.
        let _held = self.0.held();
        self.0.open.store(false, Ordering::Release);
        self.0.taken.notify_all();
    }
}

/// The receiver is gone; the message comes back.
#[derive(Debug, PartialEq, Eq)]
pub struct SendError<T>(pub T);

/// Why `try_recv` had nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TryRecvError {
    /// Nothing waiting; senders live.
    Empty,
    /// Nothing waiting and no sender left.
    Disconnected,
}

/// Every sender is gone and the box is empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecvError;

/// Why `recv_timeout` had nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvTimeoutError {
    /// The wait ran out; senders live.
    Timeout,
    /// No sender left.
    Disconnected,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn messages_come_out_in_order() {
        let (tx, rx) = mailbox();
        tx.send(1).unwrap();
        tx.send(2).unwrap();
        assert_eq!(rx.try_recv(), Ok(1));
        assert_eq!(rx.try_recv(), Ok(2));
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn an_empty_box_tells_live_senders_from_none() {
        let (tx, rx) = mailbox::<u8>();
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
        let more = tx.clone();
        drop(tx);
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
        drop(more);
        assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));
        assert_eq!(rx.recv(), Err(RecvError));
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(10)),
            Err(RecvTimeoutError::Disconnected)
        );
    }

    #[test]
    fn a_send_after_the_receiver_is_gone_comes_back() {
        let (tx, rx) = mailbox();
        drop(rx);
        assert_eq!(tx.send(7), Err(SendError(7)));
    }

    #[test]
    fn recv_waits_for_a_send_from_another_thread() {
        let (tx, rx) = mailbox();
        let sender = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            tx.send("late").unwrap();
        });
        assert_eq!(rx.recv(), Ok("late"));
        sender.join().unwrap();
    }

    #[test]
    fn recv_wakes_when_the_last_sender_goes() {
        let (tx, rx) = mailbox::<u8>();
        let sender = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            drop(tx);
        });
        assert_eq!(rx.recv(), Err(RecvError));
        sender.join().unwrap();
    }

    #[test]
    fn recv_timeout_runs_out_and_then_hears() {
        let (tx, rx) = mailbox();
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(10)),
            Err(RecvTimeoutError::Timeout)
        );
        let sender = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            tx.send(3).unwrap();
        });
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Ok(3));
        sender.join().unwrap();
    }

    #[test]
    fn many_senders_one_receiver() {
        let (tx, rx) = mailbox();
        let senders: Vec<_> = (0..4)
            .map(|i| {
                let tx = tx.clone();
                thread::spawn(move || {
                    for j in 0..100 {
                        tx.send(i * 100 + j).unwrap();
                    }
                })
            })
            .collect();
        drop(tx);
        let mut heard = Vec::new();
        while let Ok(n) = rx.recv() {
            heard.push(n);
        }
        for s in senders {
            s.join().unwrap();
        }
        heard.sort_unstable();
        assert_eq!(heard, (0..400).collect::<Vec<_>>());
    }

    #[test]
    fn a_bounded_box_holds_the_sender_back() {
        let (tx, rx) = bounded(2);
        let sent = Arc::new(AtomicUsize::new(0));
        let counted = sent.clone();
        let sender = thread::spawn(move || {
            for i in 0..6 {
                tx.send(i).unwrap();
                counted.fetch_add(1, Ordering::SeqCst);
            }
        });
        thread::sleep(Duration::from_millis(30));
        // Two inside, the third send waiting for room.
        assert_eq!(sent.load(Ordering::SeqCst), 2);
        let mut heard = Vec::new();
        while let Ok(n) = rx.recv() {
            heard.push(n);
        }
        assert_eq!(heard, (0..6).collect::<Vec<_>>());
        sender.join().unwrap();
    }

    #[test]
    fn a_sender_waiting_for_room_is_released_when_the_receiver_goes() {
        let (tx, rx) = bounded(1);
        tx.send(1).unwrap();
        let sender = thread::spawn(move || tx.send(2));
        thread::sleep(Duration::from_millis(20));
        drop(rx);
        assert_eq!(sender.join().unwrap(), Err(SendError(2)));
    }
}

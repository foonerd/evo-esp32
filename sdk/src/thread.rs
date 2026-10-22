// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Threads a plugin starts, by what they are for.
//!
//! A plugin that reads a file ahead of the decoder, fetches a station
//! or receives a room's audio starts a thread; so does one that opens a
//! track, looks for servers on the network or talks to peers. On a
//! single core they all stand somewhere against the distribution's own
//! loop, and the plugin cannot know where: a thread that feeds the sink
//! must have the core the moment its bytes arrive, where one that opens,
//! lists or answers peers must not take it from the face in front of
//! the listener. The plugin says which of the two a thread is; the
//! distribution, which knows its kernel, places it — a priority, the
//! memory its stack lives in, the name the kernel shows — through a
//! [`Placement`] it installs once with [`place`]. Without one (the
//! host, the tests) a thread is a plain `std` thread with its name.

use std::io;
use std::sync::OnceLock;
use std::thread::{Builder, JoinHandle};

/// What a thread is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// It feeds the decoder or the sink: a read-ahead reader, a
    /// station's fetcher, a room's receiver. It must not wait on
    /// anything that is not audio.
    Feed,
    /// It works in the background: an open, a listing, a search for
    /// servers, a session with a peer. The face comes first.
    Background,
}

/// How a distribution places a thread of a [`Role`].
pub trait Placement: Send + Sync {
    /// Called on the thread about to start another, just before it
    /// does: whatever the kernel needs to know about the next thread.
    fn before(&self, role: Role, name: &'static str) -> Result<(), String>;
    /// Called on the same thread once the other has started, or failed
    /// to: the calling thread's own arrangement back.
    fn after(&self);
}

static PLACEMENT: OnceLock<&'static dyn Placement> = OnceLock::new();

/// Install the distribution's placement. Once; a second call is
/// refused and says so.
pub fn place(placement: &'static dyn Placement) -> Result<(), String> {
    PLACEMENT
        .set(placement)
        .map_err(|_| "threads are already placed".to_string())
}

/// Start a thread of `role` named `name` with a stack of `stack` bytes,
/// placed by the distribution when it has said how.
pub fn spawn<F, T>(
    role: Role,
    name: &'static str,
    stack: usize,
    f: F,
) -> io::Result<JoinHandle<T>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let placement = PLACEMENT.get().copied();
    if let Some(p) = placement {
        p.before(role, name).map_err(io::Error::other)?;
    }
    let spawned = Builder::new().name(name.into()).stack_size(stack).spawn(f);
    if let Some(p) = placement {
        p.after();
    }
    spawned
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_thread_runs_and_carries_its_name() {
        let handle = spawn(Role::Background, "named", 64 * 1024, || {
            std::thread::current().name().map(str::to_string)
        })
        .unwrap();
        assert_eq!(handle.join().unwrap().as_deref(), Some("named"));
    }
}

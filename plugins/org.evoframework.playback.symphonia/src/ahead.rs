// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! A file read ahead of the decoder, on a thread of its own.
//!
//! A card on a serial bus hands its blocks over as DMA transfers the
//! calling task waits out, so a decoder that reads the file itself
//! stops decoding while the card answers (measured on the ESP32-C5: a
//! 96 kHz FLAC decoder spent 6% of the core waiting on the card and ran
//! dry for it). Here a reader thread fills a ring from the file and
//! does the waiting; the decoder takes bytes from the ring and waits
//! only when the ring is empty. A seek within what the ring holds is
//! taken from the ring; a seek beyond it empties the ring and moves the
//! reader. The ring is one allocation, large, so a distribution that
//! places by size keeps it out of its scarce memory.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use symphonia::core::io::MediaSource;

/// What the ring holds at most: 256 KB, about a second of a 96 kHz
/// FLAC, twenty of an MP3. The ring is one allocation, large, so a
/// device that places by size puts it in its bulk memory.
const CAPACITY: usize = 256 * 1024;
/// What the reader asks the file for at a time: sixteen of the card's
/// blocks in one command. Small enough that a device which places its
/// allocations by size keeps the buffer in memory the card's DMA can
/// reach; a buffer it cannot reach is read a block at a time through a
/// bounce buffer, which on the ESP32-C5 cost a third of the core.
const CHUNK: usize = 8 * 1024;
/// The card's block: reads are asked for in whole blocks from a whole
/// block, which the file system passes straight to the card.
const BLOCK: usize = 512;
/// The reader's stack: the file system and the card's driver.
const STACK: usize = 12 * 1024;

struct State {
    /// Bytes read from the file and not yet taken, in file order.
    ring: VecDeque<u8>,
    /// The file offset of the ring's first byte.
    base: u64,
    /// The reader reached the end of the file (at `base + ring.len()`).
    eof: bool,
    /// The reader could not read; the decoder gets this.
    error: Option<String>,
    /// The reader is to move the file here and start the ring afresh.
    seek_to: Option<u64>,
    /// Counts seeks, so a read begun before one is discarded after it.
    generation: u32,
    /// The decoder is gone; the reader stops.
    stop: bool,
}

struct Shared {
    state: Mutex<State>,
    /// Signalled when the ring gains bytes, ends, or fails.
    filled: Condvar,
    /// Signalled when the ring has room, a seek is wanted, or on stop.
    wanted: Condvar,
}

/// A file as a [`MediaSource`], read ahead by a thread.
pub struct ReadAhead {
    shared: Arc<Shared>,
    /// The file's length.
    len: u64,
    /// The decoder's position in the file.
    pos: u64,
}

impl ReadAhead {
    /// Start reading `file` from its start; `len` is the file's length.
    pub fn new(mut file: File, len: u64) -> std::io::Result<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                ring: VecDeque::with_capacity(CAPACITY),
                base: 0,
                eof: false,
                error: None,
                seek_to: None,
                generation: 0,
                stop: false,
            }),
            filled: Condvar::new(),
            wanted: Condvar::new(),
        });
        let reader = Arc::clone(&shared);
        thread::Builder::new()
            .name("read".into())
            .stack_size(STACK)
            .spawn(move || read_ahead(&mut file, &reader))?;
        Ok(Self {
            shared,
            len,
            pos: 0,
        })
    }
}

/// The reader: fill the ring from the file while there is room, wait
/// when there is none, move when asked, stop when told.
fn read_ahead(file: &mut File, shared: &Shared) {
    // The buffer is taken at a four-byte boundary within a slightly
    // larger allocation: the card's driver wants that alignment for a
    // direct transfer, and a byte vector promises none.
    let mut raw = vec![0u8; CHUNK + 4];
    let skew = raw.as_ptr().align_offset(4);
    let buf = &mut raw[skew..skew + CHUNK];
    loop {
        let (generation, want) = {
            let mut s = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if s.stop {
                    return;
                }
                if let Some(to) = s.seek_to.take() {
                    s.ring.clear();
                    s.base = to;
                    s.eof = false;
                    s.error = None;
                    if let Err(e) = file.seek(SeekFrom::Start(to)) {
                        s.error = Some(format!("seek: {e}"));
                        shared.filled.notify_all();
                    }
                }
                let room = CAPACITY.saturating_sub(s.ring.len());
                if s.eof || s.error.is_some() || room < BLOCK {
                    s = shared
                        .wanted
                        .wait(s)
                        .unwrap_or_else(|e| e.into_inner());
                    continue;
                }
                break (s.generation, room.min(CHUNK) / BLOCK * BLOCK);
            }
        };
        // The card is waited on with the lock released: the decoder
        // keeps taking from the ring meanwhile.
        let got = file.read(&mut buf[..want]);
        let mut s = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if s.generation != generation {
            // A seek came while the card answered: this was the old
            // place in the file.
            continue;
        }
        match got {
            Ok(0) => s.eof = true,
            Ok(n) => s.ring.extend(&buf[..n]),
            Err(e) => s.error = Some(format!("read: {e}")),
        }
        shared.filled.notify_all();
    }
}

impl Read for ReadAhead {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut s = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if !s.ring.is_empty() {
                let n = buf.len().min(s.ring.len());
                let (front, back) = s.ring.as_slices();
                let from_front = n.min(front.len());
                buf[..from_front].copy_from_slice(&front[..from_front]);
                buf[from_front..n].copy_from_slice(&back[..n - from_front]);
                s.ring.drain(..n);
                s.base += n as u64;
                self.pos += n as u64;
                self.shared.wanted.notify_all();
                return Ok(n);
            }
            if let Some(e) = &s.error {
                return Err(std::io::Error::other(e.clone()));
            }
            if s.eof {
                return Ok(0);
            }
            s = self
                .shared
                .filled
                .wait(s)
                .unwrap_or_else(|e| e.into_inner());
        }
    }
}

impl Seek for ReadAhead {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(p) => p,
            SeekFrom::End(d) => self.len.saturating_add_signed(d),
            SeekFrom::Current(d) => self.pos.saturating_add_signed(d),
        };
        let mut s = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let held = s.base..s.base + s.ring.len() as u64;
        if held.contains(&target) || (target == held.end && s.eof) {
            // Within the ring: drop what lies before the target.
            let skip = (target - s.base) as usize;
            s.ring.drain(..skip);
            s.base = target;
        } else {
            // The reader may be mid-read for the old place: the
            // generation moves now, so what it brings back is dropped.
            s.seek_to = Some(target);
            s.ring.clear();
            s.base = target;
            s.eof = false;
            s.error = None;
            s.generation = s.generation.wrapping_add(1);
        }
        self.pos = target;
        self.shared.wanted.notify_all();
        Ok(target)
    }
}

impl MediaSource for ReadAhead {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.len)
    }
}

impl Drop for ReadAhead {
    fn drop(&mut self) {
        if let Ok(mut s) = self.shared.state.lock() {
            s.stop = true;
        }
        self.shared.wanted.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn file_of(bytes: &[u8]) -> (File, u64, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "evo-ahead-{}-{}",
            std::process::id(),
            bytes.len()
        ));
        std::fs::write(&path, bytes).unwrap();
        let mut f = File::options().read(true).open(&path).unwrap();
        f.flush().unwrap();
        (f, bytes.len() as u64, path)
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn the_whole_file_comes_through_in_order() {
        let bytes = pattern(3 * CAPACITY + 777);
        let (f, len, path) = file_of(&bytes);
        let mut ahead = ReadAhead::new(f, len).unwrap();
        let mut out = Vec::new();
        ahead.read_to_end(&mut out).unwrap();
        assert_eq!(out, bytes);
        assert_eq!(ahead.byte_len(), Some(len));
        drop(ahead);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_seek_within_the_ring_and_one_beyond_it_both_land() {
        let bytes = pattern(CAPACITY + 10_000);
        let (f, len, path) = file_of(&bytes);
        let mut ahead = ReadAhead::new(f, len).unwrap();
        let mut head = [0u8; 1000];
        ahead.read_exact(&mut head).unwrap();
        assert_eq!(&head[..], &bytes[..1000]);
        // Within the ring (the reader has at least a chunk by now).
        let mut at = [0u8; 100];
        assert_eq!(ahead.seek(SeekFrom::Start(5000)).unwrap(), 5000);
        ahead.read_exact(&mut at).unwrap();
        assert_eq!(&at[..], &bytes[5000..5100]);
        // Beyond anything read: the reader moves.
        let far = len - 2000;
        assert_eq!(ahead.seek(SeekFrom::Start(far)).unwrap(), far);
        ahead.read_exact(&mut at).unwrap();
        assert_eq!(&at[..], &bytes[far as usize..far as usize + 100]);
        // Back to the start, relative, and to the end.
        assert_eq!(ahead.seek(SeekFrom::Current(-100)).unwrap(), far);
        assert_eq!(ahead.seek(SeekFrom::End(-10)).unwrap(), len - 10);
        let mut tail = Vec::new();
        ahead.read_to_end(&mut tail).unwrap();
        assert_eq!(tail, &bytes[bytes.len() - 10..]);
        assert_eq!(ahead.read(&mut at).unwrap(), 0);
        drop(ahead);
        let _ = std::fs::remove_file(path);
    }
}

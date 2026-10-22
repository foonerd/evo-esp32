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
//! places by size keeps it out of its scarce memory. The source is any
//! seekable reader: the card's file, or a file on a server reached by
//! ranged requests ([`netfile`](crate::netfile)), whose waits are the
//! network's.

use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use symphonia::core::io::MediaSource;

/// Since boot: how often a reader moved its source, and the milliseconds
/// its sources spent in `seek` and in `read`. A decoder's seek is a run
/// of moves (a FLAC seek probes by bisection); the difference across
/// one says what it cost and where.
static RESTARTS: AtomicU32 = AtomicU32::new(0);
static SEEK_MS: AtomicU32 = AtomicU32::new(0);
static READ_MS: AtomicU32 = AtomicU32::new(0);

/// The readers' account since boot: moves of the source, milliseconds
/// in the source's `seek`, milliseconds in its `read`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Account {
    /// Moves of the source: a seek beyond what the ring held.
    pub restarts: u32,
    /// Milliseconds the sources spent in `seek`.
    pub seek_ms: u32,
    /// Milliseconds the sources spent in `read`.
    pub read_ms: u32,
}

impl Account {
    /// The account now.
    pub fn now() -> Self {
        Self {
            restarts: RESTARTS.load(Relaxed),
            seek_ms: SEEK_MS.load(Relaxed),
            read_ms: READ_MS.load(Relaxed),
        }
    }

    /// What happened since `earlier`.
    pub fn since(self, earlier: Self) -> Self {
        Self {
            restarts: self.restarts.wrapping_sub(earlier.restarts),
            seek_ms: self.seek_ms.wrapping_sub(earlier.seek_ms),
            read_ms: self.read_ms.wrapping_sub(earlier.read_ms),
        }
    }
}

fn add_ms(to: &AtomicU32, since: Instant) {
    to.fetch_add(since.elapsed().as_millis() as u32, Relaxed);
}

/// What the ring holds at most: 384 KB, of which [`BEHIND`] may be
/// behind the decoder — so at least 256 KB ahead, about a second of a
/// 96 kHz FLAC, twenty of an MP3. The ring is one allocation, large, so
/// a device that places by size puts it in its bulk memory.
const CAPACITY: usize = 384 * 1024;
/// How much of what the decoder has already taken the ring keeps, so a
/// seek back that far (the probe returning to the start after reading
/// through its 64 KB stream buffer, a FLAC seek refining its landing)
/// is served from memory and does not move the source — which for a
/// file on a server is a connection.
const BEHIND: usize = 128 * 1024;
/// What the reader asks the file for at a time: sixteen of the card's
/// blocks in one command. Small enough that a device which places its
/// allocations by size keeps the buffer in memory the card's DMA can
/// reach; a buffer it cannot reach is read a block at a time through a
/// bounce buffer, which on the ESP32-C5 cost a third of the core.
const CHUNK: usize = 8 * 1024;
/// The card's block: reads are asked for in whole blocks from a whole
/// block, which the file system passes straight to the card.
const BLOCK: usize = 512;
/// With less than this ahead of the decoder the reader reads chunk after
/// chunk; with this much in hand it pauses [`PACE`] between chunks. A
/// ring refilling from empty (a seek, a track's start) otherwise holds
/// the card's bus — which on the ESP32-C5 board is the panel's bus too
/// — for the whole 384 KB, and a paint stage of 90 ms took 450 ms
/// behind it. Paced, the reader still brings 8 KB in 6 ms, 1.3 MB/s,
/// twice what a 96 kHz FLAC takes.
const EAGER: usize = 4 * CHUNK;
/// The pause between chunks once [`EAGER`] is in hand.
const PACE: Duration = Duration::from_millis(6);
/// The reader's stack: the file system and the card's driver, or a
/// network transport's read.
const STACK: usize = 12 * 1024;

/// How the reader asks its source: by chunks of what size, how much it
/// keeps ahead before it slows, the pause between chunks then, and how
/// much the ring holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tuning {
    /// Bytes asked for at a time.
    pub chunk: usize,
    /// Below this much ahead, chunk after chunk; above, paced.
    pub eager: usize,
    /// The pause between chunks once eager is in hand.
    pub pace: Duration,
    /// What the ring holds at most, [`BEHIND`] of it behind the decoder.
    pub capacity: usize,
}

impl Tuning {
    /// The card: [`CHUNK`], [`EAGER`], [`PACE`], [`CAPACITY`] — small
    /// chunks the card's DMA reaches, paced so the panel's bus is
    /// shared.
    pub const fn card() -> Self {
        Tuning {
            chunk: CHUNK,
            eager: EAGER,
            pace: PACE,
            capacity: CAPACITY,
        }
    }

    /// A file on a server: the same chunk and ring as the card, a
    /// deeper landing zone so a seek or an open has its first chunks
    /// without pauses, then the pace, which lets a listing or the next
    /// track's open onto the radio between reads. One request is in
    /// flight at a time, so a round trip bounds the rate: 8 KB per
    /// 13 ms to a server on the desk is ~600 KB/s; per 25–75 ms to a
    /// NAS over the 2.4 GHz band, 110–330 KB/s, which is not a 96 kHz
    /// FLAC. Measured on the ESP32-C5 and set aside: 64 KB chunks into
    /// a 1 MB ring moved fewer bytes (80–450 KB/s) and took the core —
    /// three worker threads at 30 % beside the Wi-Fi driver and lwIP at
    /// 23 % — for reasons the thread names did not show. The lever that
    /// remains is a second request in flight.
    pub const fn network() -> Self {
        Tuning {
            chunk: CHUNK,
            eager: 8 * CHUNK,
            pace: PACE,
            capacity: CAPACITY,
        }
    }
}

struct State {
    /// Bytes read from the file, in file order: those behind `head`
    /// the decoder has taken and may come back for, those from `head`
    /// it has not.
    ring: VecDeque<u8>,
    /// The file offset of the ring's first byte.
    base: u64,
    /// Where in the ring the decoder's next byte is.
    head: usize,
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

impl State {
    /// Bytes the decoder has not taken yet.
    fn available(&self) -> usize {
        self.ring.len().saturating_sub(self.head)
    }

    /// Let go of what lies further behind the decoder than the window
    /// keeps, making room for the reader.
    fn trim(&mut self) {
        if self.head > BEHIND {
            let drop = self.head - BEHIND;
            self.ring.drain(..drop);
            self.base += drop as u64;
            self.head = BEHIND;
        }
    }

    /// Start the ring afresh for a decoder at `at`. The ring begins at
    /// the block boundary at or before it, so every read the source
    /// sees starts on a block and asks for whole blocks: a read that
    /// starts inside a block the card's file system splits into a
    /// partial block through its own buffer and the rest into the
    /// caller's buffer at an odd offset, which the card driver cannot
    /// address directly and copies block by block through a bounce
    /// buffer — a third of the ESP32-C5's core, for the rest of the
    /// file, after a seek into a 96 kHz FLAC.
    fn restart(&mut self, at: u64) {
        self.ring.clear();
        self.base = at - at % BLOCK as u64;
        self.head = (at - self.base) as usize;
        self.eof = false;
        self.error = None;
    }
}

struct Shared {
    state: Mutex<State>,
    /// Signalled when the ring gains bytes, ends, or fails.
    filled: Condvar,
    /// Signalled when the ring has room, a seek is wanted, or on stop.
    wanted: Condvar,
    /// What the ring holds at most.
    capacity: usize,
    /// This ring's own account: bytes its source gave, and the
    /// milliseconds spent in its `read`. The rate of one track.
    read_bytes: AtomicU32,
    read_ms: AtomicU32,
}

/// A file as a [`MediaSource`], read ahead by a thread.
pub struct ReadAhead {
    shared: Arc<Shared>,
    /// The file's length.
    len: u64,
    /// The decoder's position in the file.
    pos: u64,
}

/// A look at the ring from outside the decoder: whether a read would
/// find enough not to wait. For a source whose waits are the network's,
/// the tick asks before it decodes.
#[derive(Clone)]
pub struct Watch {
    shared: Arc<Shared>,
    low: usize,
}

impl Watch {
    /// The ring holds at least the low-water mark, or the source is at
    /// its end or in error (both of which a read returns at once).
    pub fn ready(&self) -> bool {
        let s = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        s.available() >= self.low || s.eof || s.error.is_some()
    }

    /// What this ring's source has given so far, and the milliseconds
    /// spent in its `read`: bytes and ms.
    pub fn tally(&self) -> (u32, u32) {
        (
            self.shared.read_bytes.load(Relaxed),
            self.shared.read_ms.load(Relaxed),
        )
    }
}

impl ReadAhead {
    /// A watch on the ring with `low` as its low-water mark.
    pub fn watch(&self, low: usize) -> Watch {
        Watch {
            shared: Arc::clone(&self.shared),
            low: low.min(self.shared.capacity - BEHIND),
        }
    }
}

impl ReadAhead {
    /// Start reading `source` from its start, tuned for the card;
    /// `len` is its length.
    pub fn new<S>(source: S, len: u64) -> std::io::Result<Self>
    where
        S: Read + Seek + Send + 'static,
    {
        Self::with_tuning(source, len, Tuning::card())
    }

    /// Start reading `source` from its start with `tuning`.
    pub fn with_tuning<S>(
        mut source: S,
        len: u64,
        tuning: Tuning,
    ) -> std::io::Result<Self>
    where
        S: Read + Seek + Send + 'static,
    {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                ring: VecDeque::with_capacity(tuning.capacity),
                base: 0,
                head: 0,
                eof: false,
                error: None,
                seek_to: None,
                generation: 0,
                stop: false,
            }),
            filled: Condvar::new(),
            wanted: Condvar::new(),
            capacity: tuning.capacity,
            read_bytes: AtomicU32::new(0),
            read_ms: AtomicU32::new(0),
        });
        let reader = Arc::clone(&shared);
        thread::Builder::new()
            .name("read".into())
            .stack_size(STACK)
            .spawn(move || read_ahead(&mut source, &reader, tuning))?;
        Ok(Self {
            shared,
            len,
            pos: 0,
        })
    }
}

/// The reader: fill the ring from the source while there is room, wait
/// when there is none, move when asked, stop when told.
fn read_ahead<S: Read + Seek>(file: &mut S, shared: &Shared, tuning: Tuning) {
    // The buffer is taken at a four-byte boundary within a slightly
    // larger allocation: the card's driver wants that alignment for a
    // direct transfer, and a byte vector promises none.
    let chunk = tuning.chunk.max(BLOCK) / BLOCK * BLOCK;
    let mut raw = vec![0u8; chunk + 4];
    let skew = raw.as_ptr().align_offset(4);
    let buf = &mut raw[skew..skew + chunk];
    loop {
        let (generation, want, paced) = {
            let mut s = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if s.stop {
                    return;
                }
                if let Some(to) = s.seek_to.take() {
                    s.restart(to);
                    let from = s.base;
                    RESTARTS.fetch_add(1, Relaxed);
                    let began = Instant::now();
                    let moved = file.seek(SeekFrom::Start(from));
                    add_ms(&SEEK_MS, began);
                    if let Err(e) = moved {
                        s.error = Some(format!("seek: {e}"));
                        shared.filled.notify_all();
                    }
                }
                s.trim();
                let room = shared.capacity.saturating_sub(s.ring.len());
                if s.eof || s.error.is_some() || room < BLOCK {
                    s = shared
                        .wanted
                        .wait(s)
                        .unwrap_or_else(|e| e.into_inner());
                    continue;
                }
                break (
                    s.generation,
                    room.min(chunk) / BLOCK * BLOCK,
                    s.available() >= tuning.eager,
                );
            }
        };
        if paced && !tuning.pace.is_zero() {
            thread::sleep(tuning.pace);
        }
        // The card is waited on with the lock released: the decoder
        // keeps taking from the ring meanwhile.
        let began = Instant::now();
        let got = file.read(&mut buf[..want]);
        add_ms(&READ_MS, began);
        add_ms(&shared.read_ms, began);
        if let Ok(n) = got {
            shared.read_bytes.fetch_add(n as u32, Relaxed);
        }
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
            if s.available() > 0 {
                let n = buf.len().min(s.available());
                let head = s.head;
                let (front, back) = s.ring.as_slices();
                if head < front.len() {
                    let from_front = n.min(front.len() - head);
                    buf[..from_front]
                        .copy_from_slice(&front[head..head + from_front]);
                    buf[from_front..n].copy_from_slice(&back[..n - from_front]);
                } else {
                    let at = head - front.len();
                    buf[..n].copy_from_slice(&back[at..at + n]);
                }
                s.head += n;
                self.pos += n as u64;
                s.trim();
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
            // Within the ring, behind or ahead of the decoder: move the
            // head.
            s.head = (target - s.base) as usize;
        } else {
            // The reader may be mid-read for the old place: the
            // generation moves now, so what it brings back is dropped.
            s.seek_to = Some(target);
            s.restart(target);
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
    use std::fs::File;
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

    /// A source that counts how often it is moved, and where to, and
    /// how much is asked of it at a time.
    struct Counted {
        inner: std::io::Cursor<Vec<u8>>,
        seeks: Arc<std::sync::atomic::AtomicUsize>,
        log: Arc<Mutex<Vec<(u64, usize)>>>,
    }

    impl Read for Counted {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let at = self.inner.position();
            self.log.lock().unwrap().push((at, buf.len()));
            self.inner.read(buf)
        }
    }

    impl Seek for Counted {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.seeks.fetch_add(1, Relaxed);
            self.inner.seek(pos)
        }
    }

    fn counted(
        bytes: &[u8],
    ) -> (
        Counted,
        Arc<std::sync::atomic::AtomicUsize>,
        Arc<Mutex<Vec<(u64, usize)>>>,
    ) {
        let seeks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let log = Arc::new(Mutex::new(Vec::new()));
        let source = Counted {
            inner: std::io::Cursor::new(bytes.to_vec()),
            seeks: Arc::clone(&seeks),
            log: Arc::clone(&log),
        };
        (source, seeks, log)
    }

    #[test]
    fn a_seek_back_within_the_window_does_not_move_the_source() {
        let bytes = pattern(CAPACITY + 50_000);
        let (source, seeks, _) = counted(&bytes);
        let mut ahead = ReadAhead::new(source, bytes.len() as u64).unwrap();
        let watch = ahead.watch(16 * 1024);
        // Read past the window's size, then come back to the start: the
        // probe's habit. The source is not moved.
        let mut taken = vec![0u8; BEHIND / 2];
        ahead.read_exact(&mut taken).unwrap();
        assert_eq!(ahead.seek(SeekFrom::Start(0)).unwrap(), 0);
        let mut head = [0u8; 64];
        ahead.read_exact(&mut head).unwrap();
        assert_eq!(&head[..], &bytes[..64]);
        assert_eq!(seeks.load(Relaxed), 0);
        // Far beyond what is held: the source moves once.
        let far = bytes.len() as u64 - 1000;
        ahead.seek(SeekFrom::Start(far)).unwrap();
        ahead.read_exact(&mut head).unwrap();
        assert_eq!(&head[..], &bytes[far as usize..far as usize + 64]);
        assert_eq!(seeks.load(Relaxed), 1);
        // At the end the watch says ready whatever the ring holds.
        let mut rest = Vec::new();
        ahead.read_to_end(&mut rest).unwrap();
        assert!(watch.ready());
    }

    #[test]
    fn after_a_seek_the_source_is_read_in_whole_blocks_from_a_block() {
        let bytes = pattern(2 * CAPACITY + 4321);
        let (source, _, log) = counted(&bytes);
        let mut ahead = ReadAhead::new(source, bytes.len() as u64).unwrap();
        // A FLAC frame begins where it begins: an odd offset far away.
        let target = CAPACITY as u64 + 100_003;
        ahead.seek(SeekFrom::Start(target)).unwrap();
        let mut got = [0u8; 2000];
        ahead.read_exact(&mut got).unwrap();
        assert_eq!(&got[..], &bytes[target as usize..target as usize + 2000]);
        let reads = log.lock().unwrap().clone();
        let after = reads
            .iter()
            .skip_while(|(at, _)| *at < CAPACITY as u64)
            .collect::<Vec<_>>();
        assert!(!after.is_empty());
        let block = BLOCK as u64;
        assert_eq!(after[0].0, target - target % block);
        for (at, len) in after {
            assert_eq!(at % block, 0, "a read from inside a block at {at}");
            assert_eq!(len % BLOCK, 0, "a read of {len} bytes at {at}");
        }
    }

    #[test]
    fn the_refill_is_eager_at_first_and_paced_after() {
        let bytes = pattern(CAPACITY);
        let (source, _, log) = counted(&bytes);
        let ahead = ReadAhead::new(source, bytes.len() as u64).unwrap();
        // Nothing is taken: the ring fills from empty, as after a seek.
        let eager_by = Instant::now();
        while log.lock().unwrap().len() < EAGER / CHUNK {
            assert!(eager_by.elapsed() < PACE * 20, "the eager zone paced");
            thread::yield_now();
        }
        // The rest comes at the pace, not all at once.
        let paced_by = Instant::now();
        let whole = CAPACITY / CHUNK;
        while log.lock().unwrap().len() < whole {
            thread::sleep(PACE / 3);
        }
        let paced = whole - EAGER / CHUNK;
        assert!(
            paced_by.elapsed() >= PACE * (paced as u32 - 2),
            "the ring filled in {:?}, unpaced",
            paced_by.elapsed()
        );
        drop(ahead);
    }

    #[test]
    fn the_network_tuning_lands_a_deeper_eager_zone_at_once() {
        let bytes = pattern(CAPACITY + 4 * CHUNK);
        let (source, _, log) = counted(&bytes);
        let tuning = Tuning::network();
        assert_eq!(tuning.chunk, CHUNK);
        assert_eq!(tuning.capacity, CAPACITY);
        let began = Instant::now();
        let mut ahead =
            ReadAhead::with_tuning(source, bytes.len() as u64, tuning).unwrap();
        let watch = ahead.watch(16 * 1024);
        while log.lock().unwrap().len() < tuning.eager / CHUNK {
            assert!(began.elapsed() < PACE * 20, "the eager zone paced");
            thread::yield_now();
        }
        // The bytes are the file's, and the ring's own tally counts them.
        let mut head = vec![0u8; 3 * CHUNK];
        ahead.read_exact(&mut head).unwrap();
        assert_eq!(head, &bytes[..3 * CHUNK]);
        let (tallied, _) = watch.tally();
        assert!(tallied as usize >= 3 * CHUNK, "tallied {tallied}");
        drop(ahead);
    }
}

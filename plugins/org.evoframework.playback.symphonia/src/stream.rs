// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! A stream from the network as a media source for the decoder.
//!
//! The transport is the distribution's ([`StreamOpener`]); this module
//! makes what it opens readable by Symphonia without the
//! decoder's tick ever waiting on the network. A fetch thread reads the
//! stream into a ring, well ahead; the decoder's side reads from the
//! ring and the tick asks [`Live::ready`] before decoding, so a packet
//! is only read when it is there. Where the server interleaves metadata into the
//! bytes (Icecast and SHOUTcast, every `icy-metaint` bytes when asked
//! with `Icy-MetaData: 1`), the blocks are taken out on the way in and
//! the `StreamTitle` they carry is kept for the player to show.

use std::io::{Read, Seek, SeekFrom};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use evo_esp32_sdk::contract::audio::playback::{ByteStream, StreamOpener};
use log::{info, warn};

/// How a dropped connection is taken up again.
pub struct Reopen {
    /// The distribution's transport.
    pub opener: Arc<dyn StreamOpener>,
    /// The stream's address, asked for again as it was.
    pub uri: String,
}

/// How many times in a row a dropped stream is opened again before the
/// fetcher gives up, and the pause before each try (doubling).
const RECONNECTS: u32 = 3;
const RECONNECT_PAUSE: Duration = Duration::from_secs(1);
/// After this many bytes flow from a reopened stream, the count of
/// tries starts again: a drop an hour later gets its own three.
const SETTLED: usize = 256 * 1024;
use symphonia::core::io::MediaSource;

/// The ring's size: half a minute of a 128 kb/s stream, four seconds
/// of a FLAC stream. A large allocation, which a board places in PSRAM.
const RING: usize = 512 * 1024;
/// What the ring gathers before the first packet is decoded: a second
/// and a half at the bitrate the server states, else this many bytes.
const PREBUFFER_MS: u64 = 1500;
const PREBUFFER_BYTES: usize = 48 * 1024;
/// Below this in the ring, the tick waits for the fetcher rather than
/// risk a read that blocks: a FLAC frame is up to 16 KB.
pub const LOW_WATER: usize = 24 * 1024;
/// How long a read on the decoder's side may wait for bytes before the
/// stream counts as lost. The tick's gate makes the wait rare.
const READ_PATIENCE: Duration = Duration::from_secs(4);
/// The fetch thread's read size.
const FETCH: usize = 4096;
/// The fetch thread's stack: it opens connections too — on a reconnect,
/// and for every segment and playlist of an HLS station — so it has
/// what the open thread has for a TLS handshake. Placed by the
/// distribution (PSRAM on the board).
const FETCH_STACK: usize = 32 * 1024;

/// What is known about a stream once its headers are read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamInfo {
    /// The server's `content-type`, lower case.
    pub content_type: Option<String>,
    /// The station's name (`icy-name`).
    pub name: Option<String>,
    /// The bitrate the server states, kb/s (`icy-br`).
    pub bitrate_kbps: Option<u32>,
    /// Bytes between metadata blocks (`icy-metaint`), when interleaved.
    pub metaint: Option<usize>,
}

impl StreamInfo {
    /// What the response's headers say.
    pub fn from(stream: &dyn ByteStream) -> Self {
        Self {
            content_type: stream
                .header("content-type")
                .map(|s| {
                    s.split(';')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_ascii_lowercase()
                })
                .filter(|s| !s.is_empty()),
            name: stream
                .header("icy-name")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            bitrate_kbps: stream
                .header("icy-br")
                .and_then(|s| s.split(',').next()?.trim().parse().ok()),
            metaint: stream
                .header("icy-metaint")
                .and_then(|s| s.trim().parse().ok())
                .filter(|&n: &usize| n > 0),
        }
    }

    /// Symphonia's hint for the container, from the content type.
    pub fn extension_hint(&self) -> Option<&'static str> {
        match self.content_type.as_deref()? {
            "audio/mpeg" | "audio/mp3" | "audio/mpeg3" => Some("mp3"),
            "audio/flac" | "audio/x-flac" => Some("flac"),
            "audio/ogg" | "application/ogg" | "audio/vorbis" => Some("ogg"),
            "audio/aac" | "audio/aacp" | "audio/x-aac" => Some("aac"),
            "audio/wav" | "audio/x-wav" | "audio/wave" => Some("wav"),
            _ => None,
        }
    }
}

/// What the probe may consume before it must have found the format:
/// a stream it cannot name is given up here, not at Symphonia's own
/// limit of a megabyte, which at 128 kb/s is a minute with the player
/// held. MP3 syncs within a few kilobytes; an Ogg page is at most 64 KB.
const PROBE_BUDGET: usize = 96 * 1024;

/// The ring between the fetcher and the decoder.
struct Ring {
    buf: Vec<u8>,
    /// Byte positions, monotonic.
    written: usize,
    read: usize,
    /// What the decoder's side may still read before the probe is done;
    /// `usize::MAX` once it is.
    budget: usize,
    /// The fetcher is done: the server closed or the transport failed.
    ended: bool,
    /// Why, when it failed.
    fault: Option<String>,
    /// The latest `StreamTitle`, if a new one has arrived.
    title: Option<String>,
}

impl Ring {
    fn available(&self) -> usize {
        self.written - self.read
    }

    fn room(&self) -> usize {
        self.buf.len() - self.available()
    }
}

struct Shared {
    ring: Mutex<Ring>,
    /// Signalled by both sides: bytes arrived, or room was made.
    changed: Condvar,
}

/// The decoder's side of a buffered stream: what Symphonia reads.
pub struct Buffered {
    shared: Arc<Shared>,
    pos: u64,
}

/// The player's side: whether the decoder may read without waiting,
/// the titles the stream carries, and how it ended.
pub struct Live {
    shared: Arc<Shared>,
    /// What the fetcher gathers before the first read is allowed through.
    prebuffer: usize,
    primed: bool,
    /// What the headers said.
    pub info: StreamInfo,
}

/// Start fetching `stream` on a thread: the reader for the decoder and
/// the handle for the player. The thread ends when the stream ends and
/// cannot be reopened (`reopen`, when given, is tried three times with
/// a doubling pause) or both are dropped.
pub fn start(
    stream: Box<dyn ByteStream>,
    reopen: Option<Reopen>,
) -> std::io::Result<(Buffered, Live)> {
    let info = StreamInfo::from(stream.as_ref());
    let prebuffer = info
        .bitrate_kbps
        .map(|k| (k as u64 * 1000 / 8 * PREBUFFER_MS / 1000) as usize)
        .unwrap_or(PREBUFFER_BYTES)
        .clamp(8 * 1024, RING / 2);
    let shared = Arc::new(Shared {
        ring: Mutex::new(Ring {
            buf: vec![0u8; RING],
            written: 0,
            read: 0,
            budget: PROBE_BUDGET,
            ended: false,
            fault: None,
            title: None,
        }),
        changed: Condvar::new(),
    });
    let metaint = info.metaint;
    let fetcher = Arc::downgrade(&shared);
    evo_esp32_sdk::thread::spawn(
        evo_esp32_sdk::thread::Role::Feed,
        "fetch",
        FETCH_STACK,
        move || fetch(stream, metaint, reopen, fetcher),
    )?;
    Ok((
        Buffered {
            shared: shared.clone(),
            pos: 0,
        },
        Live {
            shared,
            prebuffer,
            primed: false,
            info,
        },
    ))
}

impl Live {
    /// Bytes in the ring not yet read.
    pub fn available(&self) -> usize {
        self.shared.ring.lock().map_or(0, |r| r.available())
    }

    /// The fetcher has stopped: the server closed (`Some(None)`) or the
    /// transport failed (`Some(Some(why))`).
    pub fn ended(&self) -> Option<Option<String>> {
        let r = self.shared.ring.lock().ok()?;
        r.ended.then(|| r.fault.clone())
    }

    /// True when the decoder may read without waiting: the ring holds
    /// the prebuffer at first and [`LOW_WATER`] after, or the stream has
    /// ended and what is left is all there is.
    pub fn ready(&mut self) -> bool {
        let Ok(r) = self.shared.ring.lock() else {
            return true;
        };
        if r.ended {
            return true;
        }
        let need = if self.primed {
            LOW_WATER
        } else {
            self.prebuffer
        };
        if r.available() >= need {
            self.primed = true;
            return true;
        }
        false
    }

    /// The `StreamTitle` that arrived since the last call, if any.
    pub fn take_title(&self) -> Option<String> {
        self.shared.ring.lock().ok()?.title.take()
    }

    /// The probe named the format: the decoder may read without limit.
    pub fn probed(&self) {
        if let Ok(mut r) = self.shared.ring.lock() {
            r.budget = usize::MAX;
        }
    }
}

impl Read for Buffered {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut r = self
            .shared
            .ring
            .lock()
            .map_err(|_| std::io::Error::other("ring poisoned"))?;
        let deadline = Instant::now() + READ_PATIENCE;
        while r.available() == 0 && !r.ended {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "the stream stalled",
                ));
            }
            r = self
                .shared
                .changed
                .wait_timeout(r, left)
                .map_err(|_| std::io::Error::other("ring poisoned"))?
                .0;
        }
        if r.available() == 0 {
            // Ended with nothing left: the end of the stream.
            return Ok(0);
        }
        if r.budget == 0 {
            // The probe read its allowance without naming the format.
            return Ok(0);
        }
        let n = buf.len().min(r.available()).min(r.budget);
        if r.budget != usize::MAX {
            r.budget -= n;
        }
        let len = r.buf.len();
        let at = r.read % len;
        let first = n.min(len - at);
        buf[..first].copy_from_slice(&r.buf[at..at + first]);
        buf[first..n].copy_from_slice(&r.buf[..n - first]);
        r.read += n;
        self.pos += n as u64;
        drop(r);
        self.shared.changed.notify_all();
        Ok(n)
    }
}

impl Seek for Buffered {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        // Forward within what is buffered is a read and a discard;
        // anything else a live stream cannot do.
        match pos {
            SeekFrom::Current(0) => Ok(self.pos),
            SeekFrom::Current(n) if n > 0 => {
                let mut left = n as u64;
                let mut sink = [0u8; 512];
                while left > 0 {
                    let take = (left as usize).min(sink.len());
                    let got = self.read(&mut sink[..take])?;
                    if got == 0 {
                        break;
                    }
                    left -= got as u64;
                }
                Ok(self.pos)
            }
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "a live stream does not seek",
            )),
        }
    }
}

impl MediaSource for Buffered {
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}

/// The fetch thread: the stream into the ring, metadata blocks taken
/// out, until the stream ends or the ring is dropped.
fn fetch(
    mut stream: Box<dyn ByteStream>,
    metaint: Option<usize>,
    reopen: Option<Reopen>,
    shared: std::sync::Weak<Shared>,
) {
    let mut chunk = vec![0u8; FETCH];
    // Where we are between metadata blocks, and the block in progress.
    let mut metaint = metaint;
    let mut until_meta = metaint.unwrap_or(usize::MAX);
    let mut meta: Option<(usize, Vec<u8>)> = None;
    let mut tries = 0u32;
    let mut since_reopen = 0usize;
    let finish = |why: Option<String>| {
        if let Some(s) = shared.upgrade() {
            if let Ok(mut r) = s.ring.lock() {
                r.ended = true;
                r.fault = why;
            }
            s.changed.notify_all();
        }
    };
    loop {
        let n = match stream.read(&mut chunk) {
            Ok(n) if n > 0 => n,
            end => {
                // The server closed, or the transport failed: for a live
                // stream both are a drop, taken up again a few times.
                // Without a way to reopen, a close is the end.
                let Some(again) = reopen.as_ref() else {
                    return finish(end.err());
                };
                let mut why = match end {
                    Ok(_) => "the server closed the stream".to_string(),
                    Err(e) => e,
                };
                loop {
                    if since_reopen >= SETTLED {
                        tries = 0;
                    }
                    if tries >= RECONNECTS {
                        warn!("stream: {why}; given up after {tries} tries");
                        return finish(Some(why));
                    }
                    tries += 1;
                    since_reopen = 0;
                    let pause = RECONNECT_PAUSE * 2u32.pow(tries - 1);
                    warn!(
                        "stream: {why}; opening again in {} ms ({tries}/{RECONNECTS})",
                        pause.as_millis()
                    );
                    thread::sleep(pause);
                    if shared.upgrade().is_none() {
                        return;
                    }
                    match again.opener.open(&again.uri) {
                        Ok(fresh) => {
                            // A fresh response: its own metadata interval,
                            // and the count to the next block starts over.
                            metaint = StreamInfo::from(fresh.as_ref()).metaint;
                            until_meta = metaint.unwrap_or(usize::MAX);
                            meta = None;
                            stream = fresh;
                            info!("stream: open again");
                            break;
                        }
                        Err(e) => why = format!("open again: {e}"),
                    }
                }
                continue;
            }
        };
        since_reopen += n;
        let mut bytes = &chunk[..n];
        while !bytes.is_empty() {
            let Some(s) = shared.upgrade() else {
                return;
            };
            if let Some((left, block)) = meta.as_mut() {
                // Inside a metadata block: gather it whole.
                let take = (*left).min(bytes.len());
                block.extend_from_slice(&bytes[..take]);
                bytes = &bytes[take..];
                *left -= take;
                if *left == 0 {
                    if let Some(title) = stream_title(block) {
                        if let Ok(mut r) = s.ring.lock() {
                            r.title = Some(title);
                        }
                    }
                    meta = None;
                    until_meta = metaint.unwrap_or(usize::MAX);
                }
                continue;
            }
            if until_meta == 0 {
                // The length byte: sixteen-byte units.
                let len = usize::from(bytes[0]) * 16;
                bytes = &bytes[1..];
                if len == 0 {
                    until_meta = metaint.unwrap_or(usize::MAX);
                } else {
                    meta = Some((len, Vec::with_capacity(len)));
                }
                continue;
            }
            let audio = bytes.len().min(until_meta);
            // Into the ring, waiting for room when the decoder is behind.
            let mut r = match s.ring.lock() {
                Ok(r) => r,
                Err(_) => return,
            };
            while r.room() == 0 {
                r = match s.changed.wait_timeout(r, Duration::from_millis(200))
                {
                    Ok((r, _)) => r,
                    Err(_) => return,
                };
                if Arc::strong_count(&s) == 1 {
                    // Both the decoder's and the player's sides are gone.
                    return;
                }
            }
            let take = audio.min(r.room());
            let len = r.buf.len();
            let at = r.written % len;
            let first = take.min(len - at);
            r.buf[at..at + first].copy_from_slice(&bytes[..first]);
            r.buf[..take - first].copy_from_slice(&bytes[first..take]);
            r.written += take;
            drop(r);
            s.changed.notify_all();
            bytes = &bytes[take..];
            until_meta -= take;
        }
    }
}

/// The `StreamTitle='...'` of an ICY metadata block, if it has one with
/// something in it.
fn stream_title(block: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(block);
    let text = text.trim_end_matches('\0');
    let rest = text.split("StreamTitle='").nth(1)?;
    let title = rest.split("';").next().unwrap_or(rest).trim();
    (!title.is_empty()).then(|| title.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stream from memory, served in pieces, with headers.
    struct Canned {
        bytes: Vec<u8>,
        at: usize,
        piece: usize,
        headers: Vec<(&'static str, String)>,
    }

    impl ByteStream for Canned {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, String> {
            let n = buf.len().min(self.piece).min(self.bytes.len() - self.at);
            buf[..n].copy_from_slice(&self.bytes[self.at..self.at + n]);
            self.at += n;
            Ok(n)
        }

        fn header(&self, name: &str) -> Option<String> {
            self.headers
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.clone())
        }
    }

    /// Opens a short canned stream each time, `opens` counting; after
    /// `serve` opens it refuses, so the fetcher gives up.
    struct Dropping {
        opens: std::sync::atomic::AtomicUsize,
        serve: usize,
    }

    impl StreamOpener for Dropping {
        fn open(&self, _uri: &str) -> Result<Box<dyn ByteStream>, String> {
            let n =
                self.opens.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n >= self.serve {
                return Err("refused".into());
            }
            Ok(Box::new(Canned {
                bytes: vec![b'a' + n as u8; 10],
                at: 0,
                piece: 4,
                headers: vec![],
            }))
        }
    }

    #[test]
    fn a_dropped_stream_is_opened_again_then_given_up() {
        let opener: Arc<dyn StreamOpener> = Arc::new(Dropping {
            opens: std::sync::atomic::AtomicUsize::new(1),
            serve: 3,
        });
        let first = Canned {
            bytes: vec![b'a'; 10],
            at: 0,
            piece: 4,
            headers: vec![],
        };
        let (mut b, live) = start(
            Box::new(first),
            Some(Reopen {
                opener,
                uri: "http://x.example/s".into(),
            }),
        )
        .unwrap();
        live.probed();
        let mut out = Vec::new();
        // Pauses of 1, 2 and 4 s between tries; the read waits them out.
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut buf = [0u8; 64];
        while Instant::now() < deadline {
            match b.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
                Err(e) => panic!("{e}"),
            }
        }
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "aaaaaaaaaabbbbbbbbbbcccccccccc",
            "the first, then two reopened streams, in one flow"
        );
        assert!(
            matches!(live.ended(), Some(Some(why)) if why.contains("refused"))
        );
    }

    #[test]
    fn metadata_blocks_come_out_and_titles_are_kept() {
        // metaint 8: eight bytes of audio, a length byte, a block.
        let title = b"StreamTitle='Dire Straits - Telegraph Road';";
        let mut block = title.to_vec();
        block.resize(48, 0); // three 16-byte units
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"AAAAAAAA");
        bytes.push(3);
        bytes.extend_from_slice(&block);
        bytes.extend_from_slice(b"BBBBBBBB");
        bytes.push(0);
        bytes.extend_from_slice(b"CC");
        let stream = Canned {
            bytes,
            at: 0,
            piece: 5,
            headers: vec![
                ("icy-metaint", "8".into()),
                ("icy-name", "Test FM".into()),
                ("icy-br", "128".into()),
                ("content-type", "audio/mpeg; charset=x".into()),
            ],
        };
        let (mut b, live) = start(Box::new(stream), None).unwrap();
        assert_eq!(live.info.name.as_deref(), Some("Test FM"));
        assert_eq!(live.info.bitrate_kbps, Some(128));
        assert_eq!(live.info.extension_hint(), Some("mp3"));
        let mut out = Vec::new();
        b.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"AAAAAAAABBBBBBBBCC");
        assert_eq!(
            live.take_title().as_deref(),
            Some("Dire Straits - Telegraph Road")
        );
        assert_eq!(live.take_title(), None);
        assert_eq!(live.ended(), Some(None));
    }

    #[test]
    fn without_metaint_the_bytes_pass_whole_and_ready_waits_for_the_prebuffer()
    {
        let bytes: Vec<u8> = (0..100_000u32).map(|i| i as u8).collect();
        let stream = Canned {
            bytes: bytes.clone(),
            at: 0,
            piece: 4096,
            headers: vec![],
        };
        let (mut b, mut live) = start(Box::new(stream), None).unwrap();
        // The fetcher runs ahead; ready once the prebuffer is in or the
        // stream has ended.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !live.ready() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(live.ready());
        // Until the probe is declared done, reads stop at its budget.
        let mut probe = vec![0u8; PROBE_BUDGET + 10];
        let mut got = 0;
        loop {
            let n = b.read(&mut probe[got..]).unwrap();
            if n == 0 {
                break;
            }
            got += n;
        }
        assert_eq!(got, PROBE_BUDGET, "the probe's allowance, then an end");
        live.probed();
        let mut out = probe[..got].to_vec();
        b.read_to_end(&mut out).unwrap();
        assert_eq!(out, bytes, "after it, the rest comes whole");
    }
}

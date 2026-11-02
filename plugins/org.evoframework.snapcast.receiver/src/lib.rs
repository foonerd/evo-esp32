// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! The device as one room of a Snapcast multiroom.
//!
//! `org.evoframework.snapcast.receiver` stocks `playback/snapcast`. A
//! Snapcast server streams every client chunks of audio stamped in the
//! server's clock, and each client plays a chunk when its stamp plus the
//! server's buffer comes round on its own clock, the two clocks tied by
//! time probes. This plugin is such a
//! client: a thread dials the server the distribution names (from the
//! card, or found on the network), says hello, probes the clock, takes
//! the codec header and the chunks, decodes them (FLAC through
//! Symphonia, frame by frame; PCM as it is) and queues the PCM with the
//! time it is due. The distribution watches the [`Link`]; when chunks
//! are flowing it hands this warden the sink under a [`Receive`]
//! custody, and the warden plays each chunk at its time: waiting when
//! early, trimming when late, and holding the clocks together by
//! dropping or doubling a frame now and then. The server's volume and
//! mute are honoured. A server that goes away is dialled again, with a
//! lengthening pause, for as long as the device runs.
#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use evo_esp32_sdk::context::Context;
use evo_esp32_sdk::contract::audio::playback::{
    Correction, Format, FrameSink, Receive, State, Status, KIND,
};
use evo_esp32_sdk::contract::audio::{from_value, shelf, to_value, Track};
use evo_esp32_sdk::plugin::{
    InstanceShape, InteractionShape, LoadError, Plugin, PluginDescription,
    Stocking,
};
use evo_esp32_sdk::subject::{
    ExternalAddressing, SubjectAnnouncement, SubjectId,
};
use evo_esp32_sdk::warden::{
    Assignment, CourseCorrection, CustodyError, CustodyHandle, Progress, Warden,
};

pub mod flac;
pub mod wire;
use wire::{Inbound, ReadError, ServerSettings};

/// The plugin's name, as the catalogue and the steward know it.
pub const NAME: &str = "org.evoframework.snapcast.receiver";
/// Snapcast's stream port.
pub const DEFAULT_PORT: u16 = 1704;
/// How often the clock is probed once it has settled.
pub const PROBE_INTERVAL: Duration = Duration::from_secs(1);
/// Probes at the start come quickly, until this many have answered.
const QUICK_PROBES: usize = 10;
const QUICK_PROBE_INTERVAL: Duration = Duration::from_millis(200);
/// Clock differences kept; the median is the one used.
const DIFFS_KEPT: usize = 15;
/// Queued audio beyond the server's buffer that is kept; past it the
/// oldest chunk goes.
const QUEUE_SLACK_MS: i64 = 2000;
/// A server silent this long with a custody open has stopped.
const SILENCE_LIMIT: Duration = Duration::from_secs(5);
/// A chunk's age: when it would be heard, written now, less when the
/// server says it should be. What the sink holds plays on while the
/// clock runs, so a chunk's age does not change while it waits; it is
/// set by what was written before it, and moved only by writing
/// silence ahead of it, trimming it, or dropping or doubling a frame.
///
/// Earlier than this a chunk is not written as it is: silence goes in
/// first (the sink's own, when it is empty; written, when it holds a
/// stream that paused on the server).
const EARLY_US: i64 = 3000;
/// Later than this a chunk is dropped whole.
const LATE_US: i64 = 200_000;
/// The clocks are judged by the median age of this many chunks, not by
/// one, and afresh after every correction: the sink's account of what
/// it holds is good to a millisecond or two, a clock's drift a steady
/// lean.
const AGES_KEPT: usize = 9;
/// A lean smaller than this is left alone.
const DEADBAND_US: i64 = 3000;
/// A lean larger than this is taken up at once by trimming the front
/// of the chunk; smaller ones a frame at a time.
const HARD_US: i64 = 20_000;
/// The most frames dropped or doubled in one chunk when leaning gently:
/// one in fifty, a two percent change of pace for that chunk.
const GENTLE_SHARE: usize = 50;
/// The most silence written ahead of an early chunk in one go.
const PAD_LIMIT_US: i64 = 100_000;
/// A chunk's stamp this far from where the count of samples puts it is
/// a stream that stopped and started: the count begins again.
const REANCHOR_US: i64 = 100_000;
const TICK_IDLE_MS: u32 = 2;
const READ_TIMEOUT: Duration = Duration::from_millis(100);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// The pause before dialling again, doubling to the last.
const RETRY: [u64; 6] = [1, 2, 4, 8, 16, 30];
/// The connection thread's stack: the FLAC decoder's frame work;
/// placed by the distribution (PSRAM on the board).
const LINK_STACK: usize = 40 * 1024;

static STOCKS: [Stocking; 1] = [Stocking {
    rack: shelf::PLAYBACK,
    shelf: shelf::SNAPCAST,
    shape: shelf::SHAPE,
}];

/// What the distribution tells the client about itself.
#[derive(Debug, Clone)]
pub struct Settings {
    /// What the server knows this client by: its MAC, by convention.
    pub client_id: String,
    /// The name shown in the server's client list.
    pub hostname: String,
    /// The version announced.
    pub version: String,
    /// How often to probe once settled; [`PROBE_INTERVAL`].
    pub probe_interval: Duration,
}

impl Settings {
    /// The defaults for `client_id` and `hostname`.
    pub fn new(
        client_id: impl Into<String>,
        hostname: impl Into<String>,
        version: impl Into<String>,
    ) -> Self {
        Self {
            client_id: client_id.into(),
            hostname: hostname.into(),
            version: version.into(),
            probe_interval: PROBE_INTERVAL,
        }
    }
}

/// A decoded chunk, due at a time on the server's clock; its samples
/// are the next `frames` of the queue's ring.
#[derive(Debug, Clone, Copy)]
struct Chunk {
    /// When it plays, microseconds on the server's clock.
    due_us: i64,
    /// Frames (per channel).
    frames: usize,
}

/// The decoded chunks waiting for their time: one ring of interleaved
/// samples, sized for the server's buffer and some slack at the
/// stream's rate, and the chunks in order over it. One allocation,
/// made large so a distribution that places by size puts it in its
/// bulk memory; a chunk is copied in and copied out.
#[derive(Default)]
struct Queue {
    ring: Vec<i16>,
    head: usize,
    len: usize,
    chunks: VecDeque<Chunk>,
    /// What the ring was sized for.
    sized_for: (u32, usize, i64),
}

impl Queue {
    /// The ring for `buffer_ms` plus [`QUEUE_SLACK_MS`] at `rate` and
    /// `channels`; resized (and emptied) when they change.
    fn fit(&mut self, rate: u32, channels: usize, buffer_ms: i64) {
        if self.sized_for == (rate, channels, buffer_ms) {
            return;
        }
        let frames =
            (buffer_ms.max(0) + QUEUE_SLACK_MS) as usize * rate as usize / 1000;
        self.ring = vec![0; frames.max(1) * channels];
        self.head = 0;
        self.len = 0;
        self.chunks.clear();
        self.sized_for = (rate, channels, buffer_ms);
    }

    /// Append; the oldest chunks go when there is no room. The chunks
    /// dropped.
    fn push(&mut self, due_us: i64, frames: usize, samples: &[i16]) -> u64 {
        let mut dropped = 0;
        if samples.len() > self.ring.len() {
            return 1;
        }
        while self.len + samples.len() > self.ring.len() {
            self.drop_front();
            dropped += 1;
        }
        let tail = (self.head + self.len) % self.ring.len();
        let first = samples.len().min(self.ring.len() - tail);
        self.ring[tail..tail + first].copy_from_slice(&samples[..first]);
        self.ring[..samples.len() - first].copy_from_slice(&samples[first..]);
        self.len += samples.len();
        self.chunks.push_back(Chunk { due_us, frames });
        dropped
    }

    fn peek_due(&self) -> Option<i64> {
        self.chunks.front().map(|c| c.due_us)
    }

    /// The front chunk's samples into `out` (replaced), and the chunk.
    fn pop_into(&mut self, out: &mut Vec<i16>) -> Option<Chunk> {
        let chunk = *self.chunks.front()?;
        let channels = self.sized_for.1;
        let n = chunk.frames * channels;
        out.clear();
        let first = n.min(self.ring.len() - self.head);
        out.extend_from_slice(&self.ring[self.head..self.head + first]);
        out.extend_from_slice(&self.ring[..n - first]);
        self.advance(chunk);
        Some(chunk)
    }

    fn drop_front(&mut self) -> bool {
        match self.chunks.front().copied() {
            Some(chunk) => {
                self.advance(chunk);
                true
            }
            None => false,
        }
    }

    fn advance(&mut self, chunk: Chunk) {
        let n = chunk.frames * self.sized_for.1;
        self.head = (self.head + n) % self.ring.len();
        self.len -= n;
        self.chunks.pop_front();
    }

    fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
        self.chunks.clear();
    }
}

/// The connection as the distribution sees it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Session {
    /// A server is connected.
    pub connected: bool,
    /// Its address.
    pub server: Option<String>,
    /// The stream's name, from its tags.
    pub stream: Option<String>,
    /// The codec the server sends.
    pub codec: Option<String>,
    /// The stream's rate, once the codec header is in.
    pub rate: u32,
    /// Its channels.
    pub channels: u8,
    /// Its depth.
    pub bits: u8,
    /// The server's buffer: how far behind its stamp a chunk plays.
    pub buffer_ms: i64,
    /// This client's latency, as the server set it; taken off the buffer.
    pub latency_ms: i64,
    /// This client's volume as the server set it, 0 to 100.
    pub volume: u8,
    /// Muted by the server.
    pub muted: bool,
    /// The server's clock less ours, microseconds, once probed.
    pub diff_us: Option<i64>,
    /// Clock probes answered on this connection.
    pub probes: u32,
    /// Chunks received.
    pub chunks: u64,
    /// Chunks dropped unplayed.
    pub dropped: u64,
    /// Frames that would not decode.
    pub bad_frames: u64,
    /// Counts connections; a custody is for one of them.
    pub generation: u32,
}

struct Shared {
    session: Mutex<Session>,
    queue: Mutex<Queue>,
    server: Mutex<Option<String>>,
    /// When the last chunk arrived, milliseconds on `epoch` (32 bits:
    /// the target has no 64-bit atomics).
    last_chunk_ms: AtomicU32,
    epoch: Instant,
    generation: AtomicU32,
    stop: AtomicBool,
}

/// The client's connection and chunks, shared between its thread, the
/// warden and the distribution. Cheap to clone.
#[derive(Clone)]
pub struct Link(Arc<Shared>);

impl Default for Link {
    fn default() -> Self {
        Self::new()
    }
}

impl Link {
    /// A link with no server yet.
    pub fn new() -> Self {
        Link(Arc::new(Shared {
            session: Mutex::new(Session::default()),
            queue: Mutex::new(Queue::default()),
            server: Mutex::new(None),
            last_chunk_ms: AtomicU32::new(0),
            epoch: Instant::now(),
            generation: AtomicU32::new(0),
            stop: AtomicBool::new(false),
        }))
    }

    /// Name the server to dial: `host` or `host:port`. `None` lets the
    /// connection go and waits for another name.
    pub fn set_server(&self, server: Option<String>) {
        if let Ok(mut s) = self.0.server.lock() {
            *s = server;
        }
    }

    /// The server named, if any.
    pub fn server(&self) -> Option<String> {
        self.0.server.lock().ok().and_then(|s| s.clone())
    }

    /// The connection now.
    pub fn session(&self) -> Session {
        self.0.session.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// The server is connected, its clock is known (the quick probes
    /// have answered) and it sent a chunk in the last two seconds: the
    /// distribution should hand over the sink. The stream's name and
    /// the server, for the [`Receive`] instruction.
    pub fn sending(&self) -> Option<Receive> {
        let s = self.session();
        if !s.connected
            || s.chunks == 0
            || s.rate == 0
            || (s.probes as usize) < QUICK_PROBES
        {
            return None;
        }
        if self.since_last_chunk_ms() > 2000 {
            return None;
        }
        Some(Receive {
            group: s.stream.unwrap_or_else(|| "Snapcast".into()),
            leader: s.server?,
        })
    }

    /// Chunks queued and not yet played.
    pub fn queued(&self) -> usize {
        self.0.queue.lock().map(|q| q.chunks.len()).unwrap_or(0)
    }

    /// The client's clock: microseconds since the link was made.
    pub fn now_us(&self) -> i64 {
        self.0.epoch.elapsed().as_micros() as i64
    }

    fn now_ms(&self) -> u32 {
        (self.now_us() / 1000) as u32
    }

    /// Milliseconds since the last chunk arrived. The stamp is read
    /// before the clock: the client's thread may store a newer stamp
    /// between the two, and that is no silence, not a wrap.
    fn since_last_chunk_ms(&self) -> i32 {
        let last = self.0.last_chunk_ms.load(Ordering::Relaxed);
        self.now_ms().wrapping_sub(last) as i32
    }

    /// Queue a decoded chunk. The ring keeps the server's buffer and
    /// some slack; past that, the oldest were never going to play in
    /// time and go.
    fn push(
        &self,
        due_us: i64,
        frames: usize,
        samples: &[i16],
        rate: u32,
        channels: usize,
        buffer_ms: i64,
    ) {
        let dropped = match self.0.queue.lock() {
            Ok(mut q) => {
                q.fit(rate, channels, buffer_ms);
                q.push(due_us, frames, samples)
            }
            Err(_) => 0,
        };
        self.0.last_chunk_ms.store(self.now_ms(), Ordering::Relaxed);
        if let Ok(mut s) = self.0.session.lock() {
            s.chunks += 1;
            s.dropped += dropped;
        }
    }

    fn peek_due(&self) -> Option<i64> {
        self.0.queue.lock().ok()?.peek_due()
    }

    fn pop_into(&self, out: &mut Vec<i16>) -> Option<Chunk> {
        self.0.queue.lock().ok()?.pop_into(out)
    }

    fn drop_front(&self) {
        if self.0.queue.lock().is_ok_and(|mut q| q.drop_front()) {
            if let Ok(mut s) = self.0.session.lock() {
                s.dropped += 1;
            }
        }
    }

    fn clear(&self) {
        if let Ok(mut q) = self.0.queue.lock() {
            q.clear();
        }
    }
}

/// The Snapcast client warden.
pub struct SnapcastReceiver {
    sink: Box<dyn FrameSink>,
    settings: Settings,
    link: Link,
    subject: Option<SubjectId>,
    in_hand: Option<InHand>,
}

struct InHand {
    handle: CustodyHandle,
    state: State,
    generation: u32,
    frames_out: u64,
    pending: Vec<i16>,
    taken: usize,
    /// Frames dropped and doubled to hold the clocks together.
    dropped_frames: u64,
    doubled_frames: u64,
    /// The last chunks' ages: how late each was when it came to play.
    ages: VecDeque<i64>,
}

impl SnapcastReceiver {
    /// A client writing to `sink`, reporting through `link`.
    pub fn new(
        sink: Box<dyn FrameSink>,
        settings: Settings,
        link: Link,
    ) -> Self {
        Self {
            sink,
            settings,
            link,
            subject: None,
            in_hand: None,
        }
    }

    fn publish(&self, ctx: &mut dyn Context, state: &State) {
        if let Some(id) = &self.subject {
            let _ = ctx.update(id, to_value(state));
        }
    }
}

impl Plugin for SnapcastReceiver {
    fn describe(&self) -> PluginDescription {
        PluginDescription {
            name: NAME,
            version: env!("CARGO_PKG_VERSION"),
            instance: InstanceShape::Singleton,
            interaction: InteractionShape::Warden,
            stocks: &STOCKS,
        }
    }

    fn load(&mut self, ctx: &mut dyn Context) -> Result<(), LoadError> {
        let id = ctx
            .announce(SubjectAnnouncement {
                addressing: ExternalAddressing::new("playback", "snapcast"),
                kind: KIND.into(),
                state: to_value(&State::default()),
            })
            .map_err(|e| LoadError(format!("announcing: {e}")))?;
        self.subject = Some(id);
        let (link, settings) = (self.link.clone(), self.settings.clone());
        evo_esp32_sdk::thread::spawn(
            evo_esp32_sdk::thread::Role::Feed,
            "snap",
            LINK_STACK,
            move || dial_forever(&link, &settings),
        )
        .map_err(|e| LoadError(format!("client thread: {e}")))?;
        Ok(())
    }

    fn unload(&mut self, ctx: &mut dyn Context) {
        self.link.0.stop.store(true, Ordering::Relaxed);
        if self.in_hand.take().is_some() {
            self.sink.stop();
        }
        if let Some(id) = self.subject.take() {
            let _ = ctx.retract(&id);
        }
    }
}

impl Warden for SnapcastReceiver {
    fn take_custody(
        &mut self,
        handle: CustodyHandle,
        assignment: Assignment,
        ctx: &mut dyn Context,
    ) -> Result<(), CustodyError> {
        if self.in_hand.is_some() {
            return Err(CustodyError("already receiving".into()));
        }
        let receive: Receive = from_value(&assignment.instruction)
            .map_err(|e| CustodyError(format!("instruction: {e}")))?;
        let session = self.link.session();
        if !session.connected
            || session.server.as_deref() != Some(&receive.leader)
        {
            return Err(CustodyError("no such server connected".into()));
        }
        if session.rate == 0 {
            return Err(CustodyError("no codec header yet".into()));
        }
        let format = Format {
            codec: session.codec.clone().unwrap_or_else(|| "pcm".into()),
            sample_rate: session.rate,
            bits: 16,
            channels: session.channels.max(1),
        };
        self.sink.start(&format);
        let state = State {
            status: Status::Playing,
            track: Some(Track {
                uri: format!("snapcast://{}", receive.leader),
                title: receive.group.clone(),
                artist: Some(format!("from {}", receive.leader)),
                album: Some("Snapcast".into()),
                ..Track::default()
            }),
            position_ms: 0,
            format: Some(format),
        };
        self.publish(ctx, &state);
        self.in_hand = Some(InHand {
            handle,
            state,
            generation: session.generation,
            frames_out: 0,
            pending: Vec::new(),
            taken: 0,
            dropped_frames: 0,
            doubled_frames: 0,
            ages: VecDeque::new(),
        });
        Ok(())
    }

    fn course_correct(
        &mut self,
        handle: CustodyHandle,
        correction: CourseCorrection,
        ctx: &mut dyn Context,
    ) -> Result<(), CustodyError> {
        let Some(h) = self.in_hand.as_mut().filter(|h| h.handle == handle)
        else {
            return Err(CustodyError("no such custody".into()));
        };
        let c: Correction = from_value(&correction.instruction)
            .map_err(|e| CustodyError(format!("correction: {e}")))?;
        match c {
            Correction::Pause => {
                h.state.status = Status::Paused;
                self.sink.pause();
            }
            Correction::Resume => {
                // What is due has its time; what was held is behind it
                // and goes. The schedule is the server's, not ours.
                h.pending.clear();
                h.taken = 0;
                h.ages.clear();
                h.state.status = Status::Playing;
                self.sink.flush();
                self.sink.resume();
            }
            Correction::Seek { .. } => {
                return Err(CustodyError("a room cannot be seeked".into()));
            }
        }
        let state = h.state.clone();
        self.publish(ctx, &state);
        Ok(())
    }

    fn release_custody(
        &mut self,
        handle: CustodyHandle,
        ctx: &mut dyn Context,
    ) {
        if let Some(h) = self.in_hand.take_if(|h| h.handle == handle) {
            log::info!(
                "snapcast: played {} frames, dropped {} and doubled {} to keep time",
                h.frames_out,
                h.dropped_frames,
                h.doubled_frames
            );
            self.sink.stop();
            self.publish(ctx, &State::default());
        }
    }

    fn tick(
        &mut self,
        handle: CustodyHandle,
        ctx: &mut dyn Context,
    ) -> Progress {
        let Some(h) = self.in_hand.as_mut().filter(|h| h.handle == handle)
        else {
            return Progress::Done;
        };
        let session = self.link.session();
        if !session.connected || session.generation != h.generation {
            log::info!("snapcast: the server left");
            return Progress::Done;
        }
        if h.state.status == Status::Paused {
            return Progress::Idle(250);
        }
        let since = self.link.since_last_chunk_ms();
        if i128::from(since) > SILENCE_LIMIT.as_millis() as i128 {
            log::info!(
                "snapcast: no chunk for {since} ms ({} received, {} queued)",
                session.chunks,
                self.link.queued()
            );
            return Progress::Failed("the server stopped sending".into());
        }
        let Some(diff_us) = session.diff_us else {
            // No clock yet: nothing can be timed.
            return Progress::Idle(10);
        };
        let rate = i64::from(session.rate.max(1));
        let channels = usize::from(session.channels.max(1));
        let gain = if session.muted {
            0.0
        } else {
            let v = f32::from(session.volume.min(100)) / 100.0;
            v * v
        };
        let progress = loop {
            if h.taken < h.pending.len() {
                let frames = self.sink.write(&h.pending[h.taken..]);
                if frames == 0 {
                    break Progress::Idle(TICK_IDLE_MS);
                }
                h.taken += frames * channels;
                h.frames_out += frames as u64;
                if h.taken < h.pending.len() {
                    continue;
                }
                h.pending.clear();
                h.taken = 0;
            }
            let Some(due_server_us) = self.link.peek_due() else {
                break Progress::Idle(TICK_IDLE_MS);
            };
            // The chunk's time on our clock, the server's buffer and this
            // client's latency applied; against when what the sink holds
            // will have played.
            let play_at = due_server_us - diff_us
                + (session.buffer_ms - session.latency_ms) * 1000;
            let held_us = self.sink.held() as i64 * 1_000_000 / rate;
            let audible_at = self.link.now_us() + held_us;
            let age = audible_at - play_at;
            if age > LATE_US {
                self.link.drop_front();
                h.ages.clear();
                continue;
            }
            if age < -EARLY_US {
                h.ages.clear();
                if held_us < EARLY_US {
                    // The sink is empty and plays silence of its own:
                    // the chunk's time comes round by itself.
                    break Progress::Idle(1);
                }
                // The sink holds sound and the chunk's time is after it:
                // the gap is the server's silence, written as such.
                let gap = (-age).min(PAD_LIMIT_US) as usize * rate as usize
                    / 1_000_000;
                h.pending.clear();
                h.pending.resize(gap.max(1) * channels, 0);
                h.taken = 0;
                continue;
            }
            let Some(chunk) = self.link.pop_into(&mut h.pending) else {
                break Progress::Idle(TICK_IDLE_MS);
            };
            let samples = &mut h.pending;
            let frames = chunk.frames;
            // The lean of what has been written against the server's
            // time: the median age of the last chunks.
            h.ages.push_back(age);
            if h.ages.len() == AGES_KEPT {
                let mut sorted: Vec<i64> = h.ages.iter().copied().collect();
                sorted.sort_unstable();
                let lean = sorted[sorted.len() / 2];
                if lean > HARD_US {
                    // Well behind: the front of the chunk has had its
                    // time.
                    let trim = ((lean * rate / 1_000_000) as usize).min(frames);
                    samples.drain(..trim * channels);
                    h.dropped_frames += trim as u64;
                    h.ages.clear();
                } else if lean.abs() > DEADBAND_US && frames >= GENTLE_SHARE {
                    // A little off: a frame dropped or doubled here and
                    // there, the first frame kept, moves what follows by
                    // up to two percent of this chunk; then the lean is
                    // measured again before the next move.
                    let n = ((lean.abs() * rate / 1_000_000) as usize)
                        .clamp(1, frames / GENTLE_SHARE);
                    let step = frames / n;
                    for i in (0..n).rev() {
                        let at = ((i + 1) * step - 1).min(frames - 1);
                        if lean > 0 {
                            samples.drain(at * channels..(at + 1) * channels);
                        } else {
                            let frame: Vec<i16> = samples
                                [at * channels..(at + 1) * channels]
                                .to_vec();
                            samples.splice(at * channels..at * channels, frame);
                        }
                    }
                    if lean > 0 {
                        h.dropped_frames += n as u64;
                    } else {
                        h.doubled_frames += n as u64;
                    }
                    h.ages.clear();
                } else {
                    h.ages.pop_front();
                }
            }
            if gain < 1.0 {
                for s in samples.iter_mut() {
                    *s = (f32::from(*s) * gain) as i16;
                }
            }
            h.taken = 0;
        };
        h.state.position_ms = h.frames_out * 1000 / rate as u64;
        let state = h.state.clone();
        self.publish(ctx, &state);
        progress
    }
}

/// The codec in use, from the server's header.
enum Codec {
    None,
    Flac(Box<flac::Flac>),
    Pcm,
}

/// Dial the server named on the link, serve the connection, and dial
/// again when it ends, for as long as the device runs.
fn dial_forever(link: &Link, settings: &Settings) {
    let mut failures = 0usize;
    let mut said_waiting = false;
    while !link.0.stop.load(Ordering::Relaxed) {
        let Some(server) = link.server() else {
            if !said_waiting {
                log::info!("snapcast: no server named yet");
                said_waiting = true;
            }
            thread::sleep(Duration::from_millis(500));
            continue;
        };
        said_waiting = false;
        let target = if server.contains(':') {
            server.clone()
        } else {
            format!("{server}:{DEFAULT_PORT}")
        };
        let connected = target
            .to_socket_addrs()
            .map_err(|e| format!("{target}: {e}"))
            .and_then(|mut addrs| {
                addrs.next().ok_or_else(|| format!("{target}: no address"))
            })
            .and_then(|addr| {
                TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
                    .map_err(|e| format!("{target}: {e}"))
            });
        match connected {
            Ok(stream) => {
                failures = 0;
                let generation =
                    link.0.generation.fetch_add(1, Ordering::Relaxed) + 1;
                log::info!(
                    "snapcast: connected to {target} (connection {generation})"
                );
                let why = serve(stream, link, settings, &server, generation);
                log::info!("snapcast: connection {generation} over: {why}");
                if let Ok(mut s) = link.0.session.lock() {
                    if s.generation == generation {
                        s.connected = false;
                    }
                }
                link.clear();
            }
            Err(e) => {
                if failures == 0 {
                    log::warn!("snapcast: {e}; dialling again");
                }
                failures += 1;
            }
        }
        if link.0.stop.load(Ordering::Relaxed) {
            break;
        }
        // The server named may have changed meanwhile: dial it at once.
        let pause = RETRY[failures.min(RETRY.len() - 1)];
        let until = Instant::now() + Duration::from_secs(pause);
        while Instant::now() < until
            && link.server().as_deref() == Some(server.as_str())
        {
            if link.0.stop.load(Ordering::Relaxed) {
                return;
            }
            thread::sleep(Duration::from_millis(200));
        }
    }
}

/// One connection: hello, then the server's messages and our probes
/// until it ends. Returns why.
fn serve(
    stream: TcpStream,
    link: &Link,
    settings: &Settings,
    server: &str,
    generation: u32,
) -> String {
    if let Err(e) = stream.set_nodelay(true) {
        log::debug!("snapcast: nodelay: {e}");
    }
    if let Err(e) = stream.set_read_timeout(Some(READ_TIMEOUT)) {
        return format!("read timeout: {e}");
    }
    let (mut reader, mut writer) = (&stream, &stream);
    let hello = wire::Hello {
        arch: std::env::consts::ARCH.to_string(),
        client_name: "evo-esp32".into(),
        host_name: settings.hostname.clone(),
        id: settings.client_id.clone(),
        instance: 1,
        mac: settings.client_id.clone(),
        os: "evo".into(),
        snap_stream_protocol_version: 2,
        version: settings.version.clone(),
    };
    let payload = match wire::sized_json(&hello) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let mut next_id: u16 = 1;
    if let Err(e) = wire::write_message(
        &mut writer,
        wire::TYPE_HELLO,
        next_id,
        0,
        link.now_us(),
        &payload,
    ) {
        return e;
    }
    next_id = next_id.wrapping_add(1);
    if let Ok(mut s) = link.0.session.lock() {
        *s = Session {
            connected: true,
            server: Some(server.to_string()),
            generation,
            volume: 100,
            ..Session::default()
        };
    }
    link.clear();
    let mut codec = Codec::None;
    let mut diffs: VecDeque<i64> = VecDeque::new();
    let mut last_probe: Option<Instant> = None;
    let mut probes_answered = 0usize;
    // Where the count of decoded frames puts the next chunk, on the
    // server's clock: the first chunk's stamp and the frames since.
    let mut anchor: Option<(i64, u64)> = None;
    let mut out = Vec::new();
    loop {
        if link.0.stop.load(Ordering::Relaxed) {
            return "stopping".into();
        }
        if link.server().as_deref() != Some(server) {
            return "the server named changed".into();
        }
        let interval = if probes_answered < QUICK_PROBES {
            QUICK_PROBE_INTERVAL
        } else {
            settings.probe_interval
        };
        if last_probe.is_none_or(|t| t.elapsed() >= interval) {
            last_probe = Some(Instant::now());
            if let Err(e) = wire::write_message(
                &mut writer,
                wire::TYPE_TIME,
                next_id,
                0,
                link.now_us(),
                &wire::time_payload(),
            ) {
                return e;
            }
            next_id = next_id.wrapping_add(1);
        }
        let inbound = match wire::read_inbound(&mut reader, || link.now_us()) {
            Err(ReadError::Idle) => continue,
            Err(ReadError::Over(why)) => return why,
            Ok(m) => m,
        };
        match inbound {
            Inbound::ServerSettings(ServerSettings {
                buffer_ms,
                latency,
                muted,
                volume,
            }) => {
                log::info!(
                    "snapcast: server settings: buffer {buffer_ms} ms, latency {latency} ms, volume {volume}{}",
                    if muted { ", muted" } else { "" }
                );
                if let Ok(mut s) = link.0.session.lock() {
                    s.buffer_ms = buffer_ms;
                    s.latency_ms = latency;
                    s.muted = muted;
                    s.volume = volume;
                }
            }
            Inbound::CodecHeader {
                codec: name,
                payload,
            } => {
                anchor = None;
                link.clear();
                let (rate, channels, bits) = match name.as_str() {
                    "flac" => match flac::Flac::new(&payload) {
                        Ok(f) => {
                            let (r, c, b) =
                                (f.info.rate, f.info.channels, f.info.bits);
                            codec = Codec::Flac(Box::new(f));
                            (r, c, b)
                        }
                        Err(e) => {
                            log::warn!("snapcast: flac header: {e}");
                            codec = Codec::None;
                            (0, 0, 0)
                        }
                    },
                    "pcm" => match wire::wave_format(&payload) {
                        Ok(w) => {
                            codec = Codec::Pcm;
                            (w.rate, w.channels as u8, w.bits as u8)
                        }
                        Err(e) => {
                            log::warn!("snapcast: pcm header: {e}");
                            codec = Codec::None;
                            (0, 0, 0)
                        }
                    },
                    other => {
                        log::warn!(
                            "snapcast: codec {other} is not played here; set the server's stream to flac or pcm"
                        );
                        codec = Codec::None;
                        (0, 0, 0)
                    }
                };
                log::info!("snapcast: codec {name}, {rate} Hz {channels} ch {bits} bit");
                if let Ok(mut s) = link.0.session.lock() {
                    s.codec = Some(name);
                    s.rate = rate;
                    s.channels = channels;
                    s.bits = bits;
                }
            }
            Inbound::WireChunk { timestamp, payload } => {
                let (rate, channels, bits) = {
                    let s = link.session();
                    (s.rate, usize::from(s.channels.max(1)), s.bits)
                };
                if rate == 0 {
                    continue;
                }
                out.clear();
                let (frames, bad) = match &mut codec {
                    Codec::Flac(f) => f.decode_chunk(&payload, &mut out),
                    Codec::Pcm => {
                        (pcm_to_i16(&payload, bits, channels, &mut out), 0)
                    }
                    Codec::None => continue,
                };
                if bad > 0 {
                    if let Ok(mut s) = link.0.session.lock() {
                        s.bad_frames += bad as u64;
                    }
                }
                if frames == 0 {
                    continue;
                }
                let stamp = timestamp.as_us();
                let (ts0, counted) = match anchor {
                    Some((ts0, counted)) => {
                        let expected =
                            ts0 + counted as i64 * 1_000_000 / i64::from(rate);
                        if (stamp - expected).abs() > REANCHOR_US {
                            log::info!(
                                "snapcast: the stream's time moved by {} ms; counting afresh",
                                (stamp - expected) / 1000
                            );
                            (stamp, 0)
                        } else {
                            (ts0, counted)
                        }
                    }
                    None => (stamp, 0),
                };
                let due_us = ts0 + counted as i64 * 1_000_000 / i64::from(rate);
                anchor = Some((ts0, counted + frames as u64));
                let buffer_ms = link.session().buffer_ms;
                link.push(due_us, frames, &out, rate, channels, buffer_ms);
            }
            Inbound::Time { latency, header } => {
                // The server's clock when it got our probe, less ours
                // when we sent it; our clock when its answer came, less
                // its when it sent. Half the difference is the clocks'
                // offset, the trip cancelled out.
                let c2s = latency.as_us();
                let s2c = header.received.as_us() - header.sent.as_us();
                let diff = (c2s - s2c) / 2;
                diffs.push_back(diff);
                while diffs.len() > DIFFS_KEPT {
                    diffs.pop_front();
                }
                let mut sorted: Vec<i64> = diffs.iter().copied().collect();
                sorted.sort_unstable();
                let median = sorted[sorted.len() / 2];
                probes_answered += 1;
                if let Ok(mut s) = link.0.session.lock() {
                    s.probes = probes_answered as u32;
                    if probes_answered == 1 || probes_answered == QUICK_PROBES {
                        log::info!(
                            "snapcast: clock offset {} ms after {probes_answered} probes (trip {} ms)",
                            median / 1000,
                            (c2s + s2c) / 1000
                        );
                    }
                    s.diff_us = Some(median);
                }
            }
            Inbound::StreamTags(tags) => {
                if !tags.stream.is_empty() {
                    log::info!("snapcast: stream {:?}", tags.stream);
                    if let Ok(mut s) = link.0.session.lock() {
                        s.stream = Some(tags.stream);
                    }
                }
            }
            Inbound::Error {
                code,
                error,
                detail,
            } => {
                return format!(
                    "the server refused: {error} ({code}) {detail}"
                );
            }
            Inbound::Other(kind) => {
                log::debug!("snapcast: message type {kind} passed")
            }
        }
    }
}

/// PCM bytes of `bits` depth into i16 samples; the frames appended.
fn pcm_to_i16(
    bytes: &[u8],
    bits: u8,
    channels: usize,
    out: &mut Vec<i16>,
) -> usize {
    let width = usize::from(bits.max(8) / 8);
    let samples = bytes.len() / width;
    out.reserve(samples);
    for s in bytes.chunks_exact(width) {
        out.push(match width {
            1 => i16::from(s[0] as i8) << 8,
            2 => i16::from_le_bytes([s[0], s[1]]),
            3 => i16::from_le_bytes([s[1], s[2]]),
            _ => i16::from_le_bytes([s[width - 2], s[width - 1]]),
        });
    }
    samples / channels.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_esp32_sdk::catalogue::{Catalogue, Family, Kind, Rack, Shelf};
    use evo_esp32_sdk::projection::ProjectionKey;
    use evo_esp32_sdk::Ticks;
    use evo_esp32_steward::{Config, Steward};
    use std::cell::RefCell;
    use std::net::TcpListener;
    use std::rc::Rc;

    #[test]
    fn the_ring_wraps_round_and_sheds_the_oldest_when_full() {
        // 1000 Hz stereo, a 1 ms buffer and the 2 s slack: 2001 frames.
        let mut q = Queue::default();
        q.fit(1000, 2, 1);
        assert_eq!(q.ring.len(), 2001 * 2);
        let chunk = |v: i16| vec![v; 600 * 2];
        assert_eq!(q.push(0, 600, &chunk(1)), 0);
        assert_eq!(q.push(600, 600, &chunk(2)), 0);
        assert_eq!(q.push(1200, 600, &chunk(3)), 0);
        // A fourth will not fit beside three: the first goes.
        assert_eq!(q.push(1800, 600, &chunk(4)), 1);
        assert_eq!(q.peek_due(), Some(600));
        let mut out = Vec::new();
        let c = q.pop_into(&mut out).unwrap();
        assert_eq!((c.due_us, c.frames), (600, 600));
        assert!(out.iter().all(|&s| s == 2) && out.len() == 1200);
        // The next push wraps round the end of the ring.
        assert_eq!(q.push(2400, 600, &chunk(5)), 0);
        for want in [3, 4, 5] {
            q.pop_into(&mut out).unwrap();
            assert!(out.iter().all(|&s| s == want), "{want}");
        }
        assert_eq!(q.peek_due(), None);
        assert_eq!(q.len, 0);
        // A chunk larger than the ring is refused, counted as dropped.
        assert_eq!(q.push(0, 3000, &vec![0; 6000]), 1);
    }

    fn catalogue() -> Catalogue {
        Catalogue {
            racks: vec![Rack {
                name: shelf::PLAYBACK.into(),
                family: Family::Domain,
                kinds: vec![Kind::Transformer],
                charter: String::new(),
                shelves: vec![Shelf {
                    name: shelf::SNAPCAST.into(),
                    shape: shelf::SHAPE,
                    description: String::new(),
                }],
            }],
        }
    }

    #[derive(Default)]
    struct Tape {
        frames: Vec<i16>,
        starts: Vec<Format>,
        stops: usize,
    }

    #[derive(Clone, Default)]
    struct Shared(Rc<RefCell<Tape>>);

    impl FrameSink for Shared {
        fn start(&mut self, format: &Format) {
            self.0.borrow_mut().starts.push(format.clone());
        }
        fn write(&mut self, interleaved: &[i16]) -> usize {
            self.0.borrow_mut().frames.extend_from_slice(interleaved);
            interleaved.len() / 2
        }
        fn flush(&mut self) {}
        fn stop(&mut self) {
            self.0.borrow_mut().stops += 1;
        }
    }

    fn rig() -> (Steward, Shared, Link) {
        let tape = Shared::default();
        let link = Link::new();
        let mut settings = Settings::new("aa:bb:cc:dd:ee:ff", "bench", "0.1");
        settings.probe_interval = Duration::from_millis(100);
        let mut s = Steward::new(catalogue(), Config::default()).unwrap();
        s.admit_warden(
            Box::new(SnapcastReceiver::new(
                Box::new(tape.clone()),
                settings,
                link.clone(),
            )),
            Ticks(0),
        )
        .unwrap();
        (s, tape, link)
    }

    fn state(s: &Steward) -> State {
        let p = s.projection(ProjectionKey::Rack {
            name: shelf::PLAYBACK.into(),
        });
        from_value(&p.subjects[0].state).unwrap()
    }

    fn wait_until(mut f: impl FnMut() -> bool, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !f() {
            assert!(Instant::now() < deadline, "waited in vain for {what}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// A server whose clock is ours plus `offset_us`, serving PCM
    /// 48 kHz stereo 16-bit with a buffer of `buffer_ms`.
    struct Server {
        stream: TcpStream,
        epoch: Instant,
        offset_us: i64,
    }

    impl Server {
        fn now_us(&self) -> i64 {
            self.epoch.elapsed().as_micros() as i64 + self.offset_us
        }

        fn accept(
            listener: &TcpListener,
            offset_us: i64,
            buffer_ms: i64,
        ) -> Server {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_millis(50)))
                .unwrap();
            let s = Server {
                stream,
                epoch: Instant::now(),
                offset_us,
            };
            // The client's hello first.
            loop {
                match wire::read_inbound(&mut &s.stream, || 0) {
                    Ok(Inbound::Other(wire::TYPE_HELLO)) => break,
                    Ok(other) => panic!("expected hello, got {other:?}"),
                    Err(ReadError::Idle) => continue,
                    Err(ReadError::Over(e)) => panic!("{e}"),
                }
            }
            let settings = wire::sized_json(&ServerSettings {
                buffer_ms,
                latency: 0,
                muted: false,
                volume: 100,
            })
            .unwrap();
            let now = s.now_us();
            wire::write_message(
                &mut &s.stream,
                wire::TYPE_SERVER_SETTINGS,
                1,
                0,
                now,
                &settings,
            )
            .unwrap();
            let mut wav = Vec::new();
            wav.extend_from_slice(b"RIFF");
            wav.extend_from_slice(&36u32.to_le_bytes());
            wav.extend_from_slice(b"WAVEfmt ");
            wav.extend_from_slice(&16u32.to_le_bytes());
            wav.extend_from_slice(&1u16.to_le_bytes());
            wav.extend_from_slice(&2u16.to_le_bytes());
            wav.extend_from_slice(&48_000u32.to_le_bytes());
            wav.extend_from_slice(&(48_000u32 * 4).to_le_bytes());
            wav.extend_from_slice(&4u16.to_le_bytes());
            wav.extend_from_slice(&16u16.to_le_bytes());
            wav.extend_from_slice(b"data");
            wav.extend_from_slice(&0u32.to_le_bytes());
            let mut p = 3u32.to_le_bytes().to_vec();
            p.extend_from_slice(b"pcm");
            p.extend_from_slice(&(wav.len() as u32).to_le_bytes());
            p.extend_from_slice(&wav);
            let now = s.now_us();
            wire::write_message(
                &mut &s.stream,
                wire::TYPE_CODEC_HEADER,
                2,
                0,
                now,
                &p,
            )
            .unwrap();
            s
        }

        /// Answer the probes that came; how many.
        fn answer_probes(&mut self) -> usize {
            let mut n = 0;
            loop {
                match wire::read_inbound(&mut &self.stream, || self.now_us()) {
                    Ok(Inbound::Time { header, .. }) => {
                        // The client's probe: our clock when it came,
                        // less its clock when it went.
                        let received = self.now_us();
                        let c2s = received - header.sent.as_us();
                        let mut p = Vec::new();
                        let tv = wire::Tv::from_us(c2s);
                        p.extend_from_slice(&tv.sec.to_le_bytes());
                        p.extend_from_slice(&tv.usec.to_le_bytes());
                        let now = self.now_us();
                        wire::write_message(
                            &mut &self.stream,
                            wire::TYPE_TIME,
                            0,
                            header.id,
                            now,
                            &p,
                        )
                        .unwrap();
                        n += 1;
                    }
                    Ok(_) => {}
                    Err(ReadError::Idle) => return n,
                    Err(ReadError::Over(e)) => panic!("{e}"),
                }
            }
        }

        /// A chunk of `frames` frames of a ramp starting at `first`,
        /// stamped `at_us` on our clock.
        fn chunk(&mut self, at_us: i64, frames: usize, first: i16) {
            let mut p = Vec::new();
            let tv = wire::Tv::from_us(at_us);
            p.extend_from_slice(&tv.sec.to_le_bytes());
            p.extend_from_slice(&tv.usec.to_le_bytes());
            p.extend_from_slice(&((frames * 4) as u32).to_le_bytes());
            for i in 0..frames {
                let v = first.wrapping_add(i as i16);
                p.extend_from_slice(&v.to_le_bytes());
                p.extend_from_slice(&(-v).to_le_bytes());
            }
            let now = self.now_us();
            wire::write_message(
                &mut &self.stream,
                wire::TYPE_WIRE_CHUNK,
                0,
                0,
                now,
                &p,
            )
            .unwrap();
        }
    }

    fn receive(s: &mut Steward, link: &Link) -> CustodyHandle {
        let r = link.sending().expect("a server sending");
        s.assign(
            &shelf::playback_snapcast(),
            Assignment {
                subject: SubjectId::new("playback/snapcast"),
                instruction: to_value(&r),
            },
            Ticks(0),
        )
        .unwrap()
    }

    #[test]
    fn the_client_dials_probes_the_clock_and_plays_chunks_at_the_servers_time()
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (mut s, tape, link) = rig();
        link.set_server(Some(format!("127.0.0.1:{port}")));
        // The server's clock runs 1 s ahead of ours; the buffer is 150 ms.
        let mut server = Server::accept(&listener, 1_000_000, 150);
        wait_until(
            || link.session().connected && link.session().rate == 48_000,
            "the codec",
        );
        // Probes come quickly at first; answer them until the quick ones
        // are in, which is when the clock is trusted for a hand-over.
        wait_until(
            || {
                server.answer_probes();
                link.session().probes as usize >= QUICK_PROBES
            },
            "the offset",
        );
        // The server's clock is its epoch plus a second, the client's is
        // the link's epoch: the difference the probes should find is
        // between those two clocks, not the second alone (the dialler
        // may sleep half a second before it sees the server named, and
        // the server's epoch is set at accept).
        let diff = link.session().diff_us.unwrap();
        let expected = server.now_us() - link.now_us();
        assert!(
            (diff - expected).abs() <= 10_000,
            "offset {diff} us, the clocks are {expected} us apart"
        );
        assert_eq!(link.session().buffer_ms, 150);

        // Two chunks stamped now and 20 ms on, on the server's clock:
        // due 150 ms from now on ours.
        let at = server.now_us();
        server.chunk(at, 960, 0);
        server.chunk(at + 20_000, 960, 960);
        wait_until(|| link.queued() == 2, "two chunks");
        receive(&mut s, &link);
        assert_eq!(state(&s).status, Status::Playing);
        s.tick(Ticks(1000));
        assert!(tape.0.borrow().frames.is_empty(), "nothing before its time");
        thread::sleep(Duration::from_millis(160));
        s.tick(Ticks(2000));
        {
            let t = tape.0.borrow();
            assert_eq!(t.starts.len(), 1);
            assert_eq!(
                (t.starts[0].sample_rate, t.starts[0].channels),
                (48_000, 2)
            );
            // A few milliseconds late by the test's own sleeping, so a
            // frame in fifty may have gone (never the first, never out
            // of order), and the second chunk may be due as well.
            assert!(
                t.frames.len() >= 1920 - 2 * 1920 / GENTLE_SHARE,
                "{} samples",
                t.frames.len()
            );
            assert_eq!(t.frames[0], 0);
            assert_eq!(t.frames[1], 0);
            assert!(t.frames[2] == 1 && t.frames[3] == -1);
        }
        thread::sleep(Duration::from_millis(30));
        s.tick(Ticks(3000));
        assert!(
            tape.0.borrow().frames.len()
                >= 2 * (1920 - 2 * 1920 / GENTLE_SHARE)
        );
        assert!(state(&s).position_ms >= 39);
        drop(server);
        wait_until(|| !link.session().connected, "the server gone");
        s.tick(Ticks(4000));
        assert_eq!(state(&s).status, Status::Stopped);
        assert_eq!(tape.0.borrow().stops, 1);
    }

    #[test]
    fn a_chunk_long_past_its_time_is_dropped_and_the_volume_is_the_servers() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (mut s, tape, link) = rig();
        link.set_server(Some(format!("127.0.0.1:{port}")));
        let mut server = Server::accept(&listener, 0, 100);
        wait_until(
            || {
                server.answer_probes();
                link.session().probes as usize >= QUICK_PROBES
                    && link.session().rate == 48_000
            },
            "the offset",
        );
        // Half the volume.
        let settings = wire::sized_json(&ServerSettings {
            buffer_ms: 100,
            latency: 0,
            muted: false,
            volume: 50,
        })
        .unwrap();
        let now = server.now_us();
        wire::write_message(
            &mut &server.stream,
            wire::TYPE_SERVER_SETTINGS,
            9,
            0,
            now,
            &settings,
        )
        .unwrap();
        wait_until(|| link.session().volume == 50, "the volume");
        let at = server.now_us();
        // One chunk a second late, one due now.
        server.chunk(at - 1_100_000, 960, 7);
        server.chunk(at - 100_000, 960, 1000);
        wait_until(|| link.queued() == 2, "two chunks");
        receive(&mut s, &link);
        s.tick(Ticks(1000));
        let t = tape.0.borrow();
        assert_eq!(link.session().dropped, 1, "the late chunk went");
        assert!(!t.frames.is_empty());
        // 1000 at a quarter (volume 50 squared) is 250; the first frame
        // is never the one dropped.
        assert_eq!(t.frames[0], 250);
    }

    #[test]
    fn no_server_named_is_no_connection_and_a_name_dials() {
        let (_s, _tape, link) = rig();
        thread::sleep(Duration::from_millis(50));
        assert!(!link.session().connected);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        link.set_server(Some(format!("127.0.0.1:{port}")));
        let _server = Server::accept(&listener, 0, 100);
        wait_until(|| link.session().connected, "the dial");
        assert!(
            link.sending().is_none(),
            "nothing to hand over before a chunk"
        );
    }

    #[test]
    fn pcm_widths_come_out_as_i16() {
        let mut out = Vec::new();
        assert_eq!(pcm_to_i16(&[0x34, 0x12, 0x00, 0x80], 16, 2, &mut out), 1);
        assert_eq!(out, [0x1234, i16::MIN]);
        out.clear();
        assert_eq!(
            pcm_to_i16(&[0xff, 0x34, 0x12, 0x00, 0x00, 0x80], 24, 2, &mut out),
            1
        );
        assert_eq!(out, [0x1234, i16::MIN]);
        out.clear();
        assert_eq!(pcm_to_i16(&[0x7f, 0x80], 8, 2, &mut out), 1);
        assert_eq!(out, [0x7f00, i16::MIN]);
    }
}

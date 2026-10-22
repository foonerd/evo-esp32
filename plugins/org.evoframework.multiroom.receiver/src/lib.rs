// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! The device as one room of an Evo multiroom group.
//!
//! `org.evoframework.multiroom.receiver` stocks `playback/receive`. On
//! the Evo audio plane the group's elected source-host dials each
//! member it discovers (`_evo._tcp`, the device id in the TXT record,
//! the port in the SRV) and streams it 20 ms frames of PCM stamped with
//! the source-host's clock. This plugin is the member's end: a thread
//! listens on the port from `load` on, answers the handshake, keeps the
//! connection alive with the sync probes the plane expects of a
//! receiver, and queues the frames with their arrival time. The
//! distribution watches the [`Link`] it was given; when a source-host
//! is sending it hands this warden the sink under a [`Receive`]
//! custody, and the warden plays each frame at its presentation time:
//! anchored on the first frame's arrival plus the same lead the Linux
//! receiver uses, so every room of the group lands together. When the
//! source-host says goodbye, drops the connection or falls silent, the
//! custody ends and the distribution takes the sink back.
//!
//! Frames come as `bin1` from a source-host of this version (offered in
//! the handshake), or as the JSON envelope from an older one, which is
//! decoded all the same. Everything else on the plane — the witness
//! chain, presence relays — passes unread. The device never claims to
//! be a source-host and never dials.
#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
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

pub mod wire;
use wire::{Frame, Inbound, Message, ReadError};

/// The plugin's name, as the catalogue and the steward know it.
pub const NAME: &str = "org.evoframework.multiroom.receiver";
/// The audio plane's port, as the framework advertises it.
pub const CONTROL_PORT: u16 = 7331;
/// How far behind its presentation time a frame plays: the Linux
/// receiver's default, so rooms land together.
pub const LEADER_MS: u64 = 200;
/// How often a receiver probes the source-host's clock; the plane reaps
/// a connection silent for two minutes, and this keeps it from that.
pub const PROBE_INTERVAL: Duration = Duration::from_secs(5);
/// The most frames held unplayed: five seconds of 20 ms frames. Past
/// this the oldest go, as on the Linux receiver.
const MAX_QUEUED: usize = 250;
/// A source-host silent this long with a custody open has stopped.
const SILENCE_LIMIT: Duration = Duration::from_secs(5);
/// The render tick, as on the Linux receiver.
const TICK_IDLE_MS: u32 = 5;
/// How long the listener's accept waits between looks at the stop flag.
const ACCEPT_POLL: Duration = Duration::from_millis(200);
/// The connection's read timeout: how often the probe timer is checked.
const READ_TIMEOUT: Duration = Duration::from_millis(250);
/// How long a peer has to say hello.
const HELLO_LIMIT: Duration = Duration::from_secs(5);
/// Thread stacks; placed by the distribution (PSRAM on the board).
const LISTEN_STACK: usize = 8 * 1024;
const SERVE_STACK: usize = 24 * 1024;

static STOCKS: [Stocking; 1] = [Stocking {
    rack: shelf::PLAYBACK,
    shelf: shelf::RECEIVE,
    shape: shelf::SHAPE,
}];

/// What the distribution tells the receiver about itself.
#[derive(Debug, Clone)]
pub struct Settings {
    /// This device's id on the plane: the UUID the source-host knows it
    /// by (the one in the `_evo._tcp` TXT record).
    pub device_id: String,
    /// The framework version to announce.
    pub version: String,
    /// The port to listen on; [`CONTROL_PORT`] unless testing.
    pub port: u16,
    /// How far behind presentation time to play; [`LEADER_MS`].
    pub leader_ms: u64,
    /// How often to probe; [`PROBE_INTERVAL`].
    pub probe_interval: Duration,
}

impl Settings {
    /// The plane's defaults for `device_id`.
    pub fn new(
        device_id: impl Into<String>,
        version: impl Into<String>,
    ) -> Self {
        Self {
            device_id: device_id.into(),
            version: version.into(),
            port: CONTROL_PORT,
            leader_ms: LEADER_MS,
            probe_interval: PROBE_INTERVAL,
        }
    }
}

/// A frame as queued: when it arrived, and what it is.
#[derive(Debug, Clone)]
pub struct Queued {
    /// When it came off the wire.
    pub arrived: Instant,
    /// The frame.
    pub frame: Frame,
}

/// The connection as the distribution sees it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Session {
    /// A source-host is connected.
    pub connected: bool,
    /// Its device id.
    pub leader: Option<String>,
    /// The group it is sending for, once known (its hello's claim, or
    /// the first frame's).
    pub group: Option<String>,
    /// Frames received on this connection.
    pub frames: u64,
    /// Frames dropped unplayed because the queue was full.
    pub dropped: u64,
    /// The source-host's clock less ours, from the last sync exchange.
    pub offset_ms: Option<i64>,
    /// The source-host sends the JSON envelope (an older version).
    pub json_frames: bool,
    /// Counts connections; a custody is for one of them.
    pub generation: u32,
}

struct Shared {
    session: Mutex<Session>,
    queue: Mutex<VecDeque<Queued>>,
    /// When the last frame arrived, as milliseconds on `epoch`
    /// (32 bits: the target has no 64-bit atomics; wraps in 49 days,
    /// and the comparisons wrap with it).
    last_frame_ms: AtomicU32,
    epoch: Instant,
    generation: AtomicU32,
    stop: AtomicBool,
    /// The port actually bound (a test asks for 0).
    port: AtomicU32,
}

/// The receiver's connection and frames, shared between its threads,
/// the warden and the distribution. Cheap to clone.
#[derive(Clone)]
pub struct Link(Arc<Shared>);

impl Default for Link {
    fn default() -> Self {
        Self::new()
    }
}

impl Link {
    /// A link with no connection yet.
    pub fn new() -> Self {
        Link(Arc::new(Shared {
            session: Mutex::new(Session::default()),
            queue: Mutex::new(VecDeque::new()),
            last_frame_ms: AtomicU32::new(0),
            epoch: Instant::now(),
            generation: AtomicU32::new(0),
            stop: AtomicBool::new(false),
            port: AtomicU32::new(0),
        }))
    }

    /// The connection now.
    pub fn session(&self) -> Session {
        self.0.session.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// A source-host is connected and has sent a frame in the last two
    /// seconds: the distribution should hand over the sink. The group
    /// and the leader, for the [`Receive`] instruction.
    pub fn sending(&self) -> Option<Receive> {
        let s = self.session();
        if !s.connected || s.frames == 0 {
            return None;
        }
        if self.since_last_frame_ms() > 2000 {
            return None;
        }
        Some(Receive {
            group: s.group?,
            leader: s.leader?,
        })
    }

    /// Frames queued and not yet played.
    pub fn queued(&self) -> usize {
        self.0.queue.lock().map(|q| q.len()).unwrap_or(0)
    }

    /// The port the listener bound, once it has.
    pub fn port(&self) -> u16 {
        self.0.port.load(Ordering::Relaxed) as u16
    }

    fn now_ms(&self) -> u32 {
        self.0.epoch.elapsed().as_millis() as u32
    }

    /// Milliseconds since the last frame arrived. The stamp is read
    /// before the clock: the listener's thread may store a newer stamp
    /// between the two, and that is no silence, not a wrap.
    fn since_last_frame_ms(&self) -> i32 {
        let last = self.0.last_frame_ms.load(Ordering::Relaxed);
        self.now_ms().wrapping_sub(last) as i32
    }

    fn push(&self, frame: Frame) {
        let now = self.now_ms();
        if let Ok(mut q) = self.0.queue.lock() {
            if q.len() >= MAX_QUEUED {
                q.pop_front();
                if let Ok(mut s) = self.0.session.lock() {
                    s.dropped += 1;
                }
            }
            q.push_back(Queued {
                arrived: Instant::now(),
                frame,
            });
        }
        self.0.last_frame_ms.store(now, Ordering::Relaxed);
        if let Ok(mut s) = self.0.session.lock() {
            s.frames += 1;
        }
    }

    fn pop(&self) -> Option<Queued> {
        self.0.queue.lock().ok()?.pop_front()
    }

    fn peek_due(&self) -> Option<(Instant, u64)> {
        let q = self.0.queue.lock().ok()?;
        q.front().map(|f| (f.arrived, f.frame.presentation_time_ms))
    }

    fn clear(&self) {
        if let Ok(mut q) = self.0.queue.lock() {
            q.clear();
        }
    }
}

/// The receiver warden.
pub struct MultiroomReceiver {
    sink: Box<dyn FrameSink>,
    settings: Settings,
    link: Link,
    subject: Option<SubjectId>,
    in_hand: Option<InHand>,
}

/// The custody in hand: the session it is for and the schedule.
struct InHand {
    handle: CustodyHandle,
    state: State,
    generation: u32,
    /// The first frame's arrival and presentation time: the schedule's
    /// anchor.
    anchor: Option<(Instant, u64)>,
    /// Frames the sink took.
    frames_out: u64,
    /// Samples of the frame in hand the sink has not taken yet.
    pending: Vec<i16>,
    taken: usize,
}

impl MultiroomReceiver {
    /// A receiver writing to `sink`, reporting through `link`.
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

impl Plugin for MultiroomReceiver {
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
                addressing: ExternalAddressing::new("playback", "room"),
                kind: KIND.into(),
                state: to_value(&State::default()),
            })
            .map_err(|e| LoadError(format!("announcing: {e}")))?;
        self.subject = Some(id);
        let listener = TcpListener::bind(SocketAddr::from((
            Ipv4Addr::UNSPECIFIED,
            self.settings.port,
        )))
        .map_err(|e| LoadError(format!("port {}: {e}", self.settings.port)))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| LoadError(format!("listener: {e}")))?;
        let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
        self.link.0.port.store(u32::from(port), Ordering::Relaxed);
        let (link, settings) = (self.link.clone(), self.settings.clone());
        evo_esp32_sdk::thread::spawn(
            evo_esp32_sdk::thread::Role::Feed,
            "room",
            LISTEN_STACK,
            move || listen(listener, link, settings),
        )
        .map_err(|e| LoadError(format!("listen thread: {e}")))?;
        log::info!(
            "multiroom: listening on port {port} as {}",
            self.settings.device_id
        );
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

impl Warden for MultiroomReceiver {
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
            || session.leader.as_deref() != Some(&receive.leader)
        {
            return Err(CustodyError("no such source-host connected".into()));
        }
        let short = |s: &str| s.chars().take(8).collect::<String>();
        let state = State {
            status: Status::Playing,
            track: Some(Track {
                uri: format!("evo://room/{}", receive.group),
                title: "Evo multiroom".into(),
                artist: Some(format!("from {}", short(&receive.leader))),
                album: Some(format!("Group {}", short(&receive.group))),
                ..Track::default()
            }),
            position_ms: 0,
            format: None,
        };
        self.publish(ctx, &state);
        self.in_hand = Some(InHand {
            handle,
            state,
            generation: session.generation,
            anchor: None,
            frames_out: 0,
            pending: Vec::new(),
            taken: 0,
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
                // What piled up while held is stale; the schedule
                // starts again from the next frame.
                self.link.clear();
                h.anchor = None;
                h.pending.clear();
                h.taken = 0;
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
        if self.in_hand.as_ref().is_some_and(|h| h.handle == handle) {
            self.in_hand = None;
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
            log::info!("multiroom: the source-host left");
            return Progress::Done;
        }
        if h.state.status == Status::Paused {
            return Progress::Idle(250);
        }
        if i128::from(self.link.since_last_frame_ms())
            > SILENCE_LIMIT.as_millis() as i128
        {
            return Progress::Failed("the source-host stopped sending".into());
        }
        let progress = loop {
            // The frame in hand first.
            if h.taken < h.pending.len() {
                let channels = h
                    .state
                    .format
                    .as_ref()
                    .map_or(2, |f| usize::from(f.channels).max(1));
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
            // Is the next frame due? Due means its presentation time,
            // anchored on the first frame's arrival and the lead,
            // minus what the sink already holds ahead of the ear.
            let Some((arrived, pts)) = self.link.peek_due() else {
                break Progress::Idle(TICK_IDLE_MS);
            };
            let (anchor_at, anchor_pts) =
                *h.anchor.get_or_insert((arrived, pts));
            let due = anchor_at
                + Duration::from_millis(pts.saturating_sub(anchor_pts))
                + Duration::from_millis(self.settings.leader_ms);
            let rate =
                h.state.format.as_ref().map_or(48_000, |f| f.sample_rate);
            let held_ms =
                self.sink.held() as u64 * 1000 / u64::from(rate.max(1));
            let audible_at = Instant::now() + Duration::from_millis(held_ms);
            if due > audible_at {
                break Progress::Idle(TICK_IDLE_MS);
            }
            let Some(q) = self.link.pop() else {
                break Progress::Idle(TICK_IDLE_MS);
            };
            let f = q.frame;
            if f.codec != "pcm_s16_le" {
                return Progress::Failed(format!(
                    "codec {} is not played here",
                    f.codec
                ));
            }
            let format = Format {
                codec: "pcm".into(),
                sample_rate: f.rate_hz,
                bits: 16,
                channels: u8::try_from(f.channels).unwrap_or(2).max(1),
            };
            if h.state.format.as_ref() != Some(&format) {
                self.sink.start(&format);
                h.state.format = Some(format);
            }
            h.pending = f
                .payload
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]]))
                .collect();
            h.taken = 0;
        };
        let rate = h.state.format.as_ref().map_or(48_000, |f| f.sample_rate);
        h.state.position_ms = h.frames_out * 1000 / u64::from(rate.max(1));
        let state = h.state.clone();
        self.publish(ctx, &state);
        progress
    }
}

/// The listener: accept a connection, serve it on a thread of its own.
/// A second source-host connecting supersedes the first (the elected
/// source-host changed, or it restarted and dialled again).
fn listen(listener: TcpListener, link: Link, settings: Settings) {
    while !link.0.stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, from)) => {
                let generation =
                    link.0.generation.fetch_add(1, Ordering::Relaxed) + 1;
                log::info!(
                    "multiroom: {from} connected (connection {generation})"
                );
                let (link, settings) = (link.clone(), settings.clone());
                let spawned = evo_esp32_sdk::thread::spawn(
                    evo_esp32_sdk::thread::Role::Feed,
                    "room-link",
                    SERVE_STACK,
                    move || {
                        let why = serve(stream, &link, &settings, generation);
                        log::info!(
                            "multiroom: connection {generation} over: {why}"
                        );
                        let mut s = link
                            .0
                            .session
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        if s.generation == generation {
                            s.connected = false;
                        }
                        drop(s);
                        if link.0.generation.load(Ordering::Relaxed)
                            == generation
                        {
                            link.clear();
                        }
                    },
                );
                if let Err(e) = spawned {
                    log::warn!("multiroom: no thread for the connection: {e}");
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL);
            }
            Err(e) => {
                log::warn!("multiroom: accept: {e}");
                thread::sleep(ACCEPT_POLL);
            }
        }
    }
}

/// One connection, start to end: the handshake, then frames and
/// messages until the peer leaves or a newer connection supersedes
/// this one. Returns why it ended.
fn serve(
    stream: TcpStream,
    link: &Link,
    settings: &Settings,
    generation: u32,
) -> String {
    if let Err(e) = stream.set_nodelay(true) {
        log::debug!("multiroom: nodelay: {e}");
    }
    if let Err(e) = stream.set_read_timeout(Some(READ_TIMEOUT)) {
        return format!("read timeout: {e}");
    }
    // One socket, read and written from this thread by reference: the
    // target's sockets cannot be duplicated, and nothing here needs
    // them to be.
    let (mut reader, mut writer) = (&stream, &stream);
    // Both sides send their hello first, then read the other's.
    let hello = Message::Hello {
        device_id: settings.device_id.clone(),
        framework_version: settings.version.clone(),
        claimed_source_host_groups: Vec::new(),
        formats: vec![wire::FRAME_FORMAT_BIN1.to_string()],
    };
    if let Err(e) = wire::write_message(&mut writer, &hello) {
        return e;
    }
    let started = Instant::now();
    let (leader, claimed) = loop {
        if started.elapsed() > HELLO_LIMIT {
            return "no hello".into();
        }
        match wire::read_inbound(&mut reader) {
            Ok(Inbound::Message(Message::Hello {
                device_id,
                claimed_source_host_groups,
                formats,
                ..
            })) => {
                log::info!(
                    "multiroom: hello from {device_id}, groups {claimed_source_host_groups:?}, formats {formats:?}"
                );
                break (device_id, claimed_source_host_groups);
            }
            Ok(other) => return format!("expected hello, got {other:?}"),
            Err(ReadError::Idle) => continue,
            Err(ReadError::Over(why)) => return why,
        }
    };
    {
        let mut s = link.0.session.lock().unwrap_or_else(|e| e.into_inner());
        *s = Session {
            connected: true,
            leader: Some(leader.clone()),
            group: claimed.first().cloned(),
            generation,
            ..Session::default()
        };
    }
    link.clear();
    let mut last_probe = Instant::now();
    let mut probed_at: Option<u64> = None;
    let mut json_said = false;
    loop {
        if link.0.stop.load(Ordering::Relaxed) {
            return "stopping".into();
        }
        if link.0.generation.load(Ordering::Relaxed) != generation {
            return "superseded".into();
        }
        if last_probe.elapsed() >= settings.probe_interval {
            last_probe = Instant::now();
            let group = link.session().group;
            if let Some(group_id) = group {
                let t1_ms = u64::from(link.now_ms());
                probed_at = Some(t1_ms);
                let probe = Message::SyncProbe { group_id, t1_ms };
                if let Err(e) = wire::write_message(&mut writer, &probe) {
                    return e;
                }
            }
        }
        match wire::read_inbound(&mut reader) {
            Err(ReadError::Idle) => continue,
            Err(ReadError::Over(why)) => return why,
            Ok(Inbound::Frame(frame)) => {
                if link.session().group.is_none() {
                    if let Ok(mut s) = link.0.session.lock() {
                        s.group = Some(frame.group_id.clone());
                    }
                }
                link.push(frame);
            }
            Ok(Inbound::Message(msg)) => match msg {
                Message::AudioFrame {
                    group_id,
                    sequence,
                    presentation_time_ms,
                    codec,
                    rate_hz,
                    channels,
                    payload_b64,
                } => {
                    if !json_said {
                        json_said = true;
                        log::info!("multiroom: {leader} sends frames as JSON (an older version); decoding them");
                        if let Ok(mut s) = link.0.session.lock() {
                            s.json_frames = true;
                        }
                    }
                    match Frame::from_json(
                        group_id,
                        sequence,
                        presentation_time_ms,
                        codec,
                        rate_hz,
                        channels,
                        &payload_b64,
                    ) {
                        Ok(frame) => {
                            if link.session().group.is_none() {
                                if let Ok(mut s) = link.0.session.lock() {
                                    s.group = Some(frame.group_id.clone());
                                }
                            }
                            link.push(frame);
                        }
                        Err(e) => log::warn!("multiroom: {e}"),
                    }
                }
                Message::SyncResponse { t1_ms, t2_ms, .. } => {
                    if probed_at == Some(t1_ms) {
                        let t3 = u64::from(link.now_ms());
                        // The source-host's clock less ours, the trip
                        // halved out.
                        let offset = t2_ms as i64 - ((t1_ms + t3) / 2) as i64;
                        if let Ok(mut s) = link.0.session.lock() {
                            s.offset_ms = Some(offset);
                        }
                    }
                }
                Message::Heartbeat { groups, .. } => {
                    if let (Some(g), Ok(mut s)) =
                        (groups.first(), link.0.session.lock())
                    {
                        if s.group.is_none() {
                            s.group = Some(g.clone());
                        }
                    }
                }
                Message::Goodbye { .. } => return "goodbye".into(),
                Message::Hello { .. } => return "a second hello".into(),
                Message::SyncProbe { .. } | Message::Other => {}
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_esp32_sdk::catalogue::{Catalogue, Family, Kind, Rack, Shelf};
    use evo_esp32_sdk::projection::ProjectionKey;
    use evo_esp32_sdk::Ticks;
    use evo_esp32_steward::{Config, Steward};
    use std::cell::RefCell;
    use std::io::Write as _;
    use std::rc::Rc;

    fn catalogue() -> Catalogue {
        Catalogue {
            racks: vec![Rack {
                name: shelf::PLAYBACK.into(),
                family: Family::Domain,
                kinds: vec![Kind::Transformer],
                charter: String::new(),
                shelves: vec![Shelf {
                    name: shelf::RECEIVE.into(),
                    shape: shelf::SHAPE,
                    description: String::new(),
                }],
            }],
        }
    }

    /// A sink that records what it is given.
    #[derive(Default)]
    struct Tape {
        frames: Vec<i16>,
        starts: Vec<Format>,
        stops: usize,
        held: usize,
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
        fn held(&self) -> usize {
            self.0.borrow().held
        }
    }

    fn rig(leader_ms: u64) -> (Steward, Shared, Link) {
        let tape = Shared::default();
        let link = Link::new();
        let mut settings =
            Settings::new("ffffffff-0000-4000-8000-000000000001", "0.1.0");
        settings.port = 0;
        settings.leader_ms = leader_ms;
        settings.probe_interval = Duration::from_millis(50);
        let mut s = Steward::new(catalogue(), Config::default()).unwrap();
        s.admit_warden(
            Box::new(MultiroomReceiver::new(
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

    /// A source-host of this version: connects, says hello, sends what
    /// it is told to.
    struct Host {
        stream: TcpStream,
        their_hello: Message,
    }

    impl Host {
        fn dial(port: u16, formats: Vec<String>) -> Host {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            wire::write_message(
                &mut stream,
                &Message::Hello {
                    device_id: "00000000-0000-4000-8000-00000000aaaa".into(),
                    framework_version: "0.1.13".into(),
                    claimed_source_host_groups: vec!["g1".into()],
                    formats,
                },
            )
            .unwrap();
            let their_hello = match wire::read_inbound(&mut stream).unwrap() {
                Inbound::Message(m) => m,
                other => panic!("{other:?}"),
            };
            Host {
                stream,
                their_hello,
            }
        }

        fn frame(&self, sequence: u64, pts: u64) -> Frame {
            let samples: Vec<u8> = (0..960u32)
                .flat_map(|i| {
                    let v = (sequence as i16)
                        .wrapping_mul(100)
                        .wrapping_add(i as i16);
                    [v.to_le_bytes(), (-v).to_le_bytes()].concat()
                })
                .collect();
            Frame {
                group_id: "g1".into(),
                sequence,
                presentation_time_ms: pts,
                codec: "pcm_s16_le".into(),
                rate_hz: 48_000,
                channels: 2,
                payload: samples,
            }
        }

        fn send_bin1(&mut self, f: &Frame) {
            wire::write_body(&mut self.stream, &f.to_bin1()).unwrap();
        }

        fn send_json(&mut self, f: &Frame) {
            use base64::engine::general_purpose::STANDARD;
            use base64::Engine as _;
            wire::write_message(
                &mut self.stream,
                &Message::AudioFrame {
                    group_id: f.group_id.clone(),
                    sequence: f.sequence,
                    presentation_time_ms: f.presentation_time_ms,
                    codec: f.codec.clone(),
                    rate_hz: f.rate_hz,
                    channels: f.channels,
                    payload_b64: STANDARD.encode(&f.payload),
                },
            )
            .unwrap();
        }

        /// Wait for a message from the receiver (its probe).
        fn next_message(&mut self) -> Message {
            loop {
                match wire::read_inbound(&mut self.stream) {
                    Ok(Inbound::Message(m)) => return m,
                    Ok(Inbound::Frame(f)) => {
                        panic!("a receiver sent a frame: {f:?}")
                    }
                    Err(ReadError::Idle) => continue,
                    Err(ReadError::Over(e)) => panic!("{e}"),
                }
            }
        }
    }

    fn wait_until(mut f: impl FnMut() -> bool, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !f() {
            assert!(Instant::now() < deadline, "waited in vain for {what}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn receive(s: &mut Steward, link: &Link) -> CustodyHandle {
        let r = link.sending().expect("a source-host sending");
        s.assign(
            &shelf::playback_receive(),
            Assignment {
                subject: SubjectId::new("playback/room"),
                instruction: to_value(&r),
            },
            Ticks(0),
        )
        .unwrap()
    }

    #[test]
    fn a_source_host_connects_sends_and_its_frames_play_in_order_at_their_time()
    {
        let (mut s, tape, link) = rig(100);
        let mut host = Host::dial(link.port(), vec!["bin1".into()]);
        assert!(matches!(
            host.their_hello,
            Message::Hello { ref formats, ref claimed_source_host_groups, .. }
                if formats == &["bin1"] && claimed_source_host_groups.is_empty()
        ));
        wait_until(|| link.session().connected, "the session");
        assert_eq!(
            link.session().leader.as_deref(),
            Some("00000000-0000-4000-8000-00000000aaaa")
        );
        assert_eq!(link.session().group.as_deref(), Some("g1"));
        // Nothing to hand over until frames flow.
        assert!(link.sending().is_none());
        for i in 0..3 {
            let f = host.frame(i, i * 20);
            host.send_bin1(&f);
        }
        wait_until(|| link.queued() == 3, "three frames");
        let r = link.sending().unwrap();
        assert_eq!(
            (r.group.as_str(), r.leader.as_str()),
            ("g1", "00000000-0000-4000-8000-00000000aaaa")
        );
        receive(&mut s, &link);
        assert_eq!(state(&s).status, Status::Playing);
        // Straight away nothing plays: the first frame is due 100 ms
        // after it arrived.
        s.tick(Ticks(1000));
        assert!(tape.0.borrow().frames.is_empty());
        thread::sleep(Duration::from_millis(110));
        s.tick(Ticks(2000));
        {
            let t = tape.0.borrow();
            assert_eq!(t.starts.len(), 1);
            assert_eq!(
                (
                    t.starts[0].sample_rate,
                    t.starts[0].channels,
                    t.starts[0].bits
                ),
                (48_000, 2, 16)
            );
            // Frame 0 is out; frame 1 is due 20 ms later and the sink
            // holds nothing, so not yet.
            assert_eq!(t.frames.len(), 1920, "{} samples", t.frames.len());
            assert_eq!(t.frames[0], 0);
            assert_eq!(t.frames[2], 1);
        }
        thread::sleep(Duration::from_millis(50));
        s.tick(Ticks(3000));
        assert_eq!(tape.0.borrow().frames.len(), 3 * 1920);
        assert_eq!(
            tape.0.borrow().frames[1920],
            100,
            "frame 1 follows frame 0"
        );
        assert_eq!(state(&s).position_ms, 60);
        // The source-host leaves; the custody ends.
        wire::write_message(
            &mut host.stream,
            &Message::Goodbye {
                device_id: "x".into(),
            },
        )
        .unwrap();
        wait_until(|| !link.session().connected, "the goodbye");
        s.tick(Ticks(4000));
        assert_eq!(state(&s).status, Status::Stopped);
        assert_eq!(tape.0.borrow().stops, 1);
    }

    #[test]
    fn the_receiver_probes_the_clock_and_takes_an_old_hosts_json_frames() {
        let (mut s, tape, link) = rig(0);
        let mut host = Host::dial(link.port(), vec![]);
        wait_until(|| link.session().connected, "the session");
        // The probe arrives on the host side within the interval.
        let probe = host.next_message();
        let t1 = match probe {
            Message::SyncProbe { group_id, t1_ms } => {
                assert_eq!(group_id, "g1");
                t1_ms
            }
            other => panic!("{other:?}"),
        };
        wire::write_message(
            &mut host.stream,
            &Message::SyncResponse {
                group_id: "g1".into(),
                t1_ms: t1,
                t2_ms: t1 + 1_000_000,
            },
        )
        .unwrap();
        wait_until(|| link.session().offset_ms.is_some(), "the offset");
        let off = link.session().offset_ms.unwrap();
        assert!((999_000..=1_000_001).contains(&off), "{off}");
        // JSON frames from an older source-host decode the same.
        let f = host.frame(5, 0);
        host.send_json(&f);
        wait_until(|| link.queued() == 1, "the frame");
        assert!(link.session().json_frames);
        receive(&mut s, &link);
        s.tick(Ticks(1000));
        assert_eq!(tape.0.borrow().frames.len(), 1920);
        assert_eq!(tape.0.borrow().frames[0], 500);
    }

    #[test]
    fn a_second_connection_supersedes_the_first_and_the_custody_ends() {
        let (mut s, _tape, link) = rig(0);
        let mut first = Host::dial(link.port(), vec!["bin1".into()]);
        wait_until(|| link.session().connected, "the first");
        first.send_bin1(&first.frame(0, 0));
        wait_until(|| link.queued() == 1, "a frame");
        receive(&mut s, &link);
        let _second = Host::dial(link.port(), vec!["bin1".into()]);
        wait_until(|| link.session().generation == 2, "the second");
        s.tick(Ticks(1000));
        assert_eq!(
            state(&s).status,
            Status::Stopped,
            "the custody for the first is over"
        );
        assert!(link.session().connected, "the second is connected");
    }

    #[test]
    fn a_silent_source_host_ends_the_custody_with_a_reason() {
        let (mut s, _tape, link) = rig(0);
        let mut host = Host::dial(link.port(), vec!["bin1".into()]);
        wait_until(|| link.session().connected, "the session");
        host.send_bin1(&host.frame(0, 0));
        wait_until(|| link.queued() == 1, "a frame");
        receive(&mut s, &link);
        s.tick(Ticks(1000));
        // Nothing more arrives. The link's clock is real: the silence
        // limit is reached by setting the last frame's time back to the
        // epoch and letting the epoch be far enough behind.
        link.0.last_frame_ms.store(0, Ordering::Relaxed);
        let short_of = SILENCE_LIMIT
            .saturating_sub(Duration::from_millis(u64::from(link.now_ms())))
            + Duration::from_millis(20);
        thread::sleep(short_of);
        s.tick(Ticks(2000));
        assert_eq!(state(&s).status, Status::Stopped);
        let reason = s
            .happenings_since(evo_esp32_sdk::happening::Seq(0))
            .happenings
            .into_iter()
            .find_map(|h| match h.kind {
                evo_esp32_sdk::happening::HappeningKind::CustodyReleased {
                    reason,
                    ..
                } => reason,
                _ => None,
            });
        assert_eq!(reason.as_deref(), Some("the source-host stopped sending"));
        let _ = host.stream.flush();
    }
}

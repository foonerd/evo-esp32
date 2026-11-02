// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! A playback warden that decodes with Symphonia.
//!
//! `org.evoframework.playback.symphonia` stocks `playback/decode`. Handed
//! a [`Play`] it opens the track's URI (`file://` on the host; `http://`
//! and `https://` through the [`StreamOpener`] the distribution gave it,
//! buffered on a thread of its own, see [`stream`]), probes the
//! container, makes a decoder for its first audio track and hands
//! stream and sink to a decode thread of its own (the Feed tier, one
//! above the loop that reads the touch and paints), which decodes block
//! by block and offers the frames to the [`FrameSink`] the distribution
//! gave it. The sink paces: when it takes nothing the thread waits a
//! moment and offers again. Pause, resume, seek and stop reach the
//! thread as commands between blocks; its position, format and the
//! station's title come back through shared state the warden's tick
//! publishes, so the tick itself is a hand-over of microseconds and the
//! loop paints when it likes. A network stream is decoded only from
//! what has arrived, and the titles it carries become the track's.
//!
//! The position a consumer draws is the frames the sink has accepted,
//! which is as near the ear as the warden can know without the sink's
//! own clock. Pause holds decoding; resume lets it go; seek asks the
//! reader for the time, resets the decoder and flushes the sink. An
//! MPEG audio stream is seeked coarsely (the reader estimates the byte
//! position from the time and resyncs there): accurate would walk every
//! frame from the start, which off a card is seconds a minute of audio.
//! Exact on a constant bit rate; on a variable one the landing is off
//! by the stream's drift from the mean, a few seconds at worst.
//!
//! The stream's own tags (ID3v2 on MP3, Vorbis comments on FLAC, the
//! INFO list on WAV) name the track: the title replaces the one the
//! source gave (a file's stem, usually), artist and album fill in where
//! the source had none; the container's duration likewise.
//!
//! FLAC, MP3, WAV and raw PCM, the codecs a card of music holds. The
//! decoder is pure Rust; what it costs on the target is the measurement
//! this plugin exists to make, with the panel and Wi-Fi up beside it.
//! One custody at a time: a second `Play` while one is in hand is
//! refused, as a single output carries one stream.
//!
//! A stream is reached on a thread of its own, a file on the card as
//! much as one on the network: `take_custody` returns at once with the
//! state `Opening`, the thread connects or opens, buffers and probes,
//! and the tick that finds it done starts the sink and says `Playing`;
//! one that finds it failed ends the custody with the reason. The loop
//! that also reads the touch and paints is never held for an open: a
//! file on the card is not "a read away" — its file system maps the
//! clusters at the open and the probe reads an embedded cover whole
//! (Symphonia 0.6 applies no limit to a FLAC picture), a quarter of a
//! second on a card.
#![forbid(unsafe_code)]

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use evo_esp32_sdk::context::Context;
use evo_esp32_sdk::contract::audio::playback::{
    Correction, Format, FrameSink, Play, Samples, State, Status, KIND,
};
use evo_esp32_sdk::contract::audio::{from_value, shelf, to_value, Track};
use evo_esp32_sdk::mailbox::{
    mailbox, Receiver, RecvTimeoutError, Sender, TryRecvError,
};
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
use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::audio::well_known::{
    CODEC_ID_MP1, CODEC_ID_MP2, CODEC_ID_MP3,
};
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::codecs::CodecParameters;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{
    FormatOptions, FormatReader, SeekMode, SeekTo, TrackType,
};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::{MetadataOptions, StandardTag};
use symphonia::core::units::{Time, TimeBase, Timestamp};

pub mod ahead;
pub mod flac;
pub mod hls;
pub mod measure;
pub mod netfile;
pub mod stream;
pub mod ts;
use evo_esp32_sdk::contract::audio::playback::{
    ByteFile, FileOpener, StreamOpener,
};
use stream::Live;

/// The plugin's name, as the catalogue and the steward know it.
pub const NAME: &str = "org.evoframework.playback.symphonia";

/// How long the warden asks to be left alone while paused.
const PAUSED_IDLE_MS: u32 = 250;
/// How long to wait when the sink took nothing.
const SINK_FULL_IDLE_MS: u32 = 20;
/// How long to wait for a network stream's buffer to fill.
const BUFFERING_IDLE_MS: u32 = 50;
/// How long between looks at a stream still being reached.
const OPENING_IDLE_MS: u32 = 50;
/// The stack of the thread that reaches a network stream: the TLS
/// handshake runs on it, and the probe. Placed by the distribution
/// (PSRAM on the board).
#[cfg(not(feature = "aac-symphonia"))]
const OPEN_STACK: usize = 32 * 1024;
/// With Symphonia's AAC decoder, which is built on the stack before
/// it is boxed, the probe needs far more; a host has it to spare.
#[cfg(feature = "aac-symphonia")]
const OPEN_STACK: usize = 256 * 1024;
/// The most audio the decode thread takes in one block before it looks
/// at its commands and publishes its state: a pause or a seek waits at
/// most this long, and a block is at least one packet (a FLAC block is
/// up to 93 ms of audio at 44.1 kHz).
const BLOCK_MS: u64 = 50;
/// How long the warden asks to be left alone while the decode thread
/// plays: each tick reads the thread's state and publishes it if it
/// moved, and learns of the end of the track; the next track's open
/// starts within this of the last frame.
const DECODING_IDLE_MS: u32 = 20;
/// The decode thread's stack. Placed by the distribution (PSRAM on the
/// board); a FLAC frame's decode and the sink's copy live on it.
#[cfg(not(feature = "aac-symphonia"))]
const DECODE_STACK: usize = 16 * 1024;
/// Symphonia's AAC decoder keeps a large working set on the stack.
#[cfg(feature = "aac-symphonia")]
const DECODE_STACK: usize = 64 * 1024;
/// How long a release waits for the decode thread to finish its block.
const STOP_WAIT: Duration = Duration::from_millis(500);

/// What a file on a server must have in its read-ahead ring before the
/// tick decodes from it: a 96 kHz FLAC frame is up to 24 KB, and a read
/// that finds less would block the loop on the network.
const NETWORK_LOW_WATER: usize = 64 * 1024;

static STOCKS: [Stocking; 1] = [Stocking {
    rack: shelf::PLAYBACK,
    shelf: shelf::DECODE,
    shape: shelf::SHAPE,
}];

/// The open track: reader, decoder, and what they said about it.
struct Stream {
    reader: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    time_base: Option<TimeBase>,
    /// Coarse for MPEG audio, whose accurate seek walks the frames.
    seek_mode: SeekMode,
    /// A stream from the network: its buffer's state and its titles.
    live: Option<Live>,
    /// A file on a server: a look at its read-ahead ring, so the tick
    /// waits for the network rather than blocking on it.
    ahead: Option<ahead::Watch>,
}

impl Drop for Stream {
    /// A server's file says what the link gave: the rate while the
    /// reader was in a read is the link's ceiling for one request in
    /// flight, the number that decides whether a second is wanted.
    fn drop(&mut self) {
        if let Some(watch) = &self.ahead {
            let (bytes, ms) = watch.tally();
            let rate = if ms > 0 {
                u64::from(bytes) * 1000 / 1024 / u64::from(ms)
            } else {
                0
            };
            log::info!(
                "read: {} KB from the server in {ms} ms of reads: {rate} KB/s while reading",
                bytes / 1024
            );
        }
    }
}

/// A network stream being reached on its thread: what the thread will
/// send, and what the custody asked for meanwhile.
struct Opening {
    result: Receiver<Result<(Stream, Format), CustodyError>>,
    start_ms: u64,
    /// Paused before the stream arrived: it starts held.
    hold: bool,
}

/// What the plugin asks of the decode thread.
enum Command {
    Pause,
    Resume,
    Seek(u64),
    Stop,
}

/// What the decode thread says once, as it ends.
enum Ending {
    /// The stream played to its end.
    Done,
    /// The stream was lost, for this reason.
    Failed(String),
    /// Told to stop.
    Stopped,
}

/// The sink, shared between the plugin (which stops it when a custody
/// ends) and the decode thread (which starts, writes and flushes it).
type SharedSink = Arc<SinkCell>;

/// The sink behind a lock that does not poison: a decoder that panicked
/// mid-write must not take the sink down with it.
struct SinkCell(Mutex<Box<dyn FrameSink + Send>>);

impl SinkCell {
    fn lock(&self) -> MutexGuard<'_, Box<dyn FrameSink + Send>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The decode thread's state as the plugin reads it on each tick.
struct SharedState(Mutex<State>);

impl SharedState {
    fn new(state: State) -> Self {
        Self(Mutex::new(state))
    }

    fn get(&self) -> State {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set(&self, state: &State) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = state.clone();
    }
}

/// The decode thread, from the plugin's side.
struct Decoding {
    commands: Sender<Command>,
    ending: Receiver<Ending>,
    shared: Arc<SharedState>,
    thread: Option<JoinHandle<()>>,
}

impl Decoding {
    /// Stop the thread and wait for it, within reason.
    fn stop(mut self) {
        let _ = self.commands.send(Command::Stop);
        match self.ending.recv_timeout(STOP_WAIT) {
            Ok(_) | Err(RecvTimeoutError::Disconnected) => {
                if let Some(t) = self.thread.take() {
                    let _ = t.join();
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                log::warn!(
                    "decode: the thread did not stop within {} ms; left to finish its block",
                    STOP_WAIT.as_millis()
                );
            }
        }
    }
}

/// The decode thread's own: the stream, the sink, and the account of
/// what has been decoded and what the sink has taken.
struct Decoder {
    stream: Stream,
    sink: SharedSink,
    /// The state as this thread knows it, published to `shared`.
    state: State,
    shared: Arc<SharedState>,
    commands: Receiver<Command>,
    /// Interleaved samples decoded but not yet taken by the sink, in the
    /// stream's width: 16-bit for a stream of 16 bits or fewer (what the
    /// codec would widen for nothing and the sink would store at twice
    /// the bytes), 32-bit with the stream's bits at the top for a wider
    /// one.
    pending: Pending,
    /// How far into `pending` the sink has got, in samples.
    taken: usize,
    /// Frames the sink has accepted since `base_ms`.
    frames_out: u64,
    /// Position at the start or the last seek, milliseconds.
    base_ms: u64,
    /// Frames still to drop after a seek: the reader lands on a packet
    /// boundary at or before the time asked, and the rest is trimmed here
    /// so the position is the one asked for.
    skip: u64,
    /// The reader has no more packets; drain `pending`, then done.
    ended: bool,
    paused: bool,
}

/// The custody in hand.
struct InHand {
    handle: CustodyHandle,
    /// The state as last published.
    state: State,
    /// The stream being reached; none once it is.
    opening: Option<Opening>,
    /// The decode thread; none while `opening`.
    decoding: Option<Decoding>,
}

pub use symphonia::core::codecs::registry::CodecRegistry;

/// The codecs Symphonia ships, as this crate enables them: the registry a
/// distribution starts from before preferring one of its own. Symphonia's
/// AAC decoder comes only with the `aac-symphonia` feature: `aac` alone
/// brings the ADTS demuxer and the stream's refusal logic, for a
/// distribution that decodes AAC with a decoder of its own.
pub fn default_codecs() -> CodecRegistry {
    let mut registry = CodecRegistry::new();
    #[cfg(feature = "flac")]
    registry
        .register_audio_decoder::<symphonia::default::codecs::FlacDecoder>();
    #[cfg(feature = "mp3")]
    registry.register_audio_decoder::<symphonia::default::codecs::MpaDecoder>();
    #[cfg(feature = "pcm")]
    registry.register_audio_decoder::<symphonia::default::codecs::PcmDecoder>();
    #[cfg(feature = "vorbis")]
    registry
        .register_audio_decoder::<symphonia::default::codecs::VorbisDecoder>();
    #[cfg(feature = "aac-symphonia")]
    registry.register_audio_decoder::<symphonia::default::codecs::AacDecoder>();
    registry
}

/// The Symphonia playback warden.
pub struct PlaybackSymphonia {
    sink: SharedSink,
    codecs: Arc<CodecRegistry>,
    /// How `http://` and `https://` tracks are reached; none, and they
    /// are refused.
    opener: Option<Arc<dyn StreamOpener>>,
    /// How the files of other schemes (`smb://`, `nfs://`) are reached,
    /// one opener per scheme.
    files: Arc<Vec<Box<dyn FileOpener>>>,
    /// Where the card ring's chunk buffer comes from; the heap unless
    /// the distribution says ([`with_card_memory`](Self::with_card_memory)).
    card_memory: ahead::ChunkMemory,
    /// A flag the distribution raises while the card's bus is wanted
    /// elsewhere; the ring's paced reads wait for it to clear.
    card_hold: Option<&'static std::sync::atomic::AtomicBool>,
    subject: Option<SubjectId>,
    in_hand: Option<InHand>,
}

impl PlaybackSymphonia {
    /// A plugin ready to be admitted, writing to `sink`, decoding with
    /// Symphonia's own codecs. The sink crosses to the decode thread.
    pub fn new(sink: Box<dyn FrameSink + Send>) -> Self {
        Self::with_codecs(sink, default_codecs())
    }

    /// As [`new`](Self::new), decoding with `codecs`: a distribution's
    /// registry, where a platform decoder may stand above Symphonia's.
    pub fn with_codecs(
        sink: Box<dyn FrameSink + Send>,
        codecs: CodecRegistry,
    ) -> Self {
        Self {
            sink: Arc::new(SinkCell(Mutex::new(sink))),
            codecs: Arc::new(codecs),
            opener: None,
            files: Arc::new(Vec::new()),
            card_memory: ahead::heap_chunk,
            card_hold: None,
            subject: None,
            in_hand: None,
        }
    }

    /// The memory the card ring reads into: on a device whose card
    /// driver reaches only part of its memory by DMA, that part (see
    /// [`ahead::ChunkMemory`]).
    pub fn with_card_memory(mut self, memory: ahead::ChunkMemory) -> Self {
        self.card_memory = memory;
        self
    }

    /// With a flag the card ring yields the bus to: raised by the
    /// distribution while something else on the card's bus (a panel)
    /// must not wait behind a read. See [`ahead::Tuning::hold`].
    pub fn yielding_card_to(
        mut self,
        hold: &'static std::sync::atomic::AtomicBool,
    ) -> Self {
        self.card_hold = Some(hold);
        self
    }

    /// With a way to reach `http://` and `https://` tracks: the
    /// distribution's transport. A URI that answers with an HLS playlist
    /// is played segment by segment through it ([`hls`]).
    pub fn with_opener(mut self, opener: Box<dyn StreamOpener>) -> Self {
        self.opener = Some(Arc::new(hls::Aware(Arc::from(opener))));
        self
    }

    /// With a way to reach the files of one more scheme (`smb`, `nfs`):
    /// a share's file is read by position, so a seek costs nothing.
    /// Called before admission, once per scheme.
    pub fn with_file_opener(mut self, opener: Box<dyn FileOpener>) -> Self {
        Arc::get_mut(&mut self.files)
            .expect("file openers are set before the plugin is admitted")
            .push(opener);
        self
    }

    fn publish(&self, ctx: &mut dyn Context, state: &State) {
        if let Some(id) = &self.subject {
            let _ = ctx.update(id, to_value(state));
        }
    }

    fn publish_in_hand(&self, ctx: &mut dyn Context) {
        if let Some(h) = &self.in_hand {
            self.publish(ctx, &h.state);
        }
    }
}

impl Plugin for PlaybackSymphonia {
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
                addressing: ExternalAddressing::new("playback", "now"),
                kind: KIND.into(),
                state: to_value(&State::default()),
            })
            .map_err(|e| LoadError(format!("announcing: {e}")))?;
        self.subject = Some(id);
        Ok(())
    }

    fn unload(&mut self, ctx: &mut dyn Context) {
        if let Some(h) = self.in_hand.take() {
            if let Some(decoding) = h.decoding {
                decoding.stop();
                self.sink.lock().stop();
            }
        }
        if let Some(id) = self.subject.take() {
            let _ = ctx.retract(&id);
        }
    }
}

impl Warden for PlaybackSymphonia {
    fn take_custody(
        &mut self,
        handle: CustodyHandle,
        assignment: Assignment,
        ctx: &mut dyn Context,
    ) -> Result<(), CustodyError> {
        if self.in_hand.is_some() {
            return Err(CustodyError("already playing".into()));
        }
        let play: Play = from_value(&assignment.instruction)
            .map_err(|e| CustodyError(format!("instruction: {e}")))?;
        let track = play.track;
        let opener = self.opener.clone();
        let files = Arc::clone(&self.files);
        if is_network(&track.uri) {
            if opener.is_none() && !is_http(&track.uri) {
                // A share's scheme with no opener for it is as refused
                // as a stream with no transport, but on this thread.
                file_opener(&files, &track.uri)?;
            } else if opener.is_none() {
                return Err(CustodyError(
                    "no transport for a network stream".into(),
                ));
            }
        }
        let (tx, rx) = mailbox();
        let (uri, codecs) = (track.uri.clone(), Arc::clone(&self.codecs));
        let card_memory = self.card_memory;
        let card_hold = self.card_hold;
        evo_esp32_sdk::thread::spawn(
            evo_esp32_sdk::thread::Role::Background,
            "open",
            OPEN_STACK,
            move || {
                // The custody may be gone by now; then this is dropped.
                let _ = tx.send(open(
                    &uri,
                    &codecs,
                    opener.as_ref(),
                    &files,
                    card_memory,
                    card_hold,
                ));
            },
        )
        .map_err(|e| CustodyError(format!("open thread: {e}")))?;
        let state = State {
            status: Status::Opening,
            track: Some(track),
            position_ms: 0,
            format: None,
        };
        self.publish(ctx, &state);
        self.in_hand = Some(InHand {
            handle,
            state,
            opening: Some(Opening {
                result: rx,
                start_ms: play.start_ms,
                hold: false,
            }),
            decoding: None,
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
        if let Some(opening) = h.opening.as_mut() {
            // Not yet reached: pause and resume are remembered, a seek
            // has nothing to land in.
            match c {
                Correction::Pause => opening.hold = true,
                Correction::Resume => opening.hold = false,
                Correction::Seek { .. } => {
                    return Err(CustodyError("still opening".into()));
                }
            }
            return Ok(());
        }
        let Some(decoding) = h.decoding.as_ref() else {
            return Err(CustodyError("no stream".into()));
        };
        // The thread acts within a block; the state says so now, so the
        // face does not wait on it.
        let command = match c {
            Correction::Pause => {
                h.state.status = Status::Paused;
                Command::Pause
            }
            Correction::Resume => {
                h.state.status = Status::Playing;
                Command::Resume
            }
            Correction::Seek { position_ms } => {
                let cap = h.state.track.as_ref().and_then(|t| t.duration_ms);
                let target = cap.map_or(position_ms, |d| position_ms.min(d));
                h.state.position_ms = target;
                Command::Seek(target)
            }
        };
        decoding
            .commands
            .send(command)
            .map_err(|_| CustodyError("the decoder has gone".into()))?;
        decoding.shared.set(&h.state);
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
            // Dropping a custody still opening drops the thread's
            // receiver; what it opens is dropped in turn, fetcher and all.
            if let Some(h) = self.in_hand.take() {
                if let Some(decoding) = h.decoding {
                    decoding.stop();
                    self.sink.lock().stop();
                }
            }
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
        if let Some(opening) = h.opening.as_ref() {
            let opened = match opening.result.try_recv() {
                Err(TryRecvError::Empty) => {
                    return Progress::Idle(OPENING_IDLE_MS)
                }
                Err(TryRecvError::Disconnected) => {
                    Err(CustodyError("the open gave up".into()))
                }
                Ok(r) => r,
            };
            let Opening { start_ms, hold, .. } =
                h.opening.take().expect("checked");
            let begun = opened.and_then(|o| {
                begin(h, o, start_ms, hold, Arc::clone(&self.sink))
            });
            if let Err(e) = begun {
                log::warn!("open: {}; releasing", e.0);
                return Progress::Failed(e.0);
            }
            self.publish_in_hand(ctx);
            return Progress::Working;
        }
        let Some(decoding) = h.decoding.as_mut() else {
            return Progress::Failed("no stream".into());
        };
        // The thread's word, if it has ended; else its state, published
        // when it moved.
        match decoding.ending.try_recv() {
            Ok(Ending::Done | Ending::Stopped)
            | Err(TryRecvError::Disconnected) => {
                return Progress::Done;
            }
            Ok(Ending::Failed(why)) => return Progress::Failed(why),
            Err(TryRecvError::Empty) => {}
        }
        let latest = decoding.shared.get();
        let moved = latest != h.state;
        if moved {
            h.state = latest;
        }
        let paused = h.state.status == Status::Paused;
        if moved {
            self.publish_in_hand(ctx);
        }
        Progress::Idle(if paused {
            PAUSED_IDLE_MS
        } else {
            DECODING_IDLE_MS
        })
    }
}

/// Whether `uri` is reached over the network rather than read from a
/// path: any scheme but `file`.
fn is_network(uri: &str) -> bool {
    uri.contains("://") && !uri.starts_with("file://")
}

/// Whether `uri` is a stream or a server's file over HTTP.
fn is_http(uri: &str) -> bool {
    uri.starts_with("http://") || uri.starts_with("https://")
}

/// The opener for `uri`'s scheme among the file openers.
fn file_opener<'a>(
    files: &'a [Box<dyn FileOpener>],
    uri: &str,
) -> Result<&'a dyn FileOpener, CustodyError> {
    let scheme = uri.split("://").next().unwrap_or("");
    files
        .iter()
        .find(|o| o.scheme() == scheme)
        .map(|o| o.as_ref())
        .ok_or_else(|| {
            CustodyError(format!("{scheme}://: no way to reach it here"))
        })
}

/// A [`ByteFile`] as Symphonia reads: a cursor over positional reads.
struct FileReader {
    file: Box<dyn ByteFile>,
    pos: u64,
    len: u64,
}

impl std::io::Read for FileReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.len {
            return Ok(0);
        }
        let n = self
            .file
            .read_at(self.pos, buf)
            .map_err(std::io::Error::other)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl std::io::Seek for FileReader {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        use std::io::SeekFrom;
        let to = match pos {
            SeekFrom::Start(p) => p as i128,
            SeekFrom::End(d) => self.len as i128 + d as i128,
            SeekFrom::Current(d) => self.pos as i128 + d as i128,
        };
        if to < 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek before the start",
            ));
        }
        self.pos = to as u64;
        Ok(self.pos)
    }
}

/// The stream is open: name the track from its tags, land the start,
/// and hand stream and sink to the decode thread, which starts the sink
/// and plays (or holds, if a pause came while the stream was being
/// reached).
fn begin(
    h: &mut InHand,
    (mut stream, format): (Stream, Format),
    start_ms: u64,
    hold: bool,
    sink: SharedSink,
) -> Result<(), CustodyError> {
    let track = h
        .state
        .track
        .as_mut()
        .ok_or_else(|| CustodyError("no track".into()))?;
    if track.duration_ms.is_none() {
        track.duration_ms = duration_ms(&stream);
    }
    let tags = tags(&mut stream);
    if let Some(title) = tags.title {
        track.title = title;
    }
    if track.artist.is_none() {
        track.artist = tags.artist;
    }
    if track.album.is_none() {
        track.album = tags.album;
    }
    if let Some(live) = &stream.live {
        // A station: its name is the album line, under the title,
        // which the stream replaces with what is playing. The name
        // the playlist gave wins over the server's (`icy-name` tends
        // to carry the bitrate); a station the playlist left to its
        // host takes the server's.
        let curated = track.title != host_of(&track.uri);
        track.album = Some(match &live.info.name {
            Some(name) if !curated => name.clone(),
            _ => track.title.clone(),
        });
    }
    let (base_ms, skip) = if start_ms > 0 {
        seek(&mut stream, start_ms, format.sample_rate)?
    } else {
        (0, 0)
    };
    h.state.status = if hold {
        Status::Paused
    } else {
        Status::Playing
    };
    h.state.position_ms = base_ms;
    h.state.format = Some(format.clone());
    let shared = Arc::new(SharedState::new(h.state.clone()));
    let (commands, command_rx) = mailbox();
    let (ending_tx, ending) = mailbox();
    let mut decoder = Decoder {
        stream,
        sink,
        state: h.state.clone(),
        shared: Arc::clone(&shared),
        commands: command_rx,
        pending: Pending::for_bits(format.bits),
        taken: 0,
        frames_out: 0,
        base_ms,
        skip,
        ended: false,
        paused: hold,
    };
    let thread = evo_esp32_sdk::thread::spawn(
        evo_esp32_sdk::thread::Role::Decode,
        "decode",
        DECODE_STACK,
        move || {
            let ending = decoder.run(&format);
            // The plugin may have let go; then nobody is listening.
            let _ = ending_tx.send(ending);
        },
    )
    .map_err(|e| CustodyError(format!("decode thread: {e}")))?;
    h.decoding = Some(Decoding {
        commands,
        ending,
        shared,
        thread: Some(thread),
    });
    Ok(())
}

impl Decoder {
    /// Decode until the stream ends, fails, or a `Stop` arrives; what
    /// happened is the answer. The sink is started here, on this
    /// thread, so a codec written at a rate change is written from here
    /// too.
    fn run(&mut self, format: &Format) -> Ending {
        {
            let mut sink = self.sink.lock();
            sink.start(format);
            if self.paused {
                sink.pause();
            }
        }
        self.publish();
        loop {
            // Paused: wait for the next command rather than poll.
            let command = if self.paused {
                match self.commands.recv() {
                    Ok(c) => Some(c),
                    Err(_) => return Ending::Done,
                }
            } else {
                match self.commands.try_recv() {
                    Ok(c) => Some(c),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => return Ending::Done,
                }
            };
            if let Some(c) = command {
                if let Some(ending) = self.obey(c) {
                    return ending;
                }
                continue;
            }
            match self.block() {
                Step::Working => self.publish(),
                Step::Wait(ms) => {
                    // The sink may have taken part of a block before it
                    // filled: the position moved.
                    self.publish();
                    std::thread::sleep(Duration::from_millis(u64::from(ms)));
                }
                Step::Ended(ending) => {
                    self.publish();
                    return ending;
                }
            }
        }
    }

    /// Act on a command; `Some` when it ends the run.
    fn obey(&mut self, command: Command) -> Option<Ending> {
        match command {
            Command::Pause => {
                self.sink.lock().pause();
                self.paused = true;
                self.state.status = Status::Paused;
            }
            Command::Resume => {
                self.sink.lock().resume();
                self.paused = false;
                self.state.status = Status::Playing;
            }
            Command::Seek(target) => {
                let rate =
                    self.state.format.as_ref().map_or(0, |f| f.sample_rate);
                match seek(&mut self.stream, target, rate) {
                    Ok((landed, skip)) => {
                        self.pending.clear();
                        self.taken = 0;
                        self.frames_out = 0;
                        self.base_ms = landed;
                        self.skip = skip;
                        self.ended = false;
                        self.sink.lock().flush();
                    }
                    // The track plays on from where it was; the face
                    // falls back to the published position.
                    Err(e) => log::warn!("seek: {}", e.0),
                }
            }
            Command::Stop => return Some(Ending::Stopped),
        }
        self.publish();
        None
    }

    /// One bounded slice of decoding: what is pending offered to the
    /// sink, then up to [`BLOCK_MS`] of audio decoded.
    fn block(&mut self) -> Step {
        if let Some(live) = self.stream.live.as_ref() {
            if let Some(title) = live.take_title() {
                if let Some(t) = self.state.track.as_mut() {
                    // Stations say "Artist - Title".
                    match title.split_once(" - ") {
                        Some((artist, name)) if !name.trim().is_empty() => {
                            t.artist = Some(artist.trim().to_string());
                            t.title = name.trim().to_string();
                        }
                        _ => {
                            t.artist = None;
                            t.title = title;
                        }
                    }
                }
            }
        }
        let channels = self
            .state
            .format
            .as_ref()
            .map_or(1, |f| usize::from(f.channels).max(1));
        let rate =
            u64::from(self.state.format.as_ref().map_or(0, |f| f.sample_rate));
        let budget_frames = (rate * BLOCK_MS / 1000).max(1);
        let mut decoded_frames: u64 = 0;
        loop {
            // Offer what is pending before decoding more.
            if self.taken < self.pending.len() {
                let writing = Instant::now();
                let frames =
                    self.sink.lock().write(self.pending.from(self.taken));
                measure::add(&measure::SINK_WRITE_US, writing);
                if frames == 0 {
                    return Step::Wait(SINK_FULL_IDLE_MS);
                }
                self.taken += frames * channels;
                self.frames_out += frames as u64;
                if self.taken < self.pending.len() {
                    continue;
                }
                self.pending.clear();
                self.taken = 0;
            }
            if self.ended {
                return Step::Ended(Ending::Done);
            }
            if decoded_frames >= budget_frames {
                return Step::Working;
            }
            // A stream from the network is decoded only from what has
            // arrived; short of that, wait for the fetcher.
            if self.stream.live.as_mut().is_some_and(|l| !l.ready())
                || self.stream.ahead.as_ref().is_some_and(|a| !a.ready())
            {
                return Step::Wait(BUFFERING_IDLE_MS);
            }
            let decoding = Instant::now();
            let next = next_frames(&mut self.stream, &mut self.pending);
            measure::add(&measure::DECODE_US, decoding);
            measure::PACKETS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            match next {
                Ok(Some((frames, rate, channels_now))) => {
                    // The decoder has the last word on the format: when
                    // it differs from what the sink was started with,
                    // the sink is started again and the state says so.
                    let differs = self.state.format.as_ref().is_some_and(|f| {
                        f.sample_rate != rate || f.channels != channels_now
                    });
                    if differs {
                        if let Some(f) = self.state.format.as_mut() {
                            let played = if f.sample_rate == 0 {
                                self.base_ms
                            } else {
                                self.base_ms
                                    + self.frames_out * 1000
                                        / u64::from(f.sample_rate)
                            };
                            log::info!(
                                "format: the decoder gives {rate} Hz {channels_now} ch, the container said {} Hz {} ch; following the decoder",
                                f.sample_rate, f.channels
                            );
                            f.sample_rate = rate;
                            f.channels = channels_now;
                            self.sink.lock().start(f);
                            self.base_ms = played;
                            self.frames_out = 0;
                        }
                    }
                    decoded_frames += frames;
                    self.taken = 0;
                    if self.skip > 0 {
                        let drop = self.skip.min(frames);
                        self.taken =
                            drop as usize * usize::from(channels_now).max(1);
                        self.skip -= drop;
                    }
                }
                Ok(None) => self.ended = true,
                Err(e) => {
                    // The stream is lost; the custody ends with the reason.
                    log::warn!("decode: {e}; releasing");
                    self.state.status = Status::Stopped;
                    self.pending.clear();
                    self.taken = 0;
                    self.ended = true;
                    return Step::Ended(Ending::Failed(e.to_string()));
                }
            }
        }
    }

    /// The position being heard: what the sink took, less what it still
    /// holds; then the state as the plugin publishes it.
    fn publish(&mut self) {
        let rate = self.state.format.as_ref().map_or(0, |f| f.sample_rate);
        self.state.position_ms = if rate == 0 {
            self.base_ms
        } else {
            let held = self.sink.lock().held() as u64;
            let heard = self.frames_out.saturating_sub(held);
            self.base_ms + heard * 1000 / u64::from(rate)
        };
        self.shared.set(&self.state);
    }
}

/// What one slice of the decode thread's work came to.
enum Step {
    /// Decoded a block; go round again.
    Working,
    /// Nothing to do for this many milliseconds: the sink is full or
    /// the network has not delivered.
    Wait(u32),
    /// The run is over.
    Ended(Ending),
}

/// Whether a server's `Content-Type` names something this build
/// decodes. AAC is a feature; a type not known here is tried.
fn decodable(content_type: &str) -> bool {
    let kind = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match kind.as_str() {
        "audio/aac" | "audio/aacp" | "audio/x-aac" | "audio/mp4"
        | "audio/x-m4a" => cfg!(feature = "aac"),
        "application/vnd.apple.mpegurl"
        | "application/x-mpegurl"
        | "audio/mpegurl"
        | "audio/x-mpegurl"
        | "audio/x-scpls"
        | "text/html" => false,
        _ => true,
    }
}

/// Named as a FLAC file: the only kind whose pictures are read as
/// padding.
fn is_flac(uri: &str) -> bool {
    uri.split(['?', '#'])
        .next()
        .is_some_and(|p| p.to_ascii_lowercase().ends_with(".flac"))
}

/// Open a URI: a `file://` on the host, an `http://`/`https://`
/// stream through the opener, or a share's file through the opener
/// for its scheme; probe it, pick its default audio track,
/// make a decoder, and report the format the stream will come out in.
/// A network response with a length and ranges is a file on a server,
/// read and sought like the card's; one without is a station.
fn open(
    uri: &str,
    codecs: &CodecRegistry,
    opener: Option<&Arc<dyn StreamOpener>>,
    files: &[Box<dyn FileOpener>],
    card_memory: ahead::ChunkMemory,
    card_hold: Option<&'static std::sync::atomic::AtomicBool>,
) -> Result<(Stream, Format), CustodyError> {
    let mut hint = Hint::new();
    let mut live = None;
    let mut ahead_watch = None;
    let began = Instant::now();
    let mut pictures = 0usize;
    let mut patched_in = Duration::ZERO;
    let source: Box<dyn symphonia::core::io::MediaSource> = if is_network(uri)
        && !is_http(uri)
    {
        let file = file_opener(files, uri)?.open(uri).map_err(CustodyError)?;
        if let Some(ext) = uri
            .split(['?', '#'])
            .next()
            .and_then(|p| p.rsplit('.').next())
            .filter(|e| e.len() <= 4 && !e.contains('/'))
        {
            hint.with_extension(ext);
        }
        let len = file.length();
        let mut file = FileReader { file, pos: 0, len };
        let patching = Instant::now();
        let patches = if is_flac(uri) {
            flac::picture_patches(&mut file).unwrap_or_default()
        } else {
            Vec::new()
        };
        patched_in = patching.elapsed();
        pictures = patches.len();
        let reader = ahead::ReadAhead::with_tuning(
            flac::Patched::new(file, patches),
            len,
            ahead::Tuning::network(),
        )
        .map_err(|e| CustodyError(format!("reader: {e}")))?;
        ahead_watch = Some(reader.watch(NETWORK_LOW_WATER));
        Box::new(reader)
    } else if is_network(uri) {
        let opener = opener.ok_or_else(|| {
            CustodyError("no transport for a network stream".into())
        })?;
        let bytes = opener.open(uri).map_err(CustodyError)?;
        // A format this build cannot decode is refused by the server's
        // word for it, before a byte of it is probed.
        if let Some(kind) = bytes.header("content-type") {
            if !decodable(&kind) {
                return Err(CustodyError(format!(
                    "{kind}: format not supported here"
                )));
            }
        }
        let info = stream::StreamInfo::from(bytes.as_ref());
        if let Some(ext) = info.extension_hint() {
            hint.with_extension(ext);
        } else if let Some(ext) = uri
            .split(['?', '#'])
            .next()
            .and_then(|p| p.rsplit('.').next())
            .filter(|e| e.len() <= 4 && !e.contains('/'))
        {
            hint.with_extension(ext);
        }
        if let Some(len) = netfile::seekable_length(bytes.as_ref()) {
            let file =
                netfile::NetFile::new(Arc::clone(opener), uri, bytes, len);
            let reader = ahead::ReadAhead::with_tuning(
                file,
                len,
                ahead::Tuning::network(),
            )
            .map_err(|e| CustodyError(format!("reader: {e}")))?;
            ahead_watch = Some(reader.watch(NETWORK_LOW_WATER));
            Box::new(reader)
        } else {
            let reopen = stream::Reopen {
                opener: Arc::clone(opener),
                uri: uri.to_string(),
            };
            let (buffered, handle) = stream::start(bytes, Some(reopen))
                .map_err(|e| CustodyError(format!("buffer: {e}")))?;
            live = Some(handle);
            Box::new(buffered)
        }
    } else {
        let path = file_path(uri)?;
        let mut file = File::open(&path)
            .map_err(|e| CustodyError(format!("{}: {e}", path.display())))?;
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }
        // The card is read ahead on a thread of its own, so the decoder
        // never waits on it while there is something in the ring.
        let len = file
            .metadata()
            .map_err(|e| CustodyError(format!("{}: {e}", path.display())))?
            .len();
        let patching = Instant::now();
        let patches = if is_flac(uri) {
            flac::picture_patches(&mut file).unwrap_or_default()
        } else {
            Vec::new()
        };
        patched_in = patching.elapsed();
        pictures = patches.len();
        Box::new(
            ahead::ReadAhead::with_tuning(
                flac::Patched::new(file, patches),
                len,
                match card_hold {
                    Some(h) => ahead::Tuning::card()
                        .in_memory(card_memory)
                        .yielding_to(h),
                    None => ahead::Tuning::card().in_memory(card_memory),
                },
            )
            .map_err(|e| CustodyError(format!("reader: {e}")))?,
        )
    };
    let opened_in = began.elapsed();
    let mss =
        MediaSourceStream::new(source, MediaSourceStreamOptions::default());
    let probing = Instant::now();
    let before = ahead::Account::now();
    let reader = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| CustodyError(format!("probe: {e}")))?;
    let cost = ahead::Account::now().since(before);
    log::info!(
        "open: {} ms to the source ({} pictures read as padding in {} ms), {} ms to probe: the source moved {} times, {} ms in its seeks, {} ms in its reads",
        opened_in.as_millis(),
        pictures,
        patched_in.as_millis(),
        probing.elapsed().as_millis(),
        cost.restarts,
        cost.seek_ms,
        cost.read_ms
    );
    if let Some(live) = &live {
        live.probed();
    }
    let track = reader
        .default_track(TrackType::Audio)
        .ok_or_else(|| CustodyError("no audio track".into()))?;
    let Some(CodecParameters::Audio(params)) = track.codec_params.as_ref()
    else {
        return Err(CustodyError("no audio codec parameters".into()));
    };
    let decoder = codecs
        .make_audio_decoder(params, &AudioDecoderOptions::default())
        .map_err(|e| CustodyError(format!("decoder: {e}")))?;
    let format = Format {
        codec: decoder.codec_info().short_name.into(),
        sample_rate: params
            .sample_rate
            .ok_or_else(|| CustodyError("no sample rate".into()))?,
        bits: params
            .bits_per_sample
            .and_then(|b| u8::try_from(b).ok())
            .unwrap_or(16),
        channels: params
            .channels
            .as_ref()
            .map(|c| c.count())
            .and_then(|c| u8::try_from(c).ok())
            .ok_or_else(|| CustodyError("no channel count".into()))?,
    };
    let seek_mode =
        if [CODEC_ID_MP1, CODEC_ID_MP2, CODEC_ID_MP3].contains(&params.codec) {
            SeekMode::Coarse
        } else {
            SeekMode::Accurate
        };
    let stream = Stream {
        track_id: track.id,
        time_base: track.time_base,
        reader,
        decoder,
        seek_mode,
        live,
        ahead: ahead_watch,
    };
    Ok((stream, format))
}

/// The path a `file://` URI names. Only local files; nothing else is a
/// track this warden can open.
fn file_path(uri: &str) -> Result<PathBuf, CustodyError> {
    uri.strip_prefix("file://")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| CustodyError(format!("not a file URI: {uri}")))
}

/// The host part of a URL: what the source titles a station by when the
/// playlist gives it no name.
fn host_of(uri: &str) -> &str {
    uri.split("://")
        .nth(1)
        .unwrap_or(uri)
        .split(['/', ':', '?'])
        .next()
        .unwrap_or(uri)
}

/// What the stream's tags call the track.
#[derive(Debug, Default)]
struct Tags {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
}

/// The standard tags the reader found ahead of the audio: the latest
/// revision, which the probe fills from an ID3v2 tag, a FLAC's Vorbis
/// comments or a WAV's INFO list. Empty values count as absent; the
/// album artist stands in for an artist the tags lack.
fn tags(stream: &mut Stream) -> Tags {
    let mut out = Tags::default();
    let mut album_artist = None;
    let mut metadata = stream.reader.metadata();
    let Some(revision) = metadata.skip_to_latest() else {
        return out;
    };
    let text = |s: &str| (!s.trim().is_empty()).then(|| s.trim().to_string());
    for tag in &revision.media.tags {
        match &tag.std {
            Some(StandardTag::TrackTitle(s)) => out.title = text(s),
            Some(StandardTag::Artist(s)) => out.artist = text(s),
            Some(StandardTag::AlbumArtist(s)) => album_artist = text(s),
            Some(StandardTag::Album(s)) => out.album = text(s),
            _ => {}
        }
    }
    if out.artist.is_none() {
        out.artist = album_artist;
    }
    out
}

/// The track's length in milliseconds, where the container says.
fn duration_ms(stream: &Stream) -> Option<u64> {
    let track = stream
        .reader
        .tracks()
        .iter()
        .find(|t| t.id == stream.track_id)?;
    let frames = i64::try_from(track.num_frames?).ok()?;
    let time = stream.time_base?.calc_time(Timestamp::new(frames))?;
    Some(time_ms(time))
}

fn time_ms(t: Time) -> u64 {
    let (secs, nanos) = t.parts();
    u64::try_from(secs).unwrap_or(0) * 1000 + u64::from(nanos) / 1_000_000
}

/// Seek the reader to `ms` and reset the decoder. The reader lands on a
/// packet boundary at or before `ms`; the return is the position asked
/// for and the frames to drop from the decoder's output to get there.
fn seek(
    stream: &mut Stream,
    ms: u64,
    rate: u32,
) -> Result<(u64, u64), CustodyError> {
    let beyond = || CustodyError("seek beyond time".into());
    let secs = i64::try_from(ms / 1000).map_err(|_| beyond())?;
    let nanos = u32::try_from((ms % 1000) * 1_000_000).map_err(|_| beyond())?;
    let time = Time::try_new(secs, nanos).ok_or_else(beyond)?;
    let began = Instant::now();
    let before = ahead::Account::now();
    let landed = stream
        .reader
        .seek(
            stream.seek_mode,
            SeekTo::Time {
                time,
                track_id: Some(stream.track_id),
            },
        )
        .map_err(|e| CustodyError(format!("seek: {e}")))?;
    let cost = ahead::Account::now().since(before);
    log::info!(
        "seek to {ms} ms took {} ms: the source moved {} times, {} ms in its seeks, {} ms in its reads",
        began.elapsed().as_millis(),
        cost.restarts,
        cost.seek_ms,
        cost.read_ms
    );
    stream.decoder.reset();
    let landed_ms = stream
        .time_base
        .and_then(|tb| tb.calc_time(landed.actual_ts))
        .map_or(ms, time_ms);
    let skip = ms.saturating_sub(landed_ms) * u64::from(rate) / 1000;
    Ok((ms, skip))
}

/// The decoder's output in the stream's own width, offered to the sink
/// as [`Samples`]. The width is set at the open from the stream's bits
/// and does not change within a track.
enum Pending {
    Narrow(Vec<i16>),
    Wide(Vec<i32>),
}

impl Pending {
    fn for_bits(bits: u8) -> Self {
        if bits > 16 {
            Pending::Wide(Vec::new())
        } else {
            Pending::Narrow(Vec::new())
        }
    }

    fn len(&self) -> usize {
        match self {
            Pending::Narrow(v) => v.len(),
            Pending::Wide(v) => v.len(),
        }
    }

    fn clear(&mut self) {
        match self {
            Pending::Narrow(v) => v.clear(),
            Pending::Wide(v) => v.clear(),
        }
    }

    fn from(&self, taken: usize) -> Samples<'_> {
        match self {
            Pending::Narrow(v) => Samples::I16(&v[taken..]),
            Pending::Wide(v) => Samples::I32(&v[taken..]),
        }
    }

    /// The decoded buffer's samples, interleaved, in this width.
    fn take(&mut self, buf: &GenericAudioBufferRef<'_>) {
        match self {
            Pending::Narrow(v) => buf.copy_to_vec_interleaved::<i16>(v),
            Pending::Wide(v) => buf.copy_to_vec_interleaved::<i32>(v),
        }
    }
}

/// Decode the next packet of the track into `out` in the stream's width.
/// `Ok(Some(frames))` for frames appended, `Ok(None)` at the end of the
/// stream; malformed packets are skipped, not errors.
/// The next packet's frames into `out`: how many, and the rate and
/// channel count they came out at, which may differ from the
/// container's word (an AAC-Plus stream decodes at twice the header's
/// rate; an MP3's first frame may correct the probe).
fn next_frames(
    stream: &mut Stream,
    out: &mut Pending,
) -> Result<Option<(u64, u32, u8)>, SymphoniaError> {
    loop {
        let packet = match stream.reader.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => return Ok(None),
            Err(SymphoniaError::IoError(e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                return Ok(None)
            }
            Err(e) => return Err(e),
        };
        if packet.track_id != stream.track_id {
            continue;
        }
        match stream.decoder.decode(&packet) {
            Ok(buf) => {
                let frames = buf.frames() as u64;
                if frames == 0 {
                    // The packet fed the decoder and nothing came out
                    // (an MP3 frame whose main data the decoder has
                    // not seen); its empty buffer says nothing about
                    // the format.
                    continue;
                }
                let spec = buf.spec();
                let rate = spec.rate();
                let channels = spec.channels().count().clamp(1, 255) as u8;
                out.take(&buf);
                return Ok(Some((frames, rate, channels)));
            }
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(SymphoniaError::IoError(_)) => continue,
            Err(e) => return Err(e),
        }
    }
}

/// A [`Track`] for a local file, titled by its stem; the length is
/// filled in by the warden once the container is open.
pub fn file_track(path: &Path) -> Track {
    Track {
        uri: format!("file://{}", path.display()),
        title: path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("track")
            .into(),
        artist: None,
        album: None,
        duration_ms: None,
        sample_rate: None,
        art: None,
        live: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_esp32_sdk::catalogue::{Catalogue, Family, Kind, Rack, Shelf};
    use evo_esp32_sdk::contract::audio::playback::ByteStream;
    use evo_esp32_sdk::projection::ProjectionKey;
    use evo_esp32_sdk::Ticks;
    use evo_esp32_steward::{Config, Steward};
    use std::thread;

    fn catalogue() -> Catalogue {
        Catalogue {
            racks: vec![Rack {
                name: shelf::PLAYBACK.into(),
                family: Family::Domain,
                kinds: vec![Kind::Transformer],
                charter: String::new(),
                shelves: vec![Shelf {
                    name: shelf::DECODE.into(),
                    shape: shelf::SHAPE,
                    description: String::new(),
                }],
            }],
        }
    }

    /// A sink that holds up to `cap` frames until the test's ear takes
    /// them ([`Shared::hear`]), and keeps everything it ever took in
    /// `frames` for the content checks. Full, it takes nothing, and
    /// the decode thread waits — the pacing a real sink gives.
    #[derive(Default)]
    struct Tape {
        cap: usize,
        held: usize,
        format: Option<Format>,
        frames: Vec<i16>,
        flushes: u32,
        stops: u32,
    }

    #[derive(Clone)]
    struct Shared(Arc<Mutex<Tape>>);

    impl Shared {
        fn tape(&self) -> MutexGuard<'_, Tape> {
            self.0.lock().unwrap_or_else(|e| e.into_inner())
        }

        /// The ear takes `frames` of what is held.
        fn hear(&self, frames: usize) {
            let mut t = self.tape();
            t.held = t.held.saturating_sub(frames);
        }
    }

    impl FrameSink for Shared {
        fn start(&mut self, format: &Format) {
            self.tape().format = Some(format.clone());
        }
        fn write(&mut self, samples: Samples<'_>) -> usize {
            let interleaved: Vec<i16> = match samples {
                Samples::I16(s) => s.to_vec(),
                Samples::I32(s) => s.iter().map(|v| (v >> 16) as i16).collect(),
            };
            let interleaved = &interleaved[..];
            let mut t = self.tape();
            let ch = t.format.as_ref().map_or(1, |f| usize::from(f.channels));
            let room = t.cap.saturating_sub(t.held);
            let frames = (interleaved.len() / ch).min(room);
            t.frames.extend_from_slice(&interleaved[..frames * ch]);
            t.held += frames;
            frames
        }
        fn flush(&mut self) {
            let mut t = self.tape();
            t.flushes += 1;
            t.frames.clear();
            t.held = 0;
        }
        fn stop(&mut self) {
            self.tape().stops += 1;
        }
        fn held(&self) -> usize {
            self.tape().held
        }
    }

    const RATE: u32 = 8000;

    /// A 16-bit stereo WAV of `frames` frames at 8 kHz: a ramp on the
    /// left, its negative on the right, so content can be checked. Each
    /// call gets its own file: the tests run in parallel.
    fn wav(frames: usize) -> PathBuf {
        wav_with(frames, &[])
    }

    /// [`wav`] behind an ID3v2.3 tag of `(frame id, text)` pairs, the
    /// way a tagged MP3 begins: the probe reads the tag, then the
    /// container, and the tags reach the reader as they do for an MP3.
    /// (A WAV's own INFO list would be the natural fixture, but
    /// symphonia-format-riff 0.6.1 drops the log it parses it into.)
    fn wav_with(frames: usize, tags: &[(&[u8; 4], &str)]) -> PathBuf {
        static N: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut data = Vec::with_capacity(frames * 4);
        for i in 0..frames {
            let s = (i as i16).wrapping_mul(7);
            data.extend_from_slice(&s.to_le_bytes());
            data.extend_from_slice(&(-s).to_le_bytes());
        }
        let mut id3 = Vec::new();
        if !tags.is_empty() {
            let mut frames_ = Vec::new();
            for (id, text) in tags {
                frames_.extend_from_slice(*id);
                frames_
                    .extend_from_slice(&(text.len() as u32 + 1).to_be_bytes());
                frames_.extend_from_slice(&[0, 0]); // flags
                frames_.push(0); // ISO-8859-1
                frames_.extend_from_slice(text.as_bytes());
            }
            let size = frames_.len() as u32;
            id3.extend_from_slice(b"ID3\x03\x00\x00");
            id3.extend_from_slice(&[
                ((size >> 21) & 0x7f) as u8,
                ((size >> 14) & 0x7f) as u8,
                ((size >> 7) & 0x7f) as u8,
                (size & 0x7f) as u8,
            ]);
            id3.extend_from_slice(&frames_);
        }
        let mut b = id3;
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes()); // PCM
        b.extend_from_slice(&2u16.to_le_bytes()); // channels
        b.extend_from_slice(&RATE.to_le_bytes());
        b.extend_from_slice(&(RATE * 4).to_le_bytes());
        b.extend_from_slice(&4u16.to_le_bytes());
        b.extend_from_slice(&16u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&(data.len() as u32).to_le_bytes());
        b.extend_from_slice(&data);
        let dir = std::env::temp_dir()
            .join(format!("evo-esp32-symphonia-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ramp.wav");
        std::fs::write(&path, b).unwrap();
        path
    }

    fn rig(cap: usize) -> (Steward, Shared) {
        let tape = Shared(Arc::new(Mutex::new(Tape {
            cap,
            ..Tape::default()
        })));
        let mut s = Steward::new(catalogue(), Config::default()).unwrap();
        s.admit_warden(
            Box::new(PlaybackSymphonia::new(Box::new(tape.clone()))),
            Ticks(0),
        )
        .unwrap();
        (s, tape)
    }

    fn state(s: &Steward) -> State {
        let p = s.projection(ProjectionKey::Rack {
            name: shelf::PLAYBACK.into(),
        });
        from_value(&p.subjects[0].state).unwrap()
    }

    /// Assign the track; the open runs on its thread, the state is
    /// `Opening` until a tick finds it done.
    fn assign_play(
        s: &mut Steward,
        track: Track,
        start_ms: u64,
    ) -> Result<CustodyHandle, evo_esp32_steward::DispatchError> {
        s.assign(
            &shelf::playback_decode(),
            Assignment {
                subject: SubjectId::new("playback/now"),
                instruction: to_value(&Play { track, start_ms }),
            },
            Ticks(0),
        )
    }

    /// Assign the track and tick until the open thread has it playing
    /// (or has failed); the ticks step by the opening idle so the
    /// steward re-asks the warden each time. Gives the handle and the
    /// time of the tick that found it done: a test's clock goes on
    /// from there, since the steward skips a custody ticked before it
    /// is due, and under load the open takes more than one step.
    fn play(
        s: &mut Steward,
        track: Track,
        start_ms: u64,
    ) -> Result<(CustodyHandle, u64), evo_esp32_steward::DispatchError> {
        let h = assign_play(s, track, start_ms)?;
        let began = Instant::now();
        let mut t = 0;
        let mut last = 0;
        while state(s).status == Status::Opening {
            assert!(began.elapsed() < Duration::from_secs(5), "still opening");
            thread::sleep(Duration::from_millis(2));
            s.tick(Ticks(t));
            last = t;
            t += u64::from(OPENING_IDLE_MS);
        }
        Ok((h, last))
    }

    fn correct(s: &mut Steward, h: CustodyHandle, c: Correction, at: u64) {
        s.correct(
            h,
            CourseCorrection {
                instruction: to_value(&c),
            },
            Ticks(at),
        )
        .unwrap();
    }

    /// Tick from `t` (stepping by the decoding idle, so the warden is
    /// due each time) until `done` holds, within five seconds of real
    /// time; the decode thread works between the ticks. Returns the
    /// time of the last tick.
    fn until(
        s: &mut Steward,
        t: &mut u64,
        what: &str,
        mut done: impl FnMut(&Steward) -> bool,
    ) -> u64 {
        let began = Instant::now();
        loop {
            s.tick(Ticks(*t));
            if done(s) {
                return *t;
            }
            assert!(
                began.elapsed() < Duration::from_secs(5),
                "waited five seconds for: {what}"
            );
            thread::sleep(Duration::from_millis(2));
            *t += u64::from(DECODING_IDLE_MS);
        }
    }

    #[test]
    fn a_wav_plays_to_the_end_and_every_frame_reaches_the_sink() {
        let frames = 8000; // one second
        let path = wav(frames);
        let (mut s, tape) = rig(usize::MAX);
        play(&mut s, file_track(&path), 0).unwrap();
        let st = state(&s);
        assert_eq!(st.status, Status::Playing);
        assert_eq!(st.track.as_ref().unwrap().duration_ms, Some(1000));
        let f = st.format.unwrap();
        assert_eq!((f.sample_rate, f.channels, f.bits), (RATE, 2, 16));
        assert_eq!(f.codec, "pcm_s16le");

        let mut t = 1;
        until(&mut s, &mut t, "the track to end", |s| {
            state(s).status == Status::Stopped
        });
        let tape = tape.tape();
        assert_eq!(tape.frames.len(), frames * 2);
        assert_eq!(tape.frames[0..4], [0, 0, 7, -7]);
        assert_eq!(tape.stops, 1);
        assert_eq!(state(&s).status, Status::Stopped, "released at the end");
    }

    #[test]
    fn the_streams_tags_name_the_track() {
        let path = wav_with(
            800,
            &[
                (b"TIT2", "The Show Must Go On"),
                (b"TPE1", "Queen"),
                (b"TALB", "Greatest Hits III"),
            ],
        );
        let (mut s, _tape) = rig(usize::MAX);
        play(&mut s, file_track(&path), 0).unwrap();
        let track = state(&s).track.unwrap();
        assert_eq!(track.title, "The Show Must Go On");
        assert_eq!(track.artist.as_deref(), Some("Queen"));
        assert_eq!(track.album.as_deref(), Some("Greatest Hits III"));

        // No tags: the source's title stands, and nothing is invented.
        let (mut s, _tape) = rig(usize::MAX);
        play(&mut s, file_track(&wav(800)), 0).unwrap();
        let track = state(&s).track.unwrap();
        assert_eq!(track.title, "ramp");
        assert_eq!((track.artist, track.album), (None, None));
    }

    #[test]
    fn a_full_sink_holds_the_decoder_and_the_position_follows_the_ear() {
        let path = wav(8000);
        // The sink holds a tenth of a second; nothing is heard yet.
        let (mut s, tape) = rig(800);
        let (_, mut t) = play(&mut s, file_track(&path), 0).unwrap();
        until(&mut s, &mut t, "the sink to fill", |_| {
            tape.tape().frames.len() / 2 >= 800
        });
        thread::sleep(Duration::from_millis(50));
        let got = tape.tape().frames.len() / 2;
        assert_eq!(got, 800, "the full sink took no more");
        // Everything taken is still held: nothing has been heard.
        s.tick(Ticks(t + u64::from(DECODING_IDLE_MS)));
        let st = state(&s);
        assert_eq!(st.position_ms, 0);
        assert_eq!(st.status, Status::Playing);
        // The ear takes half; the position follows it, and the decoder
        // fills the room.
        tape.hear(400);
        t += 2 * u64::from(DECODING_IDLE_MS);
        until(&mut s, &mut t, "the room to be filled", |s| {
            tape.tape().frames.len() / 2 >= 1200
                && state(s).position_ms == 400 * 1000 / u64::from(RATE)
        });
    }

    #[test]
    fn pause_holds_and_resume_continues() {
        let path = wav(8000);
        let (mut s, tape) = rig(800);
        let (h, mut t) = play(&mut s, file_track(&path), 0).unwrap();
        until(&mut s, &mut t, "the sink to fill", |_| {
            tape.tape().frames.len() / 2 >= 800
        });
        correct(&mut s, h, Correction::Pause, t + 1);
        assert_eq!(state(&s).status, Status::Paused);
        // Room appears while paused; the decoder does not use it.
        thread::sleep(Duration::from_millis(60));
        tape.hear(800);
        thread::sleep(Duration::from_millis(60));
        let before = tape.tape().frames.len();
        assert_eq!(before / 2, 800, "nothing written while paused");
        t += 100;
        correct(&mut s, h, Correction::Resume, t);
        until(&mut s, &mut t, "frames after the resume", |_| {
            tape.tape().frames.len() > before
        });
        assert_eq!(state(&s).status, Status::Playing);
    }

    #[test]
    fn seek_flushes_the_sink_and_moves_the_position() {
        let path = wav(8000);
        let (mut s, tape) = rig(800);
        let (h, mut t) = play(&mut s, file_track(&path), 0).unwrap();
        until(&mut s, &mut t, "the sink to fill", |_| {
            tape.tape().frames.len() / 2 >= 800
        });
        correct(&mut s, h, Correction::Seek { position_ms: 500 }, t + 1);
        // The face hears the target at once; the thread flushes the
        // sink within a block and lands.
        assert_eq!(state(&s).position_ms, 500);
        until(
            &mut s,
            &mut t,
            "the flush and the first frames after it",
            |_| {
                let t = tape.tape();
                t.flushes == 1 && t.frames.len() >= 2
            },
        );
        let t = tape.tape();
        // The reader lands on the packet before 500 ms and the decoder
        // trims to the frame: the first sample after the flush is frame
        // 4000 of the ramp, not the packet's first.
        assert_eq!(t.frames[0], 4000i16.wrapping_mul(7));
        assert_eq!(t.frames[1], -(4000i16.wrapping_mul(7)));
    }

    #[test]
    fn a_start_offset_is_honoured_to_the_frame() {
        let path = wav(8000);
        let (mut s, tape) = rig(800);
        let (_, mut t) = play(&mut s, file_track(&path), 250).unwrap();
        assert_eq!(state(&s).position_ms, 250);
        until(&mut s, &mut t, "the first frames", |_| {
            tape.tape().frames.len() >= 2
        });
        assert_eq!(tape.tape().frames[0], 2000i16.wrapping_mul(7));
    }

    #[test]
    fn a_second_play_is_refused_and_a_bad_uri_is_an_error() {
        let path = wav(80);
        let (mut s, _tape) = rig(usize::MAX);
        play(&mut s, file_track(&path), 0).unwrap();
        assert!(play(&mut s, file_track(&path), 0).is_err());

        let (mut s2, _tape2) = rig(usize::MAX);
        let mut tr = file_track(&path);
        tr.uri = "http://x/y".into();
        assert!(play(&mut s2, tr, 0).is_err());
    }

    /// Serves `bytes` as a stream with `content-type`, or refuses with
    /// `refusal`.
    struct Served {
        bytes: Vec<u8>,
        content_type: &'static str,
        refusal: Option<&'static str>,
    }

    struct ServedBytes {
        bytes: Vec<u8>,
        at: usize,
        content_type: &'static str,
    }

    impl ByteStream for ServedBytes {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, String> {
            let n = buf.len().min(self.bytes.len() - self.at).min(512);
            buf[..n].copy_from_slice(&self.bytes[self.at..self.at + n]);
            self.at += n;
            Ok(n)
        }

        fn header(&self, name: &str) -> Option<String> {
            (name == "content-type").then(|| self.content_type.to_string())
        }
    }

    impl StreamOpener for Served {
        fn open(&self, _uri: &str) -> Result<Box<dyn ByteStream>, String> {
            if let Some(why) = self.refusal {
                return Err(why.to_string());
            }
            Ok(Box::new(ServedBytes {
                bytes: self.bytes.clone(),
                at: 0,
                content_type: self.content_type,
            }))
        }
    }

    fn rig_with(cap: usize, opener: Served) -> (Steward, Shared) {
        let tape = Shared(Arc::new(Mutex::new(Tape {
            cap,
            ..Tape::default()
        })));
        let mut s = Steward::new(catalogue(), Config::default()).unwrap();
        s.admit_warden(
            Box::new(
                PlaybackSymphonia::new(Box::new(tape.clone()))
                    .with_opener(Box::new(opener)),
            ),
            Ticks(0),
        )
        .unwrap();
        (s, tape)
    }

    /// Tick until the custody leaves `Opening`, or `limit` ticks.
    fn settle(s: &mut Steward, limit: u64) -> u64 {
        let mut t = 1;
        while state(s).status == Status::Opening && t < limit {
            s.tick(Ticks(t));
            thread::sleep(Duration::from_millis(1));
            t += 1;
        }
        t
    }

    #[test]
    fn a_stream_is_reached_on_its_own_thread_and_then_plays() {
        // Longer than the ring's prebuffer (48 KB), so the decoder is
        // let go before the stream ends: 4 s of 8 kHz stereo is 128 KB.
        let wav_bytes = std::fs::read(wav(32_000)).unwrap();
        let (mut s, tape) = rig_with(
            800,
            Served {
                bytes: wav_bytes,
                content_type: "audio/wav",
                refusal: None,
            },
        );
        let mut tr = file_track(Path::new("/x/station.wav"));
        tr.uri = "http://x/station".into();
        tr.title = "x".into();
        assign_play(&mut s, tr, 0).unwrap();
        // Back at once, nothing out yet.
        assert_eq!(state(&s).status, Status::Opening);
        assert!(tape.tape().frames.is_empty());
        let t = settle(&mut s, 5000);
        assert_eq!(state(&s).status, Status::Playing);
        let st = state(&s);
        assert_eq!(st.format.unwrap().sample_rate, RATE);
        // A station's name is its album line.
        assert_eq!(st.track.unwrap().album.as_deref(), Some("x"));
        let mut t = t;
        until(&mut s, &mut t, "the sink to fill", |_| {
            tape.tape().frames.len() >= 800
        });
    }

    #[test]
    fn a_stream_that_cannot_be_reached_ends_the_custody_with_the_reason() {
        let (mut s, tape) = rig_with(
            800,
            Served {
                bytes: vec![],
                content_type: "audio/wav",
                refusal: Some("HTTP_CONNECT: no route"),
            },
        );
        let mut tr = file_track(Path::new("/x/station.wav"));
        tr.uri = "http://x/station".into();
        let h = assign_play(&mut s, tr, 0).unwrap();
        assert_eq!(state(&s).status, Status::Opening);
        settle(&mut s, 5000);
        assert_eq!(state(&s).status, Status::Stopped);
        assert!(s.custodies().all(|c| c != h), "released");
        assert_eq!(tape.tape().stops, 0, "a sink never started is not stopped");
        let released = s
            .happenings_since(evo_esp32_sdk::happening::Seq(0))
            .happenings
            .into_iter()
            .find_map(|h| match h.kind {
                evo_esp32_sdk::happening::HappeningKind::CustodyReleased {
                    reason,
                    ..
                } => Some(reason),
                _ => None,
            })
            .expect("a release happened");
        assert_eq!(released.as_deref(), Some("HTTP_CONNECT: no route"));
    }

    #[test]
    fn a_pause_while_opening_starts_the_stream_held() {
        let wav_bytes = std::fs::read(wav(8000)).unwrap();
        let (mut s, tape) = rig_with(
            800,
            Served {
                bytes: wav_bytes,
                content_type: "audio/wav",
                refusal: None,
            },
        );
        let mut tr = file_track(Path::new("/x/station.wav"));
        tr.uri = "http://x/station".into();
        let (h, t0) = play(&mut s, tr, 0).unwrap();
        correct(&mut s, h, Correction::Pause, t0 + 1);
        settle(&mut s, t0 + 5000);
        thread::sleep(Duration::from_millis(60));
        assert_eq!(state(&s).status, Status::Paused);
        assert!(tape.tape().frames.is_empty());
    }
}

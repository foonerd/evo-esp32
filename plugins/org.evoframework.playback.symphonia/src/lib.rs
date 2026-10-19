// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! A playback warden that decodes with Symphonia.
//!
//! `org.evoframework.playback.symphonia` stocks `playback/decode`. Handed
//! a [`Play`] it opens the track's `file://` URI, probes the container,
//! makes a decoder for its first audio track and, tick by tick, decodes a
//! bounded slice and offers the frames to the [`FrameSink`] the
//! distribution gave it. The sink paces: when it takes less than offered
//! the warden keeps the remainder and asks to be ticked again shortly, so
//! a tick is never longer than a few packets' decode and a copy.
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
#![forbid(unsafe_code)]

use std::fs::File;
use std::path::{Path, PathBuf};

use evo_esp32_sdk::context::Context;
use evo_esp32_sdk::contract::audio::playback::{
    Correction, Format, FrameSink, Play, State, Status, KIND,
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

/// The plugin's name, as the catalogue and the steward know it.
pub const NAME: &str = "org.evoframework.playback.symphonia";

/// How long the warden asks to be left alone while paused.
const PAUSED_IDLE_MS: u32 = 250;
/// How long to wait when the sink took nothing.
const SINK_FULL_IDLE_MS: u32 = 20;
/// The most audio one tick decodes, in milliseconds. Past this the
/// warden yields even if the sink would take more.
const TICK_BUDGET_MS: u64 = 200;

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
}

/// The custody in hand.
struct InHand {
    handle: CustodyHandle,
    state: State,
    stream: Stream,
    /// Interleaved samples decoded but not yet taken by the sink.
    pending: Vec<i16>,
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
}

impl InHand {
    fn position_ms(&self) -> u64 {
        let rate = self.state.format.as_ref().map_or(0, |f| f.sample_rate);
        if rate == 0 {
            return self.base_ms;
        }
        self.base_ms + self.frames_out * 1000 / u64::from(rate)
    }

    fn channels(&self) -> usize {
        self.state
            .format
            .as_ref()
            .map_or(1, |f| usize::from(f.channels).max(1))
    }
}

pub use symphonia::core::codecs::registry::CodecRegistry;

/// The codecs Symphonia ships, as this crate enables them: the registry a
/// distribution starts from before preferring one of its own.
pub fn default_codecs() -> CodecRegistry {
    let mut registry = CodecRegistry::new();
    symphonia::default::register_enabled_codecs(&mut registry);
    registry
}

/// The Symphonia playback warden.
pub struct PlaybackSymphonia {
    sink: Box<dyn FrameSink>,
    codecs: CodecRegistry,
    subject: Option<SubjectId>,
    in_hand: Option<InHand>,
}

impl PlaybackSymphonia {
    /// A plugin ready to be admitted, writing to `sink`, decoding with
    /// Symphonia's own codecs.
    pub fn new(sink: Box<dyn FrameSink>) -> Self {
        Self::with_codecs(sink, default_codecs())
    }

    /// As [`new`](Self::new), decoding with `codecs`: a distribution's
    /// registry, where a platform decoder may stand above Symphonia's.
    pub fn with_codecs(
        sink: Box<dyn FrameSink>,
        codecs: CodecRegistry,
    ) -> Self {
        Self {
            sink,
            codecs,
            subject: None,
            in_hand: None,
        }
    }

    fn publish(&self, ctx: &mut dyn Context, state: &State) {
        if let Some(id) = &self.subject {
            let _ = ctx.update(id, to_value(state));
        }
    }

    fn publish_in_hand(&self, ctx: &mut dyn Context) {
        if let Some(h) = &self.in_hand {
            let mut state = h.state.clone();
            state.position_ms = h.position_ms();
            self.publish(ctx, &state);
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
        if self.in_hand.take().is_some() {
            self.sink.stop();
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
        let mut track = play.track;
        let (mut stream, format) = open(&track.uri, &self.codecs)?;
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
        let (base_ms, skip) = if play.start_ms > 0 {
            seek(&mut stream, play.start_ms, format.sample_rate)?
        } else {
            (0, 0)
        };
        self.sink.start(&format);
        let state = State {
            status: Status::Playing,
            track: Some(track),
            position_ms: base_ms,
            format: Some(format),
        };
        self.publish(ctx, &state);
        self.in_hand = Some(InHand {
            handle,
            state,
            stream,
            pending: Vec::new(),
            taken: 0,
            frames_out: 0,
            base_ms,
            skip,
            ended: false,
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
            Correction::Pause => h.state.status = Status::Paused,
            Correction::Resume => h.state.status = Status::Playing,
            Correction::Seek { position_ms } => {
                let cap = h.state.track.as_ref().and_then(|t| t.duration_ms);
                let target = cap.map_or(position_ms, |d| position_ms.min(d));
                let rate = h.state.format.as_ref().map_or(0, |f| f.sample_rate);
                let (landed, skip) = seek(&mut h.stream, target, rate)?;
                h.pending.clear();
                h.taken = 0;
                h.frames_out = 0;
                h.base_ms = landed;
                h.skip = skip;
                h.ended = false;
                self.sink.flush();
            }
        }
        self.publish_in_hand(ctx);
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
        if h.state.status == Status::Paused {
            return Progress::Idle(PAUSED_IDLE_MS);
        }
        let rate =
            u64::from(h.state.format.as_ref().map_or(0, |f| f.sample_rate));
        let budget_frames = (rate * TICK_BUDGET_MS / 1000).max(1);
        let mut decoded_frames: u64 = 0;
        let progress = loop {
            // Offer what is pending before decoding more.
            if h.taken < h.pending.len() {
                let channels = h.channels();
                let frames = self.sink.write(&h.pending[h.taken..]);
                if frames == 0 {
                    break Progress::Idle(SINK_FULL_IDLE_MS);
                }
                h.taken += frames * channels;
                h.frames_out += frames as u64;
                if h.taken < h.pending.len() {
                    continue;
                }
                h.pending.clear();
                h.taken = 0;
            }
            if h.ended {
                break Progress::Done;
            }
            if decoded_frames >= budget_frames {
                break Progress::Working;
            }
            match next_frames(&mut h.stream, &mut h.pending) {
                Ok(Some(frames)) => {
                    decoded_frames += frames;
                    h.taken = 0;
                    if h.skip > 0 {
                        let drop = h.skip.min(frames);
                        h.taken = drop as usize * h.channels();
                        h.skip -= drop;
                    }
                }
                Ok(None) => h.ended = true,
                Err(e) => {
                    // The stream is lost. Say so in the state and let the
                    // steward release custody.
                    log::warn!("decode: {e}; releasing");
                    h.state.status = Status::Stopped;
                    if let Some(t) = h.state.track.as_mut() {
                        t.title = format!("{} ({e})", t.title);
                    }
                    h.pending.clear();
                    h.taken = 0;
                    h.ended = true;
                    break Progress::Done;
                }
            }
        };
        self.publish_in_hand(ctx);
        progress
    }
}

/// Open a `file://` URI: probe it, pick its default audio track, make a
/// decoder, and report the format the stream will come out in.
fn open(
    uri: &str,
    codecs: &CodecRegistry,
) -> Result<(Stream, Format), CustodyError> {
    let path = file_path(uri)?;
    let file = File::open(&path)
        .map_err(|e| CustodyError(format!("{}: {e}", path.display())))?;
    let mss = MediaSourceStream::new(
        Box::new(file),
        MediaSourceStreamOptions::default(),
    );
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let reader = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| CustodyError(format!("probe: {e}")))?;
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
    stream.decoder.reset();
    let landed_ms = stream
        .time_base
        .and_then(|tb| tb.calc_time(landed.actual_ts))
        .map_or(ms, time_ms);
    let skip = ms.saturating_sub(landed_ms) * u64::from(rate) / 1000;
    Ok((ms, skip))
}

/// Decode the next packet of the track into `out` as interleaved i16.
/// `Ok(Some(frames))` for frames appended, `Ok(None)` at the end of the
/// stream; malformed packets are skipped, not errors.
fn next_frames(
    stream: &mut Stream,
    out: &mut Vec<i16>,
) -> Result<Option<u64>, SymphoniaError> {
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
                buf.copy_to_vec_interleaved::<i16>(out);
                return Ok(Some(frames));
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
    use std::rc::Rc;

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

    /// A sink that takes up to `room` frames a write and keeps them all.
    #[derive(Default)]
    struct Tape {
        room: usize,
        format: Option<Format>,
        frames: Vec<i16>,
        flushes: u32,
        stops: u32,
    }

    #[derive(Clone)]
    struct Shared(Rc<RefCell<Tape>>);

    impl FrameSink for Shared {
        fn start(&mut self, format: &Format) {
            self.0.borrow_mut().format = Some(format.clone());
        }
        fn write(&mut self, interleaved: &[i16]) -> usize {
            let mut t = self.0.borrow_mut();
            let ch = t.format.as_ref().map_or(1, |f| usize::from(f.channels));
            let frames = (interleaved.len() / ch).min(t.room);
            t.frames.extend_from_slice(&interleaved[..frames * ch]);
            frames
        }
        fn flush(&mut self) {
            let mut t = self.0.borrow_mut();
            t.flushes += 1;
            t.frames.clear();
        }
        fn stop(&mut self) {
            self.0.borrow_mut().stops += 1;
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

    fn rig(room: usize) -> (Steward, Shared) {
        let tape = Shared(Rc::new(RefCell::new(Tape {
            room,
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

    fn play(
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
        while s.tick(Ticks(t)).is_some() && t < 10_000 {
            t += 1;
        }
        let tape = tape.0.borrow();
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
    fn a_full_sink_bounds_the_tick_and_the_position_follows_the_sink() {
        let path = wav(8000);
        // 800 frames a write: a tenth of a second per tick.
        let (mut s, tape) = rig(800);
        play(&mut s, file_track(&path), 0).unwrap();
        s.tick(Ticks(1));
        let got = tape.0.borrow().frames.len() / 2;
        assert!((800..8000).contains(&got), "took {got} frames");
        let st = state(&s);
        assert_eq!(st.position_ms, got as u64 * 1000 / u64::from(RATE));
        assert_eq!(st.status, Status::Playing);
    }

    #[test]
    fn pause_holds_and_resume_continues() {
        let path = wav(8000);
        let (mut s, tape) = rig(800);
        let h = play(&mut s, file_track(&path), 0).unwrap();
        s.tick(Ticks(1));
        let before = tape.0.borrow().frames.len();
        correct(&mut s, h, Correction::Pause, 2);
        s.tick(Ticks(100));
        assert_eq!(tape.0.borrow().frames.len(), before);
        assert_eq!(state(&s).status, Status::Paused);
        correct(&mut s, h, Correction::Resume, 300);
        s.tick(Ticks(300));
        assert!(tape.0.borrow().frames.len() > before);
    }

    #[test]
    fn seek_flushes_the_sink_and_moves_the_position() {
        let path = wav(8000);
        let (mut s, tape) = rig(800);
        let h = play(&mut s, file_track(&path), 0).unwrap();
        s.tick(Ticks(1));
        correct(&mut s, h, Correction::Seek { position_ms: 500 }, 2);
        assert_eq!(tape.0.borrow().flushes, 1);
        assert_eq!(state(&s).position_ms, 500);
        s.tick(Ticks(3));
        let t = tape.0.borrow();
        // The reader lands on the packet before 500 ms and the warden
        // trims to the frame: the first sample after the flush is frame
        // 4000 of the ramp, not the packet's first.
        assert_eq!(t.frames[0], 4000i16.wrapping_mul(7));
        assert_eq!(t.frames[1], -(4000i16.wrapping_mul(7)));
    }

    #[test]
    fn a_start_offset_is_honoured_to_the_frame() {
        let path = wav(8000);
        let (mut s, tape) = rig(800);
        play(&mut s, file_track(&path), 250).unwrap();
        assert_eq!(state(&s).position_ms, 250);
        s.tick(Ticks(1));
        assert_eq!(tape.0.borrow().frames[0], 2000i16.wrapping_mul(7));
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
}

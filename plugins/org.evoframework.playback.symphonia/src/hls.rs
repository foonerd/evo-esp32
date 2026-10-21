// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! HTTP Live Streaming as one byte stream.
//!
//! A station served over HLS is a playlist of short segments, refreshed
//! as the station goes on. [`Aware`] wraps the distribution's transport:
//! a URI that answers with a playlist (by its content type, or its
//! `.m3u8` name) becomes a [`Segments`] stream, which walks the media
//! playlist, fetches each segment in turn, demuxes the transport stream
//! ([`ts`](crate::ts)) and hands the audio elementary stream on as if
//! the station were one long response, with the content type the
//! segments carry. Anything else passes through untouched. A master
//! playlist is resolved to the audio variant nearest [`PREFERRED_BPS`]
//! from below. Segments in fragmented MP4 (`#EXT-X-MAP`) are not read.

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use evo_esp32_sdk::contract::audio::playback::{ByteStream, StreamOpener};
use log::{info, warn};

use crate::ts::{Audio, Demuxer};

/// The variant taken from a master playlist: the highest bandwidth at
/// or under this, else the lowest there is. The BBC offers 48, 96, 128
/// and 320 kb/s AAC; 128 is the one a radio asks for.
pub const PREFERRED_BPS: u64 = 160_000;
/// The most a playlist may be.
const PLAYLIST_LIMIT: usize = 256 * 1024;
/// A segment is read in pieces of this size.
const PIECE: usize = 4096;
/// How far back from the live edge a stream starts: three segments,
/// as the specification asks, so the next is ready when this ends.
const BEHIND_LIVE: usize = 3;
/// How many playlist refreshes with nothing new before the stream is
/// taken to have stalled.
const STALE_REFRESHES: u32 = 10;

/// What a URI's response says about being a playlist.
fn is_playlist(uri: &str, content_type: Option<&str>) -> bool {
    let kind = content_type
        .map(|s| {
            s.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        })
        .unwrap_or_default();
    matches!(
        kind.as_str(),
        "application/vnd.apple.mpegurl"
            | "application/x-mpegurl"
            | "audio/mpegurl"
            | "audio/x-mpegurl"
    ) || uri
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .ends_with(".m3u8")
}

/// `reference` against `base` (a playlist's URL): absolute as is, a
/// leading slash from the host, else from the playlist's folder.
fn resolve(base: &str, reference: &str) -> String {
    if reference.contains("://") {
        return reference.to_string();
    }
    let scheme_end = base.find("://").map_or(0, |i| i + 3);
    if let Some(path) = reference.strip_prefix('/') {
        let host_end = base[scheme_end..]
            .find('/')
            .map_or(base.len(), |i| scheme_end + i);
        return format!("{}/{path}", &base[..host_end]);
    }
    match base.rfind('/') {
        Some(i) if i >= scheme_end => format!("{}{reference}", &base[..=i]),
        _ => format!("{base}/{reference}"),
    }
}

/// One entry of a master playlist.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Variant {
    bandwidth: u64,
    codecs: String,
    uri: String,
}

/// A media playlist as last read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Media {
    target_duration: Duration,
    first_sequence: u64,
    segments: Vec<String>,
    ended: bool,
    fragmented: bool,
}

impl Media {
    fn sequence_after_last(&self) -> u64 {
        self.first_sequence + self.segments.len() as u64
    }

    fn segment(&self, sequence: u64) -> Option<&str> {
        let at = sequence.checked_sub(self.first_sequence)? as usize;
        self.segments.get(at).map(String::as_str)
    }
}

/// A playlist, master or media.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Playlist {
    Master(Vec<Variant>),
    Media(Media),
}

fn attribute<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    // ATTR=value pairs, values possibly quoted and containing commas.
    let mut rest = line;
    while !rest.is_empty() {
        let (key, after) = rest.split_once('=')?;
        let (value, next) = if let Some(q) = after.strip_prefix('"') {
            let end = q.find('"')?;
            (&q[..end], q[end + 1..].trim_start_matches(','))
        } else {
            match after.find(',') {
                Some(c) => (&after[..c], &after[c + 1..]),
                None => (after, ""),
            }
        };
        if key.trim() == name {
            return Some(value);
        }
        rest = next;
    }
    None
}

fn parse(text: &str, base: &str) -> Result<Playlist, String> {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    if lines.next() != Some("#EXTM3U") {
        return Err("not an M3U8 playlist".to_string());
    }
    let mut variants = Vec::new();
    let mut media = Media {
        target_duration: Duration::from_secs(6),
        ..Media::default()
    };
    let mut pending_variant: Option<(u64, String)> = None;
    let mut pending_segment = false;
    for line in lines {
        if let Some(info) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            let bandwidth = attribute(info, "BANDWIDTH")
                .and_then(|b| b.parse().ok())
                .unwrap_or(0);
            let codecs = attribute(info, "CODECS").unwrap_or("").to_string();
            pending_variant = Some((bandwidth, codecs));
        } else if let Some(d) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            if let Ok(s) = d.trim().parse::<u64>() {
                media.target_duration = Duration::from_secs(s.max(1));
            }
        } else if let Some(n) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            media.first_sequence = n.trim().parse().unwrap_or(0);
        } else if line.starts_with("#EXTINF:") {
            pending_segment = true;
        } else if line == "#EXT-X-ENDLIST" {
            media.ended = true;
        } else if line.starts_with("#EXT-X-MAP:") {
            media.fragmented = true;
        } else if line.starts_with('#') {
            // Other tags carry nothing this reader needs.
        } else if let Some((bandwidth, codecs)) = pending_variant.take() {
            variants.push(Variant {
                bandwidth,
                codecs,
                uri: resolve(base, line),
            });
        } else if pending_segment {
            pending_segment = false;
            media.segments.push(resolve(base, line));
        }
    }
    if !variants.is_empty() {
        Ok(Playlist::Master(variants))
    } else if !media.segments.is_empty() {
        Ok(Playlist::Media(media))
    } else {
        Err("a playlist with neither variants nor segments".to_string())
    }
}

/// The variant to play: audio-only when the playlist says what each
/// carries, the highest bandwidth not above [`PREFERRED_BPS`], else the
/// lowest.
fn choose(variants: &[Variant]) -> Option<&Variant> {
    let audio_only: Vec<&Variant> = variants
        .iter()
        .filter(|v| {
            v.codecs.is_empty()
                || !v.codecs.contains("avc") && !v.codecs.contains("hvc")
        })
        .collect();
    let pool = if audio_only.is_empty() {
        variants.iter().collect()
    } else {
        audio_only
    };
    pool.iter()
        .filter(|v| v.bandwidth <= PREFERRED_BPS)
        .max_by_key(|v| v.bandwidth)
        .or_else(|| pool.iter().min_by_key(|v| v.bandwidth))
        .copied()
}

/// The whole of a small response, bounded.
fn read_all(
    stream: &mut dyn ByteStream,
    limit: usize,
) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    let mut piece = vec![0u8; PIECE];
    loop {
        let n = stream.read(&mut piece)?;
        if n == 0 {
            return Ok(bytes);
        }
        bytes.extend_from_slice(&piece[..n]);
        if bytes.len() > limit {
            return Err(format!("more than {} KB", limit / 1024));
        }
    }
}

fn fetch_text(opener: &dyn StreamOpener, uri: &str) -> Result<String, String> {
    let mut stream = opener.open(uri)?;
    let bytes = read_all(stream.as_mut(), PLAYLIST_LIMIT)?;
    String::from_utf8(bytes)
        .map_err(|_| "a playlist that is not UTF-8".to_string())
}

/// The segments of a media playlist, as one stream of the audio
/// elementary stream they carry.
pub struct Segments {
    opener: Arc<dyn StreamOpener>,
    media_uri: String,
    media: Media,
    refreshed: Instant,
    /// The next segment to fetch.
    sequence: u64,
    segment: Option<Box<dyn ByteStream>>,
    demuxer: Demuxer,
    out: Vec<u8>,
    /// Where the next read of `out` starts.
    at: usize,
    piece: Vec<u8>,
    bitrate_kbps: Option<u32>,
    /// Refreshes in a row that brought no new segment.
    stale: u32,
}

impl Segments {
    /// From a playlist already fetched from `uri`; a master playlist is
    /// followed to its chosen variant.
    pub fn start(
        opener: Arc<dyn StreamOpener>,
        uri: &str,
        text: &str,
    ) -> Result<Self, String> {
        let (media_uri, media, bitrate_kbps) = match parse(text, uri)? {
            Playlist::Media(media) => (uri.to_string(), media, None),
            Playlist::Master(variants) => {
                let variant =
                    choose(&variants).ok_or("an empty master playlist")?;
                info!(
                    "hls: {} variants, taking {} kb/s{}",
                    variants.len(),
                    variant.bandwidth / 1000,
                    if variant.codecs.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", variant.codecs)
                    }
                );
                let text = fetch_text(opener.as_ref(), &variant.uri)?;
                match parse(&text, &variant.uri)? {
                    Playlist::Media(media) => (
                        variant.uri.clone(),
                        media,
                        Some((variant.bandwidth / 1000) as u32),
                    ),
                    Playlist::Master(_) => {
                        return Err(
                            "a master playlist inside a master playlist"
                                .to_string(),
                        )
                    }
                }
            }
        };
        if media.fragmented {
            return Err(
                "HLS in fragmented MP4 is not played; MPEG-TS segments are"
                    .to_string(),
            );
        }
        let behind = if media.ended {
            media.segments.len()
        } else {
            BEHIND_LIVE
        };
        let sequence =
            media.sequence_after_last().saturating_sub(behind as u64);
        info!(
            "hls: {} segments of {} s from sequence {}, starting at {}{}",
            media.segments.len(),
            media.target_duration.as_secs(),
            media.first_sequence,
            sequence,
            if media.ended { " (ended)" } else { " (live)" }
        );
        Ok(Self {
            opener,
            media_uri,
            media,
            refreshed: Instant::now(),
            sequence,
            segment: None,
            demuxer: Demuxer::new(),
            out: Vec::new(),
            at: 0,
            piece: vec![0u8; PIECE],
            bitrate_kbps,
            stale: 0,
        })
    }

    fn refresh(&mut self) -> Result<(), String> {
        let text = fetch_text(self.opener.as_ref(), &self.media_uri)?;
        match parse(&text, &self.media_uri)? {
            Playlist::Media(media) => {
                if media.sequence_after_last()
                    > self.media.sequence_after_last()
                {
                    self.stale = 0;
                } else {
                    self.stale += 1;
                }
                // A playlist that jumped ahead of us (a long stall, or
                // the server's window moved on): take up from behind
                // its live edge again.
                if self.sequence < media.first_sequence {
                    warn!(
                        "hls: sequence {} has left the playlist (now from {}); skipping ahead",
                        self.sequence, media.first_sequence
                    );
                    self.sequence = media
                        .sequence_after_last()
                        .saturating_sub(BEHIND_LIVE as u64)
                        .max(media.first_sequence);
                }
                self.media = media;
                self.refreshed = Instant::now();
                Ok(())
            }
            Playlist::Master(_) => {
                Err("the media playlist became a master playlist".to_string())
            }
        }
    }

    /// Open the next segment, refreshing the playlist and waiting for the
    /// station as needed.
    fn next_segment(&mut self) -> Result<Option<Box<dyn ByteStream>>, String> {
        loop {
            if let Some(uri) = self.media.segment(self.sequence) {
                let uri = uri.to_string();
                let stream = self.opener.open(&uri)?;
                self.sequence += 1;
                return Ok(Some(stream));
            }
            if self.media.ended {
                return Ok(None);
            }
            if self.stale >= STALE_REFRESHES {
                return Err("the playlist stopped moving".to_string());
            }
            // Not yet published: wait out what is left of a segment's
            // time since the last refresh, then look again.
            let since = self.refreshed.elapsed();
            let wait = (self.media.target_duration / 2).saturating_sub(since);
            if !wait.is_zero() {
                thread::sleep(wait.min(Duration::from_secs(3)));
            }
            self.refresh()?;
        }
    }
}

impl ByteStream for Segments {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, String> {
        loop {
            if self.at < self.out.len() {
                let n = (self.out.len() - self.at).min(buf.len());
                buf[..n].copy_from_slice(&self.out[self.at..self.at + n]);
                self.at += n;
                if self.at == self.out.len() {
                    self.out.clear();
                    self.at = 0;
                }
                return Ok(n);
            }
            if let Some(segment) = self.segment.as_mut() {
                let n = segment.read(&mut self.piece)?;
                if n == 0 {
                    self.segment = None;
                    continue;
                }
                self.demuxer.push(&self.piece[..n], &mut self.out)?;
                continue;
            }
            match self.next_segment()? {
                Some(segment) => self.segment = Some(segment),
                None => return Ok(0),
            }
        }
    }

    fn header(&self, name: &str) -> Option<String> {
        match name {
            // Before the first segment is read the map is not seen; the
            // BBC's and most stations' segments are AAC, and the decoder
            // probes the bytes besides.
            "content-type" => Some(
                self.demuxer
                    .audio()
                    .unwrap_or(Audio::Adts)
                    .content_type()
                    .to_string(),
            ),
            "icy-br" => self.bitrate_kbps.map(|k| k.to_string()),
            _ => None,
        }
    }
}

/// A transport that plays HLS playlists as streams and passes every
/// other URI through.
pub struct Aware(pub Arc<dyn StreamOpener>);

impl StreamOpener for Aware {
    fn open(&self, uri: &str) -> Result<Box<dyn ByteStream>, String> {
        let mut stream = self.0.open(uri)?;
        let kind = stream.header("content-type");
        if !is_playlist(uri, kind.as_deref()) {
            return Ok(stream);
        }
        let bytes = read_all(stream.as_mut(), PLAYLIST_LIMIT)?;
        let text = String::from_utf8(bytes)
            .map_err(|_| "a playlist that is not UTF-8".to_string())?;
        // Reading the first segment here lets the stream say what it
        // carries before the decoder probes it.
        let mut segments = Segments::start(self.0.clone(), uri, &text)?;
        let first = segments.next_segment()?;
        segments.segment = first;
        Ok(Box::new(segments))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[test]
    fn references_resolve_against_the_playlist() {
        let base = "https://h.example/pool/live/radio/radio.m3u8?x=1";
        assert_eq!(
            resolve(base, "seg1.ts"),
            "https://h.example/pool/live/radio/seg1.ts"
        );
        assert_eq!(
            resolve(base, "/other/seg.ts"),
            "https://h.example/other/seg.ts"
        );
        assert_eq!(resolve(base, "http://cdn/seg.ts"), "http://cdn/seg.ts");
        assert_eq!(
            resolve("https://h.example", "a.m3u8"),
            "https://h.example/a.m3u8"
        );
    }

    #[test]
    fn playlists_parse_and_the_variant_is_chosen() {
        let master = "#EXTM3U\n#EXT-X-VERSION:3\n\
            #EXT-X-STREAM-INF:BANDWIDTH=48000,CODECS=\"mp4a.40.5\"\nlow/pl.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=128000,CODECS=\"mp4a.40.2\"\nmid/pl.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=320000,CODECS=\"mp4a.40.2\"\nhigh/pl.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=96000,CODECS=\"avc1.4d401f,mp4a.40.2\"\nvideo/pl.m3u8\n";
        let Playlist::Master(variants) =
            parse(master, "https://x/a/master.m3u8").unwrap()
        else {
            panic!()
        };
        assert_eq!(variants.len(), 4);
        let v = choose(&variants).unwrap();
        assert_eq!(v.bandwidth, 128000);
        assert_eq!(v.uri, "https://x/a/mid/pl.m3u8");
        let only_high = vec![variants[2].clone()];
        assert_eq!(choose(&only_high).unwrap().bandwidth, 320000);

        let media = "#EXTM3U\n#EXT-X-TARGETDURATION:7\n#EXT-X-MEDIA-SEQUENCE:1040\n\
            #EXTINF:6.4,\nseg1040.ts\n#EXTINF:6.4,\nseg1041.ts\n#EXT-X-PROGRAM-DATE-TIME:2026-10-21T10:00:00Z\n#EXTINF:6.4,\nseg1042.ts\n";
        let Playlist::Media(m) =
            parse(media, "https://x/a/mid/pl.m3u8").unwrap()
        else {
            panic!()
        };
        assert_eq!(m.first_sequence, 1040);
        assert_eq!(m.target_duration, Duration::from_secs(7));
        assert_eq!(m.segments.len(), 3);
        assert_eq!(m.segment(1042), Some("https://x/a/mid/seg1042.ts"));
        assert_eq!(m.segment(1039), None);
        assert!(!m.ended);
        assert!(parse("nope", "u").is_err());
        assert!(is_playlist("http://x/r.m3u8", None));
        assert!(is_playlist(
            "http://x/r",
            Some("application/vnd.apple.mpegurl; charset=utf-8")
        ));
        assert!(!is_playlist("http://x/r.mp3", Some("audio/mpeg")));
    }

    /// A server of canned responses by URI, counting fetches.
    struct Canned {
        responses: Mutex<HashMap<String, (Option<String>, Vec<u8>)>>,
        fetched: Mutex<Vec<String>>,
    }

    struct CannedStream {
        kind: Option<String>,
        bytes: Vec<u8>,
        at: usize,
    }

    impl ByteStream for CannedStream {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, String> {
            let n = (self.bytes.len() - self.at).min(buf.len()).min(100);
            buf[..n].copy_from_slice(&self.bytes[self.at..self.at + n]);
            self.at += n;
            Ok(n)
        }
        fn header(&self, name: &str) -> Option<String> {
            (name == "content-type")
                .then(|| self.kind.clone())
                .flatten()
        }
    }

    impl StreamOpener for Canned {
        fn open(&self, uri: &str) -> Result<Box<dyn ByteStream>, String> {
            self.fetched.lock().unwrap().push(uri.to_string());
            let r = self.responses.lock().unwrap();
            let (kind, bytes) =
                r.get(uri).ok_or_else(|| format!("404 {uri}"))?;
            Ok(Box::new(CannedStream {
                kind: kind.clone(),
                bytes: bytes.clone(),
                at: 0,
            }))
        }
    }

    /// A segment: PAT, PMT with ADTS audio, one PES with `es`.
    fn segment(es: &[u8]) -> Vec<u8> {
        fn packet(pid: u16, start: bool, payload: &[u8]) -> Vec<u8> {
            let mut p = vec![
                0x47,
                (if start { 0x40 } else { 0 }) | (pid >> 8) as u8,
                pid as u8,
            ];
            let pad = 184 - payload.len();
            if pad == 0 {
                p.push(0x10);
            } else {
                p.push(0x30);
                p.push((pad - 1) as u8);
                if pad > 1 {
                    p.push(0);
                    p.extend(std::iter::repeat_n(0xFF, pad - 2));
                }
            }
            p.extend_from_slice(payload);
            p
        }
        fn section(table_id: u8, body: &[u8]) -> Vec<u8> {
            let length = 5 + body.len() + 4;
            let mut s = vec![
                0u8,
                table_id,
                0xB0 | ((length >> 8) as u8 & 0x0F),
                length as u8,
            ];
            s.extend_from_slice(&[0, 1, 0xC1, 0, 0]);
            s.extend_from_slice(body);
            s.extend_from_slice(&[0, 0, 0, 0]);
            s
        }
        let mut out = Vec::new();
        out.extend(packet(0, true, &section(0x00, &[0x00, 0x01, 0xE1, 0x00])));
        out.extend(packet(
            0x100,
            true,
            &section(
                0x02,
                &[0xE1, 0x01, 0xF0, 0x00, 0x0F, 0xE1, 0x01, 0xF0, 0x00],
            ),
        ));
        let mut pes =
            vec![0, 0, 1, 0xC0, 0, 0, 0x80, 0x80, 5, 0x21, 0, 1, 0, 1];
        pes.extend_from_slice(es);
        let mut first = true;
        for chunk in pes.chunks(184) {
            out.extend(packet(0x101, first, chunk));
            first = false;
        }
        out
    }

    #[test]
    fn a_live_station_is_walked_segment_by_segment() {
        let base = "http://s/live/";
        let mut responses = HashMap::new();
        let master = format!(
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=128000,CODECS=\"mp4a.40.2\"\n{base}a.m3u8\n"
        );
        responses.insert(
            format!("{base}master.m3u8"),
            (
                Some("application/vnd.apple.mpegurl".to_string()),
                master.into_bytes(),
            ),
        );
        // Five segments published, each one byte-pattern apart; the
        // playlist shows the last three, as a live window does.
        let es: Vec<Vec<u8>> = (0..5u8).map(|i| vec![i + 1; 250]).collect();
        for (i, e) in es.iter().enumerate() {
            responses.insert(
                format!("{base}seg{i}.ts"),
                (Some("video/MP2T".to_string()), segment(e)),
            );
        }
        let window = |from: usize, to: usize| {
            let mut t = format!("#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:{from}\n");
            for i in from..to {
                t.push_str(&format!("#EXTINF:1.0,\nseg{i}.ts\n"));
            }
            t
        };
        responses
            .insert(format!("{base}a.m3u8"), (None, window(0, 3).into_bytes()));
        let canned = Arc::new(Canned {
            responses: Mutex::new(responses),
            fetched: Mutex::new(Vec::new()),
        });
        let aware = Aware(canned.clone());
        let mut stream = aware.open(&format!("{base}master.m3u8")).unwrap();
        assert_eq!(stream.header("content-type").as_deref(), Some("audio/aac"));
        assert_eq!(stream.header("icy-br").as_deref(), Some("128"));

        // Three segments behind the live edge of a three-segment window
        // is the first one; the whole window reads through in order.
        let mut got = Vec::new();
        let mut buf = [0u8; 300];
        while got.len() < 750 {
            let n = stream.read(&mut buf).unwrap();
            assert!(n > 0);
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(&got[..250], &es[0][..]);
        assert_eq!(&got[250..500], &es[1][..]);
        assert_eq!(&got[500..750], &es[2][..]);

        // The window moves on; the next read refreshes and carries on
        // from sequence 3, not from the window's start.
        canned
            .responses
            .lock()
            .unwrap()
            .insert(format!("{base}a.m3u8"), (None, window(2, 5).into_bytes()));
        let n = stream.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], &es[3][..n]);
        {
            let fetched = canned.fetched.lock().unwrap();
            assert_eq!(
                fetched.iter().filter(|u| u.ends_with("seg2.ts")).count(),
                1
            );
            assert!(!fetched.iter().any(|u| u.ends_with("seg4.ts")));
        }

        // A plain stream passes through untouched.
        canned.responses.lock().unwrap().insert(
            "http://s/plain.mp3".to_string(),
            (Some("audio/mpeg".to_string()), vec![0xFF; 10]),
        );
        let mut plain = aware.open("http://s/plain.mp3").unwrap();
        assert_eq!(plain.header("content-type").as_deref(), Some("audio/mpeg"));
        assert_eq!(plain.read(&mut buf).unwrap(), 10);
    }
}

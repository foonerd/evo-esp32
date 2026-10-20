// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! The audio player's shelves, from evo-device-audio's slot vocabulary.
//!
//! | Rack | Shelf | Shape | Interaction | What stocks it |
//! | --- | --- | --- | --- | --- |
//! | `source` | `files` | 1 | respondent | a tree of files: a card, a directory |
//! | `source` | `radio` | 1 | respondent | streams by URL |
//! | `playback` | `decode` | 1 | warden | turns a track into frames and drives them out |
//! | `delivery` | `output` | 1 | respondent | the sink's level and mute |
//! | `metadata` | `tags` | 1 | respondent | what a track is called |
//! | `artwork` | `embedded` | 1 | respondent | where a track's picture is |
//!
//! Presentation is not a shelf. The panel is a consumer: it reads the
//! steward's projections and draws, as the network does; a plugin cannot,
//! because a plugin holds a [`Context`](crate::context::Context) and not
//! the steward. The distribution's loop is where the pages live.
//!
//! Every type here serialises to the JSON the fabric carries and back;
//! `to_value` and `from_value` are the two conversions, so a plugin does
//! not hand-build objects and a consumer does not hand-read them.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::catalogue::ShelfRef;
use crate::Value;

/// A value in the contract's shape, or why the given one was not.
pub fn from_value<T: DeserializeOwned>(v: &Value) -> Result<T, String> {
    serde_json::from_value(v.clone()).map_err(|e| alloc::format!("{e}"))
}

/// A contract type as the value the fabric carries.
pub fn to_value<T: Serialize>(t: &T) -> Value {
    serde_json::to_value(t).unwrap_or(Value::Null)
}

/// The shelves, one spelling for catalogues, plugins and consumers.
pub mod shelf {
    use super::ShelfRef;

    /// Rack `source`.
    pub const SOURCE: &str = "source";
    /// Rack `playback`.
    pub const PLAYBACK: &str = "playback";
    /// Rack `delivery`.
    pub const DELIVERY: &str = "delivery";
    /// Rack `metadata`.
    pub const METADATA: &str = "metadata";
    /// Rack `artwork`.
    pub const ARTWORK: &str = "artwork";

    /// `source/files`.
    pub const FILES: &str = "files";
    /// `source/radio`.
    pub const RADIO: &str = "radio";
    /// `playback/decode`.
    pub const DECODE: &str = "decode";
    /// `delivery/output`.
    pub const OUTPUT: &str = "output";
    /// `metadata/tags`.
    pub const TAGS: &str = "tags";
    /// `artwork/embedded`.
    pub const EMBEDDED: &str = "embedded";

    /// The shape every shelf in this contract is at.
    pub const SHAPE: u32 = 1;

    /// `source/files` as a reference.
    pub fn source_files() -> ShelfRef {
        ShelfRef::new(SOURCE, FILES)
    }
    /// `source/radio` as a reference.
    pub fn source_radio() -> ShelfRef {
        ShelfRef::new(SOURCE, RADIO)
    }
    /// `playback/decode` as a reference.
    pub fn playback_decode() -> ShelfRef {
        ShelfRef::new(PLAYBACK, DECODE)
    }
    /// `delivery/output` as a reference.
    pub fn delivery_output() -> ShelfRef {
        ShelfRef::new(DELIVERY, OUTPUT)
    }
    /// `metadata/tags` as a reference.
    pub fn metadata_tags() -> ShelfRef {
        ShelfRef::new(METADATA, TAGS)
    }
    /// `artwork/embedded` as a reference.
    pub fn artwork_embedded() -> ShelfRef {
        ShelfRef::new(ARTWORK, EMBEDDED)
    }
}

/// What a track is, wherever it came from. The state of a `track` subject
/// and the entry a source lists.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Track {
    /// How to ask for it again: `file:///music/a.flac`,
    /// `http://host/stream`.
    pub uri: String,
    /// Title, or the file name when nothing better is known.
    pub title: String,
    /// Artist, where known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    /// Album, where known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    /// Length in milliseconds, where known. Streams have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Frames per second, where the source read it from the file's
    /// header; a distribution with a rate ceiling passes such a track
    /// over without opening it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    /// A picture for it, where the source knows one: a station's logo
    /// (`file:///sdcard/Radio/logos/a.png`, `https://host/a.png`). Not
    /// set, a distribution looks where it will (the folder's cover).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub art: Option<String>,
}

/// The subject kind a source announces a track under.
pub const TRACK_KIND: &str = "track";

/// `source/*`: where tracks come from.
pub mod source {
    use super::*;

    /// Verb: list what is under a path. Args [`ListArgs`], reply [`Listing`].
    pub const LIST: &str = "list";

    /// Arguments to [`LIST`].
    #[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
    pub struct ListArgs {
        /// Where to look, in the source's own terms; empty is the root.
        #[serde(default)]
        pub path: String,
    }

    /// One thing a listing holds.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case", tag = "entry")]
    pub enum Entry {
        /// Somewhere to list further.
        Folder {
            /// Its path, to pass back as [`ListArgs::path`].
            path: String,
            /// What to call it.
            name: String,
        },
        /// Something to play.
        Track(Track),
    }

    /// Reply to [`LIST`].
    #[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
    pub struct Listing {
        /// The path that was listed.
        pub path: String,
        /// Its entries, folders first, each group in the source's order.
        pub entries: Vec<Entry>,
    }
}

/// `playback/decode`: the warden that plays.
pub mod playback {
    use super::*;

    /// The subject kind the warden keeps its state under.
    pub const KIND: &str = "playback";

    /// Where the warden is.
    #[derive(
        Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize,
    )]
    #[serde(rename_all = "lowercase")]
    pub enum Status {
        /// Nothing in custody.
        #[default]
        Stopped,
        /// The track is being reached (connected, buffered, probed);
        /// nothing is going out yet. A network stream passes through
        /// here; a file opens at once.
        Opening,
        /// Frames are going out.
        Playing,
        /// Held at a position.
        Paused,
    }

    /// The stream's format once the decoder knows it.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Format {
        /// Codec name as the decoder reports it: `flac`, `mp3`, `pcm`.
        pub codec: String,
        /// Frames per second.
        pub sample_rate: u32,
        /// Bits per sample.
        pub bits: u8,
        /// Channels.
        pub channels: u8,
    }

    /// The warden's state: what a consumer draws.
    #[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
    pub struct State {
        /// Where the warden is.
        pub status: Status,
        /// The track in custody, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub track: Option<Track>,
        /// Position in the track, milliseconds.
        pub position_ms: u64,
        /// The stream's format, once known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub format: Option<Format>,
    }

    /// An [`Assignment`](crate::warden::Assignment)'s instruction.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Play {
        /// What to play.
        pub track: Track,
        /// Where to start, milliseconds.
        #[serde(default)]
        pub start_ms: u64,
    }

    /// A [`CourseCorrection`](crate::warden::CourseCorrection)'s instruction.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case", tag = "do")]
    pub enum Correction {
        /// Hold at the current position.
        Pause,
        /// Carry on from it.
        Resume,
        /// Move to a position.
        Seek {
            /// Milliseconds from the start.
            position_ms: u64,
        },
    }

    /// Where decoded frames go.
    ///
    /// Frames do not travel through the fabric's values, as a picture does
    /// not: the distribution owns the one sink its board has (an I2S
    /// peripheral, a file, nothing) and hands it to the decoder it admits.
    /// The decoder writes; what the sink does with level and mute is the
    /// output plugin's business through its own route to the hardware, so
    /// the two plugins still never meet.
    ///
    /// Samples are signed 16-bit, interleaved by channel, `channels` per
    /// frame as the last [`start`](Self::start) said. A sink never blocks:
    /// it takes what it has room for and says how much, and the decoder
    /// keeps the rest for its next tick. That is how a tick stays bounded
    /// and how the sink's clock, not the decoder's, paces playback.
    ///
    /// A sink may hold frames it has taken for a while before they are
    /// heard (a board's sink runs the decoder ahead of its DMA so that a
    /// busy moment elsewhere does not starve it). It says how many through
    /// [`held`](Self::held), so the position the decoder publishes is the
    /// one being heard; and it is told of a pause so the held frames wait
    /// rather than play on.
    pub trait FrameSink {
        /// The stream's format from here on. Before the first write of a
        /// stream and again whenever it changes.
        fn start(&mut self, format: &Format);
        /// Offer frames; the return is how many frames were taken, from
        /// the front. Zero means full: ask again later.
        fn write(&mut self, interleaved: &[i16]) -> usize;
        /// Drop what has been taken but has not left yet. After a seek,
        /// so the ear does not hear the old place first.
        fn flush(&mut self);
        /// Nothing more follows for this stream. The sink lets its tail
        /// out and may stop its clock.
        fn stop(&mut self);
        /// The listener paused: what is held waits, silence goes out.
        fn pause(&mut self) {}
        /// The listener resumed: what is held plays on from where it was.
        fn resume(&mut self) {}
        /// Frames taken but not yet heard. A sink that plays what it takes
        /// at once says zero.
        fn held(&self) -> usize {
            0
        }
    }

    /// Bytes from beyond the card: what an `http://` or `https://` URI
    /// reads as once opened. The distribution opens it through its
    /// platform (the board's TLS, a host's sockets) and hands it to the
    /// decoder it admits, as it hands the sink; the decoder reads and
    /// never knows the transport.
    pub trait ByteStream: Send {
        /// Read into `buf`; the bytes read, zero at the end of the
        /// stream. Blocks until there is something or the transport
        /// gives up, which it says.
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, String>;
        /// A response header by its lower-case name (`content-type`,
        /// `icy-metaint`, `icy-name`, `icy-br`), if the server sent it.
        fn header(&self, name: &str) -> Option<String>;
    }

    /// Opens the URIs a distribution's platform can reach. Shared with
    /// the thread that fetches a stream, which opens the URI again when
    /// the connection drops.
    pub trait StreamOpener: Send + Sync {
        /// Open `uri` for reading, asking the server for the stream's
        /// metadata where the protocol has it (`Icy-MetaData: 1`).
        fn open(&self, uri: &str) -> Result<Box<dyn ByteStream>, String>;
    }
}

/// `delivery/output`: the sink.
pub mod delivery {
    use super::*;

    /// The subject kind the sink keeps its state under.
    pub const KIND: &str = "output";

    /// Verb: set the level. Args [`Volume`].
    pub const VOLUME: &str = "volume";
    /// Verb: mute or unmute. Args [`Mute`].
    pub const MUTE: &str = "mute";

    /// Arguments to [`VOLUME`].
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Volume {
        /// 0 to 100.
        pub level: u8,
    }

    /// Arguments to [`MUTE`].
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Mute {
        /// True to silence.
        pub on: bool,
    }

    /// The sink's state.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct State {
        /// The sink, in the board's words: `es8311`, `i2s-dac`, `null`.
        pub sink: String,
        /// 0 to 100.
        pub volume: u8,
        /// True while silenced.
        pub muted: bool,
    }
}

/// `metadata/tags`: what a track is called.
pub mod metadata {
    use super::*;

    /// Verb: the tags of a track. Args [`Lookup`], reply [`Track`].
    pub const LOOKUP: &str = "lookup";

    /// Arguments to [`LOOKUP`].
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Lookup {
        /// The track's URI.
        pub uri: String,
    }
}

/// `artwork/embedded`: where a track's picture is.
pub mod artwork {
    use super::*;

    /// Verb: the picture for a track. Args [`Fetch`], reply [`Art`].
    pub const FETCH: &str = "fetch";

    /// Arguments to [`FETCH`].
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Fetch {
        /// The track's URI.
        pub uri: String,
    }

    /// Where a consumer fetches the picture from. A reference, never the
    /// bytes: a picture does not travel through the fabric's values.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Art {
        /// A URL the consumer's HTTP client can GET, or an `embedded:` URI
        /// the distribution's art loader understands.
        pub source: String,
        /// The MIME type, where known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub mime: Option<String>,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_track_round_trips_and_omits_the_unknown() {
        let t = Track {
            uri: "file:///a.flac".into(),
            title: "a".into(),
            artist: None,
            album: None,
            duration_ms: Some(1000),
            sample_rate: None,
            art: None,
        };
        let v = to_value(&t);
        // Value maps sort their keys; the unknown fields are absent.
        assert_eq!(
            serde_json::to_string(&v).unwrap(),
            r#"{"duration_ms":1000,"title":"a","uri":"file:///a.flac"}"#
        );
        assert_eq!(from_value::<Track>(&v).unwrap(), t);
    }

    #[test]
    fn corrections_are_tagged_with_do() {
        let v = to_value(&playback::Correction::Seek { position_ms: 5 });
        assert_eq!(
            serde_json::to_string(&v).unwrap(),
            r#"{"do":"seek","position_ms":5}"#
        );
        assert_eq!(
            from_value::<playback::Correction>(&v).unwrap(),
            playback::Correction::Seek { position_ms: 5 }
        );
    }

    #[test]
    fn a_wrong_shape_says_why() {
        let err = from_value::<delivery::Volume>(
            &serde_json::json!({"level": "loud"}),
        );
        assert!(err.unwrap_err().contains("expected u8"));
    }

    #[test]
    fn shelf_references_spell_the_catalogue() {
        assert_eq!(shelf::playback_decode().to_string(), "playback/decode");
        assert_eq!(shelf::source_files().to_string(), "source/files");
    }
}

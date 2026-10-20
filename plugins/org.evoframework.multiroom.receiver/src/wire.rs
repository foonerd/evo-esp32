// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! The Evo audio plane on the wire, as a receiver sees it.
//!
//! Every message is a 4-byte big-endian length and a body. A body that
//! opens with `0xEB` is a `bin1` audio frame: a 30-byte big-endian
//! header, then the group id, the codec name and the PCM. Any other
//! body is a JSON document with a `type` field: `hello`, `heartbeat`,
//! `sync_probe`, `sync_response`, `audio_frame` (an older source-host's
//! frame, PCM in base64), `goodbye`, and others a receiver has no part
//! in and lets pass. The shapes are the audio plane's own
//! (`evo-multiroom`'s `AudioPlaneMessage` and `BinaryAudioFrame`), kept
//! here by hand so the receiver carries no dependency on the framework
//! crates and no JSON parse touches an audio frame from a source-host
//! of this version.

use std::io::{Read, Write};

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};

/// First byte of a `bin1` body.
pub const BIN1_MAGIC: u8 = 0xEB;
/// Second byte: an audio frame.
const BIN1_KIND_AUDIO_FRAME: u8 = 0x01;
/// The fixed part of a `bin1` audio frame.
const BIN1_HEADER_LEN: usize = 30;
/// The frame format this receiver offers in its `hello`.
pub const FRAME_FORMAT_BIN1: &str = "bin1";
/// The most a body may be: an audio frame is a few kilobytes, a chain
/// message a few tens; anything larger is a peer that is not an Evo
/// source-host.
pub const MAX_BODY: usize = 256 * 1024;

/// A JSON message on the audio plane. Fields a receiver does not read
/// are left out; serde skips them. A `type` not listed is `Other`. The
/// field names are the plane's own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum Message {
    /// The handshake, first from each side.
    Hello {
        device_id: String,
        framework_version: String,
        #[serde(default)]
        claimed_source_host_groups: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        formats: Vec<String>,
    },
    /// Liveness, either way; carries the sender's groups.
    Heartbeat {
        device_id: String,
        monotonic_ms: u64,
        #[serde(default)]
        groups: Vec<String>,
    },
    /// Ours to send: `t1_ms` is our clock at send.
    SyncProbe { group_id: String, t1_ms: u64 },
    /// The source-host's answer: our `t1_ms` back, its clock at receipt.
    SyncResponse {
        group_id: String,
        t1_ms: u64,
        t2_ms: u64,
    },
    /// An older source-host's audio frame.
    AudioFrame {
        group_id: String,
        sequence: u64,
        presentation_time_ms: u64,
        codec: String,
        rate_hz: u32,
        channels: u16,
        payload_b64: String,
    },
    /// The peer is leaving.
    Goodbye { device_id: String },
    /// Anything else on the plane (witness chain, presence relays).
    #[serde(other)]
    Other,
}

/// An audio frame, whichever way it came.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// The group it is for.
    pub group_id: String,
    /// Its place in the group's sequence.
    pub sequence: u64,
    /// The source-host's clock at which this frame plays.
    pub presentation_time_ms: u64,
    /// `pcm_s16_le`, so far.
    pub codec: String,
    /// Frames per second.
    pub rate_hz: u32,
    /// Channels per frame.
    pub channels: u16,
    /// The codec payload: for `pcm_s16_le`, little-endian 16-bit
    /// interleaved samples.
    pub payload: Vec<u8>,
}

impl Frame {
    /// A frame from a `bin1` body.
    pub fn from_bin1(body: &[u8]) -> Result<Self, String> {
        if body.len() < BIN1_HEADER_LEN {
            return Err("bin1: body short".into());
        }
        if body[0] != BIN1_MAGIC || body[1] != BIN1_KIND_AUDIO_FRAME {
            return Err(format!(
                "bin1: unknown body {:#04x} {:#04x}",
                body[0], body[1]
            ));
        }
        let (gl, cl) = (usize::from(body[2]), usize::from(body[3]));
        let u64_at = |at: usize| {
            u64::from_be_bytes(body[at..at + 8].try_into().expect("8"))
        };
        let sequence = u64_at(4);
        let presentation_time_ms = u64_at(12);
        let rate_hz = u32::from_be_bytes(body[20..24].try_into().expect("4"));
        let channels = u16::from_be_bytes(body[24..26].try_into().expect("2"));
        let n =
            u32::from_be_bytes(body[26..30].try_into().expect("4")) as usize;
        let rest = &body[BIN1_HEADER_LEN..];
        if rest.len() != gl + cl + n {
            return Err(format!(
                "bin1: {} bytes after the header, the header says {}",
                rest.len(),
                gl + cl + n
            ));
        }
        let text = |b: &[u8], what: &str| {
            std::str::from_utf8(b)
                .map(str::to_owned)
                .map_err(|_| format!("bin1: {what} is not UTF-8"))
        };
        Ok(Self {
            group_id: text(&rest[..gl], "group id")?,
            codec: text(&rest[gl..gl + cl], "codec")?,
            sequence,
            presentation_time_ms,
            rate_hz,
            channels,
            payload: rest[gl + cl..].to_vec(),
        })
    }

    /// A frame from an older source-host's JSON envelope.
    #[allow(clippy::too_many_arguments)]
    pub fn from_json(
        group_id: String,
        sequence: u64,
        presentation_time_ms: u64,
        codec: String,
        rate_hz: u32,
        channels: u16,
        payload_b64: &str,
    ) -> Result<Self, String> {
        let payload = STANDARD
            .decode(payload_b64)
            .map_err(|e| format!("audio_frame: payload is not base64: {e}"))?;
        Ok(Self {
            group_id,
            sequence,
            presentation_time_ms,
            codec,
            rate_hz,
            channels,
            payload,
        })
    }

    /// The frame as `bin1` bytes: what a source-host sends. Here for
    /// the tests' source-host and for symmetry.
    pub fn to_bin1(&self) -> Vec<u8> {
        let (g, c) = (self.group_id.as_bytes(), self.codec.as_bytes());
        let mut out = Vec::with_capacity(
            BIN1_HEADER_LEN + g.len() + c.len() + self.payload.len(),
        );
        out.push(BIN1_MAGIC);
        out.push(BIN1_KIND_AUDIO_FRAME);
        out.push(g.len() as u8);
        out.push(c.len() as u8);
        out.extend_from_slice(&self.sequence.to_be_bytes());
        out.extend_from_slice(&self.presentation_time_ms.to_be_bytes());
        out.extend_from_slice(&self.rate_hz.to_be_bytes());
        out.extend_from_slice(&self.channels.to_be_bytes());
        out.extend_from_slice(&(self.payload.len() as u32).to_be_bytes());
        out.extend_from_slice(g);
        out.extend_from_slice(c);
        out.extend_from_slice(&self.payload);
        out
    }
}

/// What one read from the wire came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inbound {
    /// A JSON message.
    Message(Message),
    /// A `bin1` audio frame.
    Frame(Frame),
}

/// Why a read gave nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    /// Nothing arrived within the socket's read timeout; the connection
    /// is whole and the next read continues it.
    Idle,
    /// The connection is over: closed, broken, or carrying something
    /// that is not the audio plane.
    Over(String),
}

fn is_timeout(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

/// Fill `buf` from `reader`, waiting through read timeouts: once a
/// body has begun, the rest of it is owed.
fn read_full<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<(), ReadError> {
    let mut got = 0;
    while got < buf.len() {
        match reader.read(&mut buf[got..]) {
            Ok(0) => return Err(ReadError::Over("closed".into())),
            Ok(n) => got += n,
            Err(e) if is_timeout(&e) => continue,
            Err(e) => return Err(ReadError::Over(format!("read: {e}"))),
        }
    }
    Ok(())
}

/// One length-prefixed body: a frame, or a message. [`ReadError::Idle`]
/// when the socket's read timeout passed before a body began.
pub fn read_inbound<R: Read>(reader: &mut R) -> Result<Inbound, ReadError> {
    let mut len = [0u8; 4];
    match reader.read(&mut len) {
        Ok(0) => return Err(ReadError::Over("closed".into())),
        Ok(n) => read_full(reader, &mut len[n..])?,
        Err(e) if is_timeout(&e) => return Err(ReadError::Idle),
        Err(e) => return Err(ReadError::Over(format!("read: {e}"))),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len == 0 || len > MAX_BODY {
        return Err(ReadError::Over(format!("body of {len} bytes")));
    }
    let mut body = vec![0u8; len];
    read_full(reader, &mut body)?;
    if body[0] == BIN1_MAGIC {
        return Frame::from_bin1(&body)
            .map(Inbound::Frame)
            .map_err(ReadError::Over);
    }
    let msg: Message = serde_json::from_slice(&body)
        .map_err(|e| ReadError::Over(format!("message: {e}")))?;
    Ok(Inbound::Message(msg))
}

/// `body` to the wire behind its length.
pub fn write_body<W: Write>(writer: &mut W, body: &[u8]) -> Result<(), String> {
    let len = (body.len() as u32).to_be_bytes();
    writer
        .write_all(&len)
        .and_then(|()| writer.write_all(body))
        .and_then(|()| writer.flush())
        .map_err(|e| format!("write: {e}"))
}

/// `msg` to the wire as JSON.
pub fn write_message<W: Write>(
    writer: &mut W,
    msg: &Message,
) -> Result<(), String> {
    let body = serde_json::to_vec(msg).map_err(|e| format!("encode: {e}"))?;
    write_body(writer, &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bin1_round_trips_and_refuses_a_lying_header() {
        let f = Frame {
            group_id: "g".into(),
            sequence: 3,
            presentation_time_ms: 60,
            codec: "pcm_s16_le".into(),
            rate_hz: 48_000,
            channels: 2,
            payload: vec![1, 0, 2, 0, 3, 0, 4, 0],
        };
        let b = f.to_bin1();
        assert_eq!(b[0], BIN1_MAGIC);
        assert_eq!(Frame::from_bin1(&b).unwrap(), f);
        let mut lying = b.clone();
        lying[29] += 1;
        assert!(Frame::from_bin1(&lying).is_err());
        assert!(Frame::from_bin1(&b[..10]).is_err());
    }

    #[test]
    fn the_plane_messages_parse_and_the_rest_pass() {
        let hello = br#"{"type":"hello","device_id":"d","framework_version":"0.1.13","claimed_source_host_groups":["g"],"formats":["bin1"]}"#;
        let m: Message = serde_json::from_slice(hello).unwrap();
        assert!(
            matches!(m, Message::Hello { ref device_id, ref formats, .. } if device_id == "d" && formats == &["bin1"])
        );
        let old_hello = br#"{"type":"hello","device_id":"d","framework_version":"0.1.0","claimed_source_host_groups":[]}"#;
        let m: Message = serde_json::from_slice(old_hello).unwrap();
        assert!(
            matches!(m, Message::Hello { ref formats, .. } if formats.is_empty())
        );
        let witness = br#"{"type":"domain_witness_announce","witness":{"anything":[1,2,3]}}"#;
        let m: Message = serde_json::from_slice(witness).unwrap();
        assert_eq!(m, Message::Other);
        let probe = Message::SyncProbe {
            group_id: "g".into(),
            t1_ms: 7,
        };
        let text = serde_json::to_string(&probe).unwrap();
        assert_eq!(text, r#"{"type":"sync_probe","group_id":"g","t1_ms":7}"#);
    }

    #[test]
    fn a_frame_and_a_message_read_back_in_order() {
        let f = Frame {
            group_id: "g".into(),
            sequence: 1,
            presentation_time_ms: 20,
            codec: "pcm_s16_le".into(),
            rate_hz: 48_000,
            channels: 2,
            payload: vec![0; 8],
        };
        let mut wire = Vec::new();
        write_body(&mut wire, &f.to_bin1()).unwrap();
        write_message(
            &mut wire,
            &Message::Goodbye {
                device_id: "d".into(),
            },
        )
        .unwrap();
        let mut r = &wire[..];
        assert_eq!(read_inbound(&mut r).unwrap(), Inbound::Frame(f));
        assert_eq!(
            read_inbound(&mut r).unwrap(),
            Inbound::Message(Message::Goodbye {
                device_id: "d".into()
            })
        );
        assert_eq!(
            read_inbound(&mut r),
            Err(ReadError::Over("closed".into())),
            "nothing left"
        );
    }

    #[test]
    fn an_old_source_hosts_json_frame_decodes() {
        let f = Frame::from_json(
            "g".into(),
            9,
            180,
            "pcm_s16_le".into(),
            48_000,
            2,
            &STANDARD.encode([1u8, 0, 255, 255]),
        )
        .unwrap();
        assert_eq!(f.payload, [1, 0, 255, 255]);
        assert!(Frame::from_json("g".into(), 0, 0, "x".into(), 1, 1, "%%%")
            .is_err());
    }
}

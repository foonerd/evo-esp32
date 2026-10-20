// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Snapcast's binary protocol, as a client speaks it.
//!
//! Every message is a 26-byte base header, little-endian — type, id,
//! refers-to, the sender's clock when sent, the receiver's clock when
//! received, the payload's size — and a typed payload. The client
//! sends `Hello`, then `Time` on a cadence; the server answers with
//! `ServerSettings`, `CodecHeader`, `WireChunk`s stamped in its clock,
//! `Time` replies, and `StreamTags`. The shapes are Snapcast's
//! (`doc/binary_protocol.md` in the Snapcast repository).

use std::io::{Read, Write};

use serde::{Deserialize, Serialize};

/// The base header's length.
pub const HEADER_LEN: usize = 26;
/// The most a payload may be: a chunk of PCM at 48 kHz is under 20 KB.
pub const MAX_PAYLOAD: usize = 1024 * 1024;

/// Message type: the codec and its header.
pub const TYPE_CODEC_HEADER: u16 = 1;
/// Message type: a piece of the stream.
pub const TYPE_WIRE_CHUNK: u16 = 2;
/// Message type: the server's settings for this client.
pub const TYPE_SERVER_SETTINGS: u16 = 3;
/// Message type: a clock probe, or its answer.
pub const TYPE_TIME: u16 = 4;
/// Message type: the client's hello.
pub const TYPE_HELLO: u16 = 5;
/// Message type: the stream's tags.
pub const TYPE_STREAM_TAGS: u16 = 6;
/// Message type: the client's volume, to the server.
pub const TYPE_CLIENT_INFO: u16 = 7;
/// Message type: the server's refusal.
pub const TYPE_ERROR: u16 = 8;

/// A clock reading as the wire carries it: seconds and microseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tv {
    /// Whole seconds.
    pub sec: i32,
    /// Microseconds within the second.
    pub usec: i32,
}

impl Tv {
    /// From microseconds.
    pub fn from_us(us: i64) -> Self {
        Tv {
            sec: us.div_euclid(1_000_000) as i32,
            usec: us.rem_euclid(1_000_000) as i32,
        }
    }

    /// As microseconds.
    pub fn as_us(self) -> i64 {
        i64::from(self.sec) * 1_000_000 + i64::from(self.usec)
    }
}

/// The base header. The field names are the protocol's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(missing_docs)]
pub struct Header {
    pub kind: u16,
    pub id: u16,
    pub refers_to: u16,
    pub sent: Tv,
    pub received: Tv,
    pub size: u32,
}

impl Header {
    fn encode(&self) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        b[0..2].copy_from_slice(&self.kind.to_le_bytes());
        b[2..4].copy_from_slice(&self.id.to_le_bytes());
        b[4..6].copy_from_slice(&self.refers_to.to_le_bytes());
        b[6..10].copy_from_slice(&self.sent.sec.to_le_bytes());
        b[10..14].copy_from_slice(&self.sent.usec.to_le_bytes());
        b[14..18].copy_from_slice(&self.received.sec.to_le_bytes());
        b[18..22].copy_from_slice(&self.received.usec.to_le_bytes());
        b[22..26].copy_from_slice(&self.size.to_le_bytes());
        b
    }

    fn decode(b: &[u8; HEADER_LEN]) -> Self {
        let i32_at = |at: usize| {
            i32::from_le_bytes(b[at..at + 4].try_into().expect("4"))
        };
        Header {
            kind: u16::from_le_bytes([b[0], b[1]]),
            id: u16::from_le_bytes([b[2], b[3]]),
            refers_to: u16::from_le_bytes([b[4], b[5]]),
            sent: Tv {
                sec: i32_at(6),
                usec: i32_at(10),
            },
            received: Tv {
                sec: i32_at(14),
                usec: i32_at(18),
            },
            size: u32::from_le_bytes(b[22..26].try_into().expect("4")),
        }
    }
}

/// What the server sends. The payloads' field names are the protocol's.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum Inbound {
    /// The client's volume, mute, latency and the buffer.
    ServerSettings(ServerSettings),
    /// The codec and what its decoder starts from.
    CodecHeader { codec: String, payload: Vec<u8> },
    /// A piece of the stream, stamped in the server's clock.
    WireChunk { timestamp: Tv, payload: Vec<u8> },
    /// The answer to a `Time`: the server's clock when it received the
    /// probe less the client's when it sent it (`latency`), and in the
    /// header the server's clock when it answered.
    Time { latency: Tv, header: Header },
    /// The stream's tags (its name).
    StreamTags(StreamTags),
    /// The server refused (authentication, usually).
    Error {
        code: u32,
        error: String,
        detail: String,
    },
    /// A type this client has no part in.
    Other(u16),
}

/// `ServerSettings`' JSON; the field names are the protocol's.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(missing_docs)]
pub struct ServerSettings {
    #[serde(default)]
    pub buffer_ms: i64,
    #[serde(default)]
    pub latency: i64,
    #[serde(default)]
    pub muted: bool,
    #[serde(default = "full")]
    pub volume: u8,
}

fn full() -> u8 {
    100
}

/// `StreamTags`' JSON: whatever the server has; the stream's name is
/// `STREAM`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct StreamTags {
    /// The stream's name.
    #[serde(rename = "STREAM", default)]
    pub stream: String,
}

/// `Hello`'s JSON; the field names are the protocol's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "PascalCase")]
#[allow(missing_docs)]
pub struct Hello {
    pub arch: String,
    pub client_name: String,
    pub host_name: String,
    #[serde(rename = "ID")]
    pub id: String,
    pub instance: u32,
    #[serde(rename = "MAC")]
    pub mac: String,
    #[serde(rename = "OS")]
    pub os: String,
    pub snap_stream_protocol_version: u32,
    pub version: String,
}

/// Why a read gave nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    /// Nothing arrived within the socket's read timeout; the connection
    /// is whole.
    Idle,
    /// The connection is over.
    Over(String),
}

fn is_timeout(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

/// Fill `buf`, waiting through read timeouts: once a message has begun,
/// the rest of it is owed.
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

fn u32_at(b: &[u8], at: usize) -> Result<u32, ReadError> {
    b.get(at..at + 4)
        .map(|s| u32::from_le_bytes(s.try_into().expect("4")))
        .ok_or_else(|| ReadError::Over("payload short".into()))
}

fn i32_at(b: &[u8], at: usize) -> Result<i32, ReadError> {
    u32_at(b, at).map(|v| v as i32)
}

/// A length-prefixed string or buffer at `at`: the bytes and where the
/// next field begins.
fn sized_at(b: &[u8], at: usize) -> Result<(&[u8], usize), ReadError> {
    let n = u32_at(b, at)? as usize;
    let start = at + 4;
    let end = start
        .checked_add(n)
        .filter(|&e| e <= b.len())
        .ok_or_else(|| ReadError::Over("payload short".into()))?;
    Ok((&b[start..end], end))
}

fn text(b: &[u8], what: &str) -> Result<String, ReadError> {
    std::str::from_utf8(b)
        .map(str::to_owned)
        .map_err(|_| ReadError::Over(format!("{what} is not UTF-8")))
}

/// One message from the wire. `now_us` is the client's clock, read the
/// moment the header is in and written into it as the time received:
/// a clock probe's answer is timed by that, so it is not read before
/// the wait for it.
pub fn read_inbound<R: Read>(
    reader: &mut R,
    now_us: impl Fn() -> i64,
) -> Result<Inbound, ReadError> {
    let mut head = [0u8; HEADER_LEN];
    match reader.read(&mut head) {
        Ok(0) => return Err(ReadError::Over("closed".into())),
        Ok(n) => read_full(reader, &mut head[n..])?,
        Err(e) if is_timeout(&e) => return Err(ReadError::Idle),
        Err(e) => return Err(ReadError::Over(format!("read: {e}"))),
    }
    let mut header = Header::decode(&head);
    header.received = Tv::from_us(now_us());
    let size = header.size as usize;
    if size > MAX_PAYLOAD {
        return Err(ReadError::Over(format!("payload of {size} bytes")));
    }
    let mut body = vec![0u8; size];
    read_full(reader, &mut body)?;
    let b = &body[..];
    Ok(match header.kind {
        TYPE_SERVER_SETTINGS => {
            let (json, _) = sized_at(b, 0)?;
            let settings: ServerSettings = serde_json::from_slice(json)
                .map_err(|e| {
                    ReadError::Over(format!("server settings: {e}"))
                })?;
            Inbound::ServerSettings(settings)
        }
        TYPE_CODEC_HEADER => {
            let (codec, next) = sized_at(b, 0)?;
            let (payload, _) = sized_at(b, next)?;
            Inbound::CodecHeader {
                codec: text(codec, "codec")?,
                payload: payload.to_vec(),
            }
        }
        TYPE_WIRE_CHUNK => {
            let timestamp = Tv {
                sec: i32_at(b, 0)?,
                usec: i32_at(b, 4)?,
            };
            let (payload, _) = sized_at(b, 8)?;
            Inbound::WireChunk {
                timestamp,
                payload: payload.to_vec(),
            }
        }
        TYPE_TIME => Inbound::Time {
            latency: Tv {
                sec: i32_at(b, 0)?,
                usec: i32_at(b, 4)?,
            },
            header,
        },
        TYPE_STREAM_TAGS => {
            let (json, _) = sized_at(b, 0)?;
            let tags: StreamTags =
                serde_json::from_slice(json).unwrap_or_default();
            Inbound::StreamTags(tags)
        }
        TYPE_ERROR => {
            let code = u32_at(b, 0)?;
            let (error, next) = sized_at(b, 4)?;
            let (detail, _) = sized_at(b, next)?;
            Inbound::Error {
                code,
                error: text(error, "error")?,
                detail: text(detail, "error detail")?,
            }
        }
        other => Inbound::Other(other),
    })
}

/// `payload` to the wire as a message of `kind`, stamped with the
/// client's clock `now_us`; `refers_to` for an answer, else 0.
pub fn write_message<W: Write>(
    writer: &mut W,
    kind: u16,
    id: u16,
    refers_to: u16,
    now_us: i64,
    payload: &[u8],
) -> Result<(), String> {
    let header = Header {
        kind,
        id,
        refers_to,
        sent: Tv::from_us(now_us),
        received: Tv::default(),
        size: payload.len() as u32,
    };
    writer
        .write_all(&header.encode())
        .and_then(|()| writer.write_all(payload))
        .and_then(|()| writer.flush())
        .map_err(|e| format!("write: {e}"))
}

/// A JSON payload as the wire wants it: its length, then the text.
pub fn sized_json<T: Serialize>(value: &T) -> Result<Vec<u8>, String> {
    let json = serde_json::to_vec(value).map_err(|e| format!("encode: {e}"))?;
    let mut out = Vec::with_capacity(4 + json.len());
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    out.extend_from_slice(&json);
    Ok(out)
}

/// A `Time` probe's payload: a zero latency the server fills in.
pub fn time_payload() -> [u8; 8] {
    [0u8; 8]
}

/// The parts of a RIFF WAVE header a PCM stream's decoder needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaveFormat {
    /// Frames per second.
    pub rate: u32,
    /// Channels per frame.
    pub channels: u16,
    /// Bits per sample.
    pub bits: u16,
}

/// Read the `fmt ` chunk of the WAV header a PCM codec sends.
pub fn wave_format(header: &[u8]) -> Result<WaveFormat, String> {
    if header.len() < 36
        || &header[0..4] != b"RIFF"
        || &header[8..12] != b"WAVE"
    {
        return Err("not a RIFF WAVE header".into());
    }
    let mut at = 12;
    while at + 8 <= header.len() {
        let id = &header[at..at + 4];
        let len =
            u32::from_le_bytes(header[at + 4..at + 8].try_into().expect("4"))
                as usize;
        if id == b"fmt " {
            let f = header
                .get(at + 8..at + 8 + 16)
                .ok_or_else(|| "fmt chunk short".to_string())?;
            return Ok(WaveFormat {
                channels: u16::from_le_bytes([f[2], f[3]]),
                rate: u32::from_le_bytes(f[4..8].try_into().expect("4")),
                bits: u16::from_le_bytes([f[14], f[15]]),
            });
        }
        at += 8 + len + (len & 1);
    }
    Err("no fmt chunk".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(kind: u16, sent_us: i64, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        write_message(&mut out, kind, 1, 0, sent_us, payload).unwrap();
        out
    }

    #[test]
    fn the_header_is_26_little_endian_bytes_and_round_trips() {
        let h = Header {
            kind: TYPE_TIME,
            id: 7,
            refers_to: 3,
            sent: Tv {
                sec: 12,
                usec: 345_678,
            },
            received: Tv::default(),
            size: 8,
        };
        let b = h.encode();
        assert_eq!(b.len(), HEADER_LEN);
        assert_eq!(&b[0..2], &[4, 0]);
        assert_eq!(&b[6..10], &12i32.to_le_bytes());
        assert_eq!(Header::decode(&b), h);
        assert_eq!(
            Tv::from_us(-1_500_000),
            Tv {
                sec: -2,
                usec: 500_000
            }
        );
        assert_eq!(Tv::from_us(1_500_000).as_us(), 1_500_000);
    }

    #[test]
    fn the_servers_messages_parse() {
        let settings =
            br#"{"bufferMs":1500,"latency":0,"muted":false,"volume":73}"#;
        let mut p = (settings.len() as u32).to_le_bytes().to_vec();
        p.extend_from_slice(settings);
        let wire = message(TYPE_SERVER_SETTINGS, 0, &p);
        let got = read_inbound(&mut &wire[..], || 5).unwrap();
        assert_eq!(
            got,
            Inbound::ServerSettings(ServerSettings {
                buffer_ms: 1500,
                latency: 0,
                muted: false,
                volume: 73
            })
        );

        let mut p = 4u32.to_le_bytes().to_vec();
        p.extend_from_slice(b"flac");
        p.extend_from_slice(&3u32.to_le_bytes());
        p.extend_from_slice(b"abc");
        let wire = message(TYPE_CODEC_HEADER, 0, &p);
        assert_eq!(
            read_inbound(&mut &wire[..], || 0).unwrap(),
            Inbound::CodecHeader {
                codec: "flac".into(),
                payload: b"abc".to_vec()
            }
        );

        let mut p = 9i32.to_le_bytes().to_vec();
        p.extend_from_slice(&250_000i32.to_le_bytes());
        p.extend_from_slice(&2u32.to_le_bytes());
        p.extend_from_slice(&[1, 2]);
        let wire = message(TYPE_WIRE_CHUNK, 0, &p);
        assert_eq!(
            read_inbound(&mut &wire[..], || 0).unwrap(),
            Inbound::WireChunk {
                timestamp: Tv {
                    sec: 9,
                    usec: 250_000
                },
                payload: vec![1, 2]
            }
        );

        let mut p = 0i32.to_le_bytes().to_vec();
        p.extend_from_slice(&40_000i32.to_le_bytes());
        let wire = message(TYPE_TIME, 77_000_000, &p);
        match read_inbound(&mut &wire[..], || 77_010_000).unwrap() {
            Inbound::Time { latency, header } => {
                assert_eq!(latency.as_us(), 40_000);
                assert_eq!(header.sent.as_us(), 77_000_000);
                assert_eq!(header.received.as_us(), 77_010_000);
            }
            other => panic!("{other:?}"),
        }

        let tags = br#"{"STREAM":"Radio","OTHER":1}"#;
        let mut p = (tags.len() as u32).to_le_bytes().to_vec();
        p.extend_from_slice(tags);
        let wire = message(TYPE_STREAM_TAGS, 0, &p);
        assert_eq!(
            read_inbound(&mut &wire[..], || 0).unwrap(),
            Inbound::StreamTags(StreamTags {
                stream: "Radio".into()
            })
        );

        let wire = message(42, 0, &[]);
        assert_eq!(
            read_inbound(&mut &wire[..], || 0).unwrap(),
            Inbound::Other(42)
        );
        assert_eq!(
            read_inbound(&mut &b""[..], || 0),
            Err(ReadError::Over("closed".into()))
        );
    }

    #[test]
    fn hello_is_the_json_the_server_expects() {
        let hello = Hello {
            arch: "riscv32".into(),
            client_name: "evo-esp32".into(),
            host_name: "evo-player".into(),
            id: "aa:bb".into(),
            instance: 1,
            mac: "aa:bb".into(),
            os: "ESP-IDF".into(),
            snap_stream_protocol_version: 2,
            version: "0.1.1".into(),
        };
        let p = sized_json(&hello).unwrap();
        let json = std::str::from_utf8(&p[4..]).unwrap();
        for key in [
            "\"Arch\"",
            "\"ClientName\"",
            "\"HostName\"",
            "\"ID\"",
            "\"Instance\"",
            "\"MAC\"",
            "\"OS\"",
            "\"SnapStreamProtocolVersion\":2",
            "\"Version\"",
        ] {
            assert!(json.contains(key), "{json}");
        }
    }

    #[test]
    fn a_wave_header_gives_the_format() {
        let mut h = Vec::new();
        h.extend_from_slice(b"RIFF");
        h.extend_from_slice(&36u32.to_le_bytes());
        h.extend_from_slice(b"WAVEfmt ");
        h.extend_from_slice(&16u32.to_le_bytes());
        h.extend_from_slice(&1u16.to_le_bytes());
        h.extend_from_slice(&2u16.to_le_bytes());
        h.extend_from_slice(&48_000u32.to_le_bytes());
        h.extend_from_slice(&(48_000u32 * 4).to_le_bytes());
        h.extend_from_slice(&4u16.to_le_bytes());
        h.extend_from_slice(&16u16.to_le_bytes());
        h.extend_from_slice(b"data");
        h.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            wave_format(&h).unwrap(),
            WaveFormat {
                rate: 48_000,
                channels: 2,
                bits: 16
            }
        );
        assert!(wave_format(b"nope").is_err());
    }
}

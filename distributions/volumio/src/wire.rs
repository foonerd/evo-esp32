// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Engine.IO 3 and socket.io 2 on the wire, the polling transport.
//!
//! A polling request or response carries a payload: packets one after
//! another, each as `<length>:<packet>`, the length counted in UTF-16
//! code units because the reference implementation counts JavaScript
//! string length. A packet's first character is its Engine.IO type; a
//! message (`4`) carries a socket.io packet whose first character is
//! its type, `2` for an event, followed by a JSON array of the event's
//! name and its data.

use std::io::{self, BufReader, Read};

use serde_json::Value;

/// Engine.IO: the server's opening packet, JSON after it.
pub const OPEN: char = '0';
/// Engine.IO: the connection is closed.
pub const CLOSE: char = '1';
/// Engine.IO: a ping, from the client.
pub const PING: char = '2';
/// Engine.IO: a pong, from the server.
pub const PONG: char = '3';
/// Engine.IO: a message; a socket.io packet follows.
pub const MESSAGE: char = '4';
/// Engine.IO: nothing, sent to release a waiting poll.
pub const NOOP: char = '6';

/// socket.io, after `4`: connected to the namespace.
pub const SIO_CONNECT: char = '0';
/// socket.io, after `4`: the namespace was left.
pub const SIO_DISCONNECT: char = '1';
/// socket.io, after `4`: an event.
pub const SIO_EVENT: char = '2';

/// The connect packet for the default namespace, as the server sends it
/// right after opening.
pub const CONNECTED: &str = "40";

/// `s` as JavaScript would count it.
fn js_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// Packets into one payload.
pub fn encode_payload<S: AsRef<str>>(
    packets: impl IntoIterator<Item = S>,
) -> String {
    let mut out = String::new();
    for p in packets {
        let p = p.as_ref();
        out.push_str(&js_len(p).to_string());
        out.push(':');
        out.push_str(p);
    }
    out
}

/// A payload of one control packet, such as a ping.
pub fn control(kind: char) -> String {
    format!("1:{kind}")
}

/// A payload into its packets; the error names what was wrong.
pub fn decode_payload(payload: &str) -> Result<Vec<String>, String> {
    read_payload(&mut payload.as_bytes(), usize::MAX).map_err(|e| e.to_string())
}

/// A payload read as it comes; packets longer than `keep` UTF-16 units
/// are passed over (a peer broadcasts its whole queue to everyone, and
/// we have no use for it), the rest returned in order.
///
/// Two framings are read. The text one, `<length>:<packet>`, is what a
/// client sends and what a server sends to a client that asked for text
/// (`b64=1`). The binary one, a type byte (0 for a string, 1 for bytes),
/// the length's digits one a byte, 255, then the packet, is what an
/// older server sends anyway; a string packet's length counts UTF-16
/// units in both. Byte packets carry nothing we listen for and are
/// passed over.
pub fn read_payload(r: &mut dyn Read, keep: usize) -> io::Result<Vec<String>> {
    let mut r = BufReader::new(r);
    let mut packets = Vec::new();
    let mut byte = [0u8; 1];
    // Decided by the payload's first byte: a type byte of 0 or 1 is the
    // binary framing, a digit the text one.
    let mut binary: Option<bool> = None;
    while let Some(first) = next(&mut r)? {
        let framing = *binary.get_or_insert(first == 0 || first == 1);
        let mut len: usize = 0;
        let mut digits = 0;
        let bytes_not_text = if framing {
            let bytes_not_text = match first {
                0 => false,
                1 => true,
                _ => return Err(invalid("a type that is not 0 or 1")),
            };
            loop {
                let b = next(&mut r)?
                    .ok_or_else(|| invalid("a length cut short"))?;
                match b {
                    0..=9 => {
                        len = len
                            .checked_mul(10)
                            .and_then(|l| l.checked_add(b as usize))
                            .ok_or_else(|| invalid("a length too long"))?;
                        digits += 1;
                    }
                    255 if digits > 0 => break,
                    _ => return Err(invalid("a length that is not a number")),
                }
            }
            bytes_not_text
        } else {
            let mut b = first;
            loop {
                match b {
                    b'0'..=b'9' => {
                        len = len
                            .checked_mul(10)
                            .and_then(|l| l.checked_add((b - b'0') as usize))
                            .ok_or_else(|| invalid("a length too long"))?;
                        digits += 1;
                    }
                    b':' if digits > 0 => break,
                    _ => return Err(invalid("a length that is not a number")),
                }
                b = next(&mut r)?
                    .ok_or_else(|| invalid("a length cut short"))?;
            }
            false
        };
        if bytes_not_text {
            // So many bytes, of no interest.
            let mut left = len;
            let mut sink = [0u8; 64];
            while left > 0 {
                let n = r.read(&mut sink[..left.min(64)])?;
                if n == 0 {
                    return Err(invalid("a packet cut short"));
                }
                left -= n;
            }
            continue;
        }
        let keeping = len <= keep;
        let mut bytes = Vec::with_capacity(if keeping { len } else { 0 });
        let mut units = 0;
        while units < len {
            if r.read(&mut byte)? == 0 {
                return Err(invalid("a packet cut short"));
            }
            let lead = byte[0];
            let width = match lead {
                0x00..=0x7F => 1,
                0xC0..=0xDF => 2,
                0xE0..=0xEF => 3,
                0xF0..=0xF7 => 4,
                _ => return Err(invalid("not UTF-8")),
            };
            units += if width == 4 { 2 } else { 1 };
            if keeping {
                bytes.push(lead);
            }
            for _ in 1..width {
                if r.read(&mut byte)? == 0 {
                    return Err(invalid("a packet cut short"));
                }
                if keeping {
                    bytes.push(byte[0]);
                }
            }
        }
        if keeping {
            packets.push(
                String::from_utf8(bytes).map_err(|_| invalid("not UTF-8"))?,
            );
        }
    }
    Ok(packets)
}

fn next(r: &mut impl Read) -> io::Result<Option<u8>> {
    let mut b = [0u8; 1];
    Ok(if r.read(&mut b)? == 0 {
        None
    } else {
        Some(b[0])
    })
}

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_string())
}

/// An event packet: `42["name",data]`.
pub fn event(name: &str, data: &Value) -> String {
    let arr = Value::Array(vec![Value::String(name.to_string()), data.clone()]);
    format!("{MESSAGE}{SIO_EVENT}{arr}")
}

/// What a received packet means.
#[derive(Debug, Clone, PartialEq)]
pub enum Packet {
    /// The server opened; the JSON that came with it.
    Open(Value),
    /// The connection is closed.
    Close,
    /// A ping.
    Ping,
    /// A pong.
    Pong,
    /// Nothing.
    Noop,
    /// socket.io connected to the namespace.
    Connected,
    /// socket.io left the namespace.
    Disconnected,
    /// An event by name, with its data (`Null` when it had none).
    Event(String, Value),
    /// Something else, kept as it came.
    Other(String),
}

/// Read one packet.
pub fn parse(packet: &str) -> Packet {
    let mut chars = packet.chars();
    let Some(kind) = chars.next() else {
        return Packet::Other(String::new());
    };
    let rest = chars.as_str();
    match kind {
        OPEN => Packet::Open(serde_json::from_str(rest).unwrap_or(Value::Null)),
        CLOSE => Packet::Close,
        PING => Packet::Ping,
        PONG => Packet::Pong,
        NOOP => Packet::Noop,
        MESSAGE => {
            let mut inner = rest.chars();
            match inner.next() {
                Some(SIO_CONNECT) => Packet::Connected,
                Some(SIO_DISCONNECT) => Packet::Disconnected,
                Some(SIO_EVENT) => {
                    // A namespace other than the default would precede the
                    // array as `/name,`; Volumio's peers use the default.
                    let body = inner.as_str();
                    let body = body.find('[').map_or(body, |at| &body[at..]);
                    match serde_json::from_str::<Value>(body) {
                        Ok(Value::Array(mut items)) if !items.is_empty() => {
                            let data = if items.len() > 1 {
                                items.remove(1)
                            } else {
                                Value::Null
                            };
                            let name = match items.remove(0) {
                                Value::String(s) => s,
                                other => other.to_string(),
                            };
                            Packet::Event(name, data)
                        }
                        _ => Packet::Other(packet.to_string()),
                    }
                }
                _ => Packet::Other(packet.to_string()),
            }
        }
        _ => Packet::Other(packet.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn payloads_round_trip_and_count_as_javascript_does() {
        let e = event("pushState", &json!({"title": "Café — “40”"}));
        let payload = encode_payload([e.as_str(), "2", "40"]);
        assert!(payload.starts_with(&format!("{}:42[", js_len(&e))));
        assert!(payload.ends_with("1:22:40"));
        let back = decode_payload(&payload).unwrap();
        assert_eq!(back, vec![e.clone(), "2".to_string(), "40".to_string()]);
        // An astral character is two units.
        let smile = "42[\"x\",\"\u{1F600}\"]";
        assert_eq!(js_len(smile), smile.chars().count() + 1);
        assert_eq!(
            decode_payload(&encode_payload([smile])).unwrap(),
            vec![smile]
        );
        assert!(decode_payload("5:42[").is_err());
        assert!(decode_payload("x:1").is_err());
    }

    #[test]
    fn a_streamed_payload_passes_over_what_is_too_long() {
        let big = format!("42[\"pushQueue\",\"{}\"]", "x".repeat(5000));
        let smile = "42[\"x\",\"\u{1F600}é\"]";
        let payload = encode_payload(["3", big.as_str(), smile, "6"]);
        let mut bytes = payload.as_bytes();
        let packets = read_payload(&mut bytes, 1024).unwrap();
        assert_eq!(packets, vec!["3", smile, "6"]);
        let mut bytes = payload.as_bytes();
        let packets = read_payload(&mut bytes, 8192).unwrap();
        assert_eq!(packets.len(), 4);
        assert_eq!(packets[1], big);
        assert!(read_payload(&mut &b"5:42["[..], 100).is_err());
        assert!(read_payload(&mut &b"x:1"[..], 100).is_err());
        assert!(read_payload(&mut &b""[..], 100).unwrap().is_empty());
    }

    #[test]
    fn the_binary_framing_is_read_too() {
        // As an older server sends it: type 0, the length's digits one a
        // byte, 255, then the string; a byte packet in between is passed
        // over; a non-ASCII title still counts as JavaScript does.
        let open = "0{\"sid\":\"abcdefghijklmnopqrst\",\"upgrades\":[],\"pingInterval\":25000,\"pingTimeout\":60000}";
        let n = js_len(open);
        assert_eq!(n, 86);
        let title = "42[\"pushState\",{\"title\":\"Caf\u{e9} \u{1F600}\"}]";
        let mut raw = vec![0u8, (n / 10) as u8, (n % 10) as u8, 255];
        raw.extend_from_slice(open.as_bytes());
        raw.extend_from_slice(&[1u8, 3, 255, 0xAA, 0xBB, 0xCC]);
        raw.extend_from_slice(&[0u8, 2, 255]);
        raw.extend_from_slice(b"40");
        let n = js_len(title);
        raw.extend_from_slice(&[0u8, (n / 10) as u8, (n % 10) as u8, 255]);
        raw.extend_from_slice(title.as_bytes());
        let packets = read_payload(&mut &raw[..], 4096).unwrap();
        assert_eq!(packets, vec![open, "40", title]);
        assert!(read_payload(&mut &[0u8, 9, 255, b'x'][..], 100).is_err());
        assert!(read_payload(&mut &[2u8, 1, 255, b'x'][..], 100).is_err());
        assert!(read_payload(&mut &[0u8, 1, 10, 255, b'x'][..], 100).is_err());
    }

    #[test]
    fn packets_are_read() {
        assert_eq!(
            parse("0{\"sid\":\"abc\",\"pingInterval\":25000}"),
            Packet::Open(json!({"sid": "abc", "pingInterval": 25000}))
        );
        assert_eq!(parse("2"), Packet::Ping);
        assert_eq!(parse("3"), Packet::Pong);
        assert_eq!(parse("6"), Packet::Noop);
        assert_eq!(parse("1"), Packet::Close);
        assert_eq!(parse("40"), Packet::Connected);
        assert_eq!(parse("41"), Packet::Disconnected);
        assert_eq!(
            parse("42[\"getState\",\"\"]"),
            Packet::Event("getState".into(), json!(""))
        );
        assert_eq!(
            parse("42[\"initSocket\",{\"id\":\"u\"}]"),
            Packet::Event("initSocket".into(), json!({"id": "u"}))
        );
        assert_eq!(
            parse("42[\"only\"]"),
            Packet::Event("only".into(), Value::Null)
        );
        assert_eq!(parse("43[]"), Packet::Other("43[]".into()));
    }
}

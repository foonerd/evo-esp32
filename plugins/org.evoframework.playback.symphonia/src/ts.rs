// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! MPEG transport stream to the audio elementary stream.
//!
//! An HLS segment is a transport stream: 188-byte packets, each with a
//! PID. The program association table (PID 0) names the program map
//! table, the map names the elementary streams, and the audio one's
//! packets carry PES packets whose payload is the elementary stream:
//! ADTS frames for AAC (stream type 0x0F), MPEG audio frames for MP3
//! (0x03, 0x04). The demuxer takes bytes as they arrive, in any pieces,
//! and gives the elementary stream back; a decoder that reads ADTS or
//! MP3 takes it from there. Video and other streams are passed over.

/// A packet's length.
const PACKET: usize = 188;
const SYNC: u8 = 0x47;

/// The audio the stream carries, by the map's stream type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Audio {
    /// AAC in ADTS frames.
    Adts,
    /// MPEG-1/2 audio (MP3 and its kin).
    Mpeg,
}

impl Audio {
    /// The media type a decoder would be told.
    pub fn content_type(self) -> &'static str {
        match self {
            Audio::Adts => "audio/aac",
            Audio::Mpeg => "audio/mpeg",
        }
    }

    fn from_stream_type(t: u8) -> Option<Self> {
        match t {
            0x0F => Some(Audio::Adts),
            0x03 | 0x04 => Some(Audio::Mpeg),
            _ => None,
        }
    }
}

/// The demuxer's state across packets.
#[derive(Debug, Default)]
pub struct Demuxer {
    /// Bytes of a packet not yet whole.
    partial: Vec<u8>,
    /// The program map's PID, once the association table said.
    pmt_pid: Option<u16>,
    /// The audio stream's PID and kind, once the map said.
    audio: Option<(u16, Audio)>,
    /// Inside a PES packet's payload on the audio PID.
    in_pes: bool,
    /// Packets seen, for the error messages.
    packets: u64,
}

impl Demuxer {
    /// A demuxer waiting for the tables.
    pub fn new() -> Self {
        Self::default()
    }

    /// The audio the map named, once seen.
    pub fn audio(&self) -> Option<Audio> {
        self.audio.map(|(_, a)| a)
    }

    /// Take `bytes` of the transport stream; the elementary stream that
    /// came out of them is appended to `out`.
    pub fn push(
        &mut self,
        bytes: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<(), String> {
        let mut rest = bytes;
        if !self.partial.is_empty() {
            let need = PACKET - self.partial.len();
            let take = need.min(rest.len());
            self.partial.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if self.partial.len() < PACKET {
                return Ok(());
            }
            let packet = std::mem::take(&mut self.partial);
            self.packet(&packet, out)?;
        }
        while rest.len() >= PACKET {
            if rest[0] != SYNC {
                // Out of step: slide to the next sync byte.
                match rest.iter().position(|&b| b == SYNC) {
                    Some(at) => {
                        rest = &rest[at..];
                        continue;
                    }
                    None => return Ok(()),
                }
            }
            self.packet(&rest[..PACKET], out)?;
            rest = &rest[PACKET..];
        }
        self.partial.extend_from_slice(rest);
        Ok(())
    }

    fn packet(&mut self, p: &[u8], out: &mut Vec<u8>) -> Result<(), String> {
        self.packets += 1;
        if p[0] != SYNC {
            return Err(format!("packet {} out of sync", self.packets));
        }
        let error = p[1] & 0x80 != 0;
        let start = p[1] & 0x40 != 0;
        let pid = (u16::from(p[1] & 0x1F) << 8) | u16::from(p[2]);
        let adaptation = (p[3] >> 4) & 0b11;
        if error || adaptation & 0b01 == 0 {
            // Damaged, or no payload.
            return Ok(());
        }
        let mut at = 4;
        if adaptation & 0b10 != 0 {
            let len = usize::from(p[4]);
            at = 5 + len;
            if at > PACKET {
                return Ok(());
            }
        }
        let payload = &p[at..];
        match (pid, self.pmt_pid, self.audio) {
            (0, _, _) => self.pat(payload, start),
            (pid, Some(pmt), None) if pid == pmt => self.pmt(payload, start),
            (pid, _, Some((audio, _))) if pid == audio => {
                self.pes(payload, start, out);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// A table section's body after the pointer field and the 8-byte
    /// section header; `None` when this packet does not start one.
    fn section(payload: &[u8], start: bool, table_id: u8) -> Option<&[u8]> {
        if !start || payload.is_empty() {
            return None;
        }
        let pointer = usize::from(payload[0]);
        let s = payload.get(1 + pointer..)?;
        if s.len() < 8 || s[0] != table_id {
            return None;
        }
        let length = usize::from(((s[1] & 0x0F) as u16) << 8 | s[2] as u16);
        // The section runs `length` bytes after byte 2, with a 4-byte CRC
        // at its end; the body follows the 5 bytes of ids and numbers.
        let end = (3 + length).min(s.len()).saturating_sub(4);
        s.get(8..end)
    }

    fn pat(&mut self, payload: &[u8], start: bool) -> Result<(), String> {
        let Some(body) = Self::section(payload, start, 0x00) else {
            return Ok(());
        };
        for entry in body.chunks_exact(4) {
            let program = (u16::from(entry[0]) << 8) | u16::from(entry[1]);
            let pid = (u16::from(entry[2] & 0x1F) << 8) | u16::from(entry[3]);
            if program != 0 {
                self.pmt_pid = Some(pid);
                return Ok(());
            }
        }
        Ok(())
    }

    fn pmt(&mut self, payload: &[u8], start: bool) -> Result<(), String> {
        let Some(body) = Self::section(payload, start, 0x02) else {
            return Ok(());
        };
        if body.len() < 4 {
            return Ok(());
        }
        let info_len =
            usize::from(((body[2] & 0x0F) as u16) << 8 | body[3] as u16);
        let mut rest = body.get(4 + info_len..).unwrap_or(&[]);
        let mut found: Option<(u16, Audio)> = None;
        while rest.len() >= 5 {
            let stream_type = rest[0];
            let pid = (u16::from(rest[1] & 0x1F) << 8) | u16::from(rest[2]);
            let es_len =
                usize::from(((rest[3] & 0x0F) as u16) << 8 | rest[4] as u16);
            if let Some(audio) = Audio::from_stream_type(stream_type) {
                // The first audio stream is the one; a map with several
                // languages is beyond a radio stream.
                found.get_or_insert((pid, audio));
            }
            rest = rest.get(5 + es_len..).unwrap_or(&[]);
        }
        match found {
            Some(a) => {
                self.audio = Some(a);
                Ok(())
            }
            None => Err("no audio stream in the program map".to_string()),
        }
    }

    fn pes(&mut self, payload: &[u8], start: bool, out: &mut Vec<u8>) {
        if start {
            // A PES header: 00 00 01, stream id, length, then for audio
            // two flag bytes and the header data length.
            if payload.len() < 9 || payload[..3] != [0, 0, 1] {
                self.in_pes = false;
                return;
            }
            let header_len = usize::from(payload[8]);
            let Some(data) = payload.get(9 + header_len..) else {
                self.in_pes = false;
                return;
            };
            self.in_pes = true;
            out.extend_from_slice(data);
        } else if self.in_pes {
            out.extend_from_slice(payload);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A transport packet with `payload`, padded with an adaptation field
    /// when short, as a muxer writes one.
    fn packet(pid: u16, start: bool, counter: u8, payload: &[u8]) -> Vec<u8> {
        assert!(payload.len() <= 184);
        let mut p = vec![
            SYNC,
            (if start { 0x40 } else { 0 }) | (pid >> 8) as u8,
            pid as u8,
        ];
        let pad = 184 - payload.len();
        if pad == 0 {
            p.push(0x10 | (counter & 0x0F));
        } else {
            p.push(0x30 | (counter & 0x0F));
            p.push((pad - 1) as u8);
            if pad > 1 {
                p.push(0);
                p.extend(std::iter::repeat_n(0xFF, pad - 2));
            }
        }
        p.extend_from_slice(payload);
        assert_eq!(p.len(), PACKET);
        p
    }

    fn section(table_id: u8, body: &[u8]) -> Vec<u8> {
        let length = 5 + body.len() + 4;
        let mut s = vec![0u8]; // pointer field
        s.push(table_id);
        s.push(0xB0 | ((length >> 8) as u8 & 0x0F));
        s.push(length as u8);
        s.extend_from_slice(&[0, 1, 0xC1, 0, 0]);
        s.extend_from_slice(body);
        s.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        s
    }

    fn pes(stream_id: u8, es: &[u8]) -> Vec<u8> {
        let mut p =
            vec![0, 0, 1, stream_id, 0, 0, 0x80, 0x80, 5, 0x21, 0, 1, 0, 1];
        p.extend_from_slice(es);
        p
    }

    #[test]
    fn tables_are_read_and_the_audio_pes_comes_out() {
        let pat = section(0x00, &[0x00, 0x01, 0xE1, 0x00]); // program 1 -> PMT 0x100
        let pmt_body = {
            let mut b = vec![0xE1, 0x01, 0xF0, 0x00]; // PCR PID, no program info
            b.extend_from_slice(&[0x1B, 0xE1, 0x01, 0xF0, 0x00]); // H.264 on 0x101
            b.extend_from_slice(&[0x0F, 0xE1, 0x02, 0xF0, 0x00]); // ADTS AAC on 0x102
            b
        };
        let pmt = section(0x02, &pmt_body);
        let adts: Vec<u8> = (0..300u32).map(|i| (i % 251) as u8).collect();
        let first = pes(0xC0, &adts[..170]);
        let mut stream = Vec::new();
        stream.extend(packet(0, true, 0, &pat));
        stream.extend(packet(0x100, true, 0, &pmt));
        stream.extend(packet(0x101, true, 0, &pes(0xE0, &[9; 100]))); // video, passed over
        stream.extend(packet(0x102, true, 0, &first));
        stream.extend(packet(0x102, false, 1, &adts[170..]));
        stream.extend(packet(0x1FFF, false, 0, &[0xFF; 184])); // null packet

        // Whole, and in odd pieces, the same elementary stream comes out.
        let mut d = Demuxer::new();
        let mut out = Vec::new();
        d.push(&stream, &mut out).unwrap();
        assert_eq!(d.audio(), Some(Audio::Adts));
        assert_eq!(out, adts);

        let mut d = Demuxer::new();
        let mut out = Vec::new();
        for piece in stream.chunks(61) {
            d.push(piece, &mut out).unwrap();
        }
        assert_eq!(out, adts);

        // Leading rubbish before the first sync byte is slid over.
        let mut d = Demuxer::new();
        let mut out = Vec::new();
        let mut dirty = vec![1, 2, 3];
        dirty.extend_from_slice(&stream);
        d.push(&dirty, &mut out).unwrap();
        assert_eq!(out, adts);
    }

    #[test]
    fn a_map_without_audio_is_refused_and_mpeg_audio_is_named() {
        let pat = section(0x00, &[0x00, 0x01, 0xE1, 0x00]);
        let video_only = section(
            0x02,
            &[0xE1, 0x01, 0xF0, 0x00, 0x1B, 0xE1, 0x01, 0xF0, 0x00],
        );
        let mut d = Demuxer::new();
        let mut out = Vec::new();
        d.push(&packet(0, true, 0, &pat), &mut out).unwrap();
        assert!(d
            .push(&packet(0x100, true, 0, &video_only), &mut out)
            .is_err());

        let mp3 = section(
            0x02,
            &[0xE1, 0x01, 0xF0, 0x00, 0x03, 0xE1, 0x01, 0xF0, 0x00],
        );
        let mut d = Demuxer::new();
        d.push(&packet(0, true, 0, &pat), &mut out).unwrap();
        d.push(&packet(0x100, true, 0, &mp3), &mut out).unwrap();
        assert_eq!(d.audio(), Some(Audio::Mpeg));
        assert_eq!(Audio::Mpeg.content_type(), "audio/mpeg");
    }
}

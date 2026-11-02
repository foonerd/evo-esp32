// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! FLAC as Snapcast sends it: a stream header once, then chunks that
//! each hold one or more whole frames (the server's encoder emits a
//! frame at a time and a chunk is what it emitted for 50 ms of input,
//! at its default level 2 blocks of 1152 frames). Symphonia's decoder
//! takes one frame per packet and says nothing about where the frame
//! ended, so a chunk is split on frame headers first: the 14-bit sync
//! code, a header that parses, and the header's CRC-8.

use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::audio::well_known::CODEC_ID_FLAC;
use symphonia::core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions,
};
use symphonia::core::packet::Packet;
use symphonia::core::units::{Duration, Timestamp};
use symphonia::default::codecs::FlacDecoder;

/// What the stream header says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamInfo {
    /// Frames per second.
    pub rate: u32,
    /// Channels per frame.
    pub channels: u8,
    /// Bits per sample.
    pub bits: u8,
    /// The STREAMINFO block's body, what the decoder starts from.
    pub block: Vec<u8>,
}

/// The STREAMINFO out of a `fLaC` header (the marker, then metadata
/// blocks; STREAMINFO is the first and 34 bytes).
pub fn stream_info(header: &[u8]) -> Result<StreamInfo, String> {
    if header.len() < 4 + 4 + 34 || &header[0..4] != b"fLaC" {
        return Err("not a fLaC header".into());
    }
    let mut at = 4;
    while at + 4 <= header.len() {
        let kind = header[at] & 0x7f;
        let last = header[at] & 0x80 != 0;
        let len = usize::from(header[at + 1]) << 16
            | usize::from(header[at + 2]) << 8
            | usize::from(header[at + 3]);
        let body = header
            .get(at + 4..at + 4 + len)
            .ok_or_else(|| "metadata block short".to_string())?;
        if kind == 0 {
            if body.len() < 34 {
                return Err("STREAMINFO short".into());
            }
            let rate = u32::from(body[10]) << 12
                | u32::from(body[11]) << 4
                | u32::from(body[12]) >> 4;
            let channels = ((body[12] >> 1) & 0x07) + 1;
            let bits = ((body[12] & 0x01) << 4 | body[13] >> 4) + 1;
            return Ok(StreamInfo {
                rate,
                channels,
                bits,
                block: body.to_vec(),
            });
        }
        if last {
            break;
        }
        at += 4 + len;
    }
    Err("no STREAMINFO".into())
}

/// CRC-8, polynomial 0x07, as FLAC's frame header carries.
fn crc8(bytes: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &b in bytes {
        crc ^= b;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 0x07
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// The length of a valid FLAC frame header starting at `b[0]`, or
/// `None` when no frame starts here.
fn frame_header_len(b: &[u8]) -> Option<usize> {
    if b.len() < 6 || b[0] != 0xff || b[1] & 0xfe != 0xf8 {
        return None;
    }
    let block_code = b[2] >> 4;
    let rate_code = b[2] & 0x0f;
    let channel_code = b[3] >> 4;
    let size_code = (b[3] >> 1) & 0x07;
    if block_code == 0
        || rate_code == 0x0f
        || channel_code > 0x0a
        || size_code == 0x03
        || b[3] & 0x01 != 0
    {
        return None;
    }
    // The UTF-8 coded frame or sample number: its length from the
    // first byte.
    let lead = b[4];
    let number_len = match lead {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        0xf8..=0xfb => 5,
        0xfc..=0xfd => 6,
        0xfe => 7,
        _ => return None,
    };
    let mut len = 4 + number_len;
    for i in 1..number_len {
        if b.get(4 + i).is_none_or(|x| x & 0xc0 != 0x80) {
            return None;
        }
    }
    len += match block_code {
        0x06 => 1,
        0x07 => 2,
        _ => 0,
    };
    len += match rate_code {
        0x0c => 1,
        0x0d | 0x0e => 2,
        _ => 0,
    };
    // The CRC-8 closes the header.
    let crc = *b.get(len)?;
    (crc8(&b[..len]) == crc).then_some(len + 1)
}

/// The frames in `chunk`, as byte ranges: each begins at a valid frame
/// header and runs to the next one or the end.
pub fn split_frames(chunk: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut at = 0;
    while at + 2 <= chunk.len() {
        if let Some(header) = frame_header_len(&chunk[at..]) {
            starts.push(at);
            // No frame is shorter than its header.
            at += header;
        } else {
            at += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(i, &s)| {
            let end = starts.get(i + 1).copied().unwrap_or(chunk.len());
            &chunk[s..end]
        })
        .collect()
}

/// A FLAC decoder for one stream, frame in, interleaved i16 out.
pub struct Flac {
    decoder: FlacDecoder,
    /// What the stream header said.
    pub info: StreamInfo,
}

impl Flac {
    /// From the stream header the server sent.
    pub fn new(header: &[u8]) -> Result<Self, String> {
        let info = stream_info(header)?;
        let mut params = AudioCodecParameters::new();
        params
            .for_codec(CODEC_ID_FLAC)
            .with_extra_data(info.block.clone().into_boxed_slice());
        let decoder =
            FlacDecoder::try_new(&params, &AudioDecoderOptions::default())
                .map_err(|e| format!("flac decoder: {e}"))?;
        Ok(Self { decoder, info })
    }

    /// Decode every frame in `chunk` into `out`, appending; the number
    /// of frames (per channel) appended. A frame that will not decode
    /// is skipped and counted in the second value.
    pub fn decode_chunk(
        &mut self,
        chunk: &[u8],
        out: &mut Vec<i16>,
    ) -> (usize, usize) {
        let (mut frames, mut bad) = (0, 0);
        for frame in split_frames(chunk) {
            let packet =
                Packet::new(0, Timestamp::new(0), Duration::new(0), frame);
            match self.decoder.decode(&packet) {
                Ok(buf) => {
                    frames += buf.frames();
                    append_interleaved(&buf, out);
                }
                Err(_) => bad += 1,
            }
        }
        (frames, bad)
    }
}

fn append_interleaved(buf: &GenericAudioBufferRef<'_>, out: &mut Vec<i16>) {
    let mut part = Vec::new();
    buf.copy_to_vec_interleaved::<i16>(&mut part);
    out.extend_from_slice(&part);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A STREAMINFO for 48 kHz, stereo, 16 bit, blocks of 1152.
    fn streaminfo() -> Vec<u8> {
        let mut b = vec![0u8; 34];
        b[0..2].copy_from_slice(&1152u16.to_be_bytes());
        b[2..4].copy_from_slice(&1152u16.to_be_bytes());
        // 48 000 Hz over 20 bits, then channels-1 (1) over 3, then
        // bits-1 (15) over 5.
        let rate = 48_000u32;
        b[10] = (rate >> 12) as u8;
        b[11] = (rate >> 4) as u8;
        b[12] = ((rate & 0x0f) << 4) as u8 | (1 << 1);
        b[13] = 0xf0;
        b
    }

    fn header_with(streaminfo: &[u8]) -> Vec<u8> {
        let mut h = b"fLaC".to_vec();
        h.push(0x80);
        h.extend_from_slice(&[0, 0, 34]);
        h.extend_from_slice(streaminfo);
        h
    }

    #[test]
    fn the_stream_header_gives_the_format() {
        let info = stream_info(&header_with(&streaminfo())).unwrap();
        assert_eq!((info.rate, info.channels, info.bits), (48_000, 2, 16));
        assert_eq!(info.block.len(), 34);
        assert!(stream_info(b"fLaCxx").is_err());
    }

    /// A frame header: sync, block size code 0x6 (8-bit follows),
    /// 48 kHz code 0xa, stereo 0x1, 16-bit 0x4, frame number 0,
    /// block size 255+1, CRC.
    fn frame_header(frame_number: u8) -> Vec<u8> {
        let mut h = vec![0xff, 0xf8, 0x6a, 0x18, frame_number, 0xff];
        let crc = crc8(&h);
        h.push(crc);
        h
    }

    #[test]
    fn frame_headers_are_told_from_data_by_structure_and_crc() {
        let h = frame_header(0);
        assert_eq!(frame_header_len(&h), Some(7));
        let mut broken = h.clone();
        broken[6] ^= 1;
        assert_eq!(frame_header_len(&broken), None);
        // Sync bytes inside data without a header behind them.
        assert_eq!(
            frame_header_len(&[0xff, 0xf8, 0x00, 0x00, 0x00, 0x00, 0x00]),
            None
        );
        assert_eq!(
            frame_header_len(&[0xff, 0xf8, 0x6a, 0x18, 0x00, 0xff, 0x00]),
            None
        );
    }

    #[test]
    fn a_chunk_splits_on_its_frames() {
        let mut chunk = Vec::new();
        let a = frame_header(0);
        let b = frame_header(1);
        chunk.extend_from_slice(&a);
        chunk.extend_from_slice(&[1, 2, 3, 0xff, 0xf8, 9]); // data, a false sync
        chunk.extend_from_slice(&b);
        chunk.extend_from_slice(&[4, 5]);
        let frames = split_frames(&chunk);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].len(), a.len() + 6);
        assert_eq!(frames[1].len(), b.len() + 2);
        assert_eq!(&frames[1][..b.len()], &b[..]);
    }

    #[test]
    fn the_decoder_starts_from_the_header_and_takes_a_frame() {
        let mut flac = Flac::new(&header_with(&streaminfo())).unwrap();
        assert_eq!((flac.info.rate, flac.info.channels), (48_000, 2));
        let mut out = Vec::new();
        // A frame header for 256 frames followed by zero bytes: two
        // constant subframes of silence, as far as the decoder is
        // concerned (it does not check the frame's CRC).
        let mut chunk = frame_header(0);
        chunk.extend_from_slice(&[0u8; 20]);
        let (frames, bad) = flac.decode_chunk(&chunk, &mut out);
        assert_eq!((frames, bad), (256, 0));
        assert_eq!(out.len(), 512);
        assert!(out.iter().all(|&s| s == 0));
    }
}

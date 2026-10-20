// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Espressif's AAC decoder as a Symphonia [`AudioDecoder`].
//!
//! Symphonia's ADTS demuxer hands one raw AAC frame per packet, the
//! ADTS header taken off; this wrapper puts a header back (AAC-LC,
//! the rate and channel count the demuxer read) and gives the frame to
//! `esp_aac_dec_decode`, which returns 16-bit interleaved PCM. AAC-Plus
//! (HE-AAC v1 and v2, the form most AAC stations take) is decoded in
//! full: the spectral band replication is signalled inside the frame,
//! not in the header, and the decoder reports the rate it comes out at
//! (twice the header's), which the [`AudioBuffer`]'s signal spec then
//! carries; the player follows the decoder, not the container.
//!
//! Why not Symphonia's own: its decoder is built on the stack before it
//! is boxed, tens of kilobytes of it, more than the board's decoding
//! task has; and it decodes the AAC-LC core alone. Registered at
//! [`Tier::Preferred`] by [`register`]; the firmware builds the playback
//! plugin without Symphonia's AAC decoder, so nothing falls through to
//! it.

use std::ffi::c_void;
use std::ptr;

use esp_idf_svc::sys::esp_audio_codec as c;
use symphonia_core::audio::{
    AsGenericAudioBufferRef, Audio, AudioBuffer, AudioSpec, Channels,
    GenericAudioBufferRef, Position,
};
use symphonia_core::codecs::audio::well_known::CODEC_ID_AAC;
use symphonia_core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia_core::codecs::registry::{
    CodecRegistry, RegisterableAudioDecoder, SupportedAudioCodec,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::common::Tier;
use symphonia_core::errors::{decode_error, unsupported_error, Result};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

/// Frames in an AAC frame after AAC-Plus doubles them.
const MAX_FRAMES: usize = 2048;
/// An ADTS header without a CRC.
const HEADER: usize = 7;

/// Put Espressif's AAC decoder in `registry`, preferred over any other.
pub fn register(registry: &mut CodecRegistry) {
    registry.register_audio_decoder_at_tier::<EspAac>(Tier::Preferred);
}

/// Espressif's AAC decoder, one stream's worth.
pub struct EspAac {
    handle: *mut c_void,
    params: AudioCodecParameters,
    /// The ADTS header for this stream's rate and channels, the frame
    /// length filled in per packet.
    header: [u8; HEADER],
    /// Header and frame together, as the decoder reads them.
    frame: Vec<u8>,
    /// What the decoder wrote, interleaved; grown if it asks for more.
    pcm: Vec<u8>,
    buf: AudioBuffer<i16>,
}

// SAFETY: the handle is only ever used through `&mut self`, by one task.
unsafe impl Send for EspAac {}
// SAFETY: as above; nothing is shared behind `&self` but plain data.
unsafe impl Sync for EspAac {}

impl Drop for EspAac {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful open and is closed once.
        unsafe { c::esp_aac_dec_close(self.handle) };
    }
}

/// Speaker positions for a channel count the decoder reports.
fn channels_for(count: u8) -> Channels {
    match count {
        1 => Channels::Positioned(Position::FRONT_LEFT),
        2 => Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT),
        n => Channels::Discrete(u16::from(n)),
    }
}

/// The ADTS sampling frequency index for `rate`, if it is one AAC has.
fn frequency_index(rate: u32) -> Option<u8> {
    const RATES: [u32; 13] = [
        96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000,
        11025, 8000, 7350,
    ];
    RATES.iter().position(|&r| r == rate).map(|i| i as u8)
}

/// An ADTS header (MPEG-4, AAC-LC, no CRC) for frames of `rate` and
/// `channels`; the frame length is left at zero for [`with_length`].
fn adts_header(rate: u32, channels: u8) -> Option<[u8; HEADER]> {
    let index = frequency_index(rate)?;
    // 8 channels is configuration 7; more has no configuration.
    let config = match channels {
        1..=7 => channels,
        8 => 7,
        _ => return None,
    };
    // Profile 1 is AAC-LC (object type 2, minus one).
    Some([
        0xFF,
        0xF1,
        (1 << 6) | (index << 2) | (config >> 2),
        (config & 0x3) << 6,
        0,
        0x1F,
        0xFC,
    ])
}

/// `header` with the frame length (header included) filled in.
fn with_length(header: &[u8; HEADER], total: usize) -> [u8; HEADER] {
    let len = total as u32 & 0x1FFF;
    let mut h = *header;
    h[3] |= (len >> 11) as u8;
    h[4] = (len >> 3) as u8;
    h[5] |= ((len & 0x7) << 5) as u8;
    h
}

impl EspAac {
    fn open(params: &AudioCodecParameters) -> Result<Self> {
        if params.codec != CODEC_ID_AAC {
            return unsupported_error("esp aac: not aac");
        }
        let rate = params.sample_rate.unwrap_or(0);
        let channels = params
            .channels
            .as_ref()
            .map_or(2, |ch| ch.count().clamp(1, 8) as u8);
        let Some(header) = adts_header(rate, channels) else {
            return unsupported_error("esp aac: rate or channels not AAC's");
        };
        let cfg = c::esp_aac_dec_cfg_t {
            sample_rate: rate as i32,
            channel: channels,
            bits_per_sample: 16,
            no_adts_header: false,
            aac_plus_enable: true,
        };
        let mut handle: *mut c_void = ptr::null_mut();
        // SAFETY: `cfg` is a plain struct alive for the call; `handle`
        // receives the instance on success.
        let ret = unsafe {
            c::esp_aac_dec_open(
                ptr::from_ref(&cfg).cast_mut().cast(),
                size_of::<c::esp_aac_dec_cfg_t>() as u32,
                &mut handle,
            )
        };
        if ret != c::esp_audio_err_t_ESP_AUDIO_ERR_OK || handle.is_null() {
            log::error!("esp aac: open failed ({ret})");
            return decode_error("esp aac: open failed");
        }
        Ok(Self {
            handle,
            params: params.clone(),
            header,
            frame: Vec::with_capacity(HEADER + 2048),
            pcm: vec![0; MAX_FRAMES * 2 * 2],
            buf: AudioBuffer::default(),
        })
    }

    /// Decode one packet into `self.buf`; frames decoded.
    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<usize> {
        let total = HEADER + packet.data.len();
        self.frame.clear();
        self.frame
            .extend_from_slice(&with_length(&self.header, total));
        self.frame.extend_from_slice(packet.data);
        let mut raw = c::esp_audio_dec_in_raw_t {
            buffer: self.frame.as_mut_ptr(),
            len: self.frame.len() as u32,
            consumed: 0,
            frame_recover:
                c::esp_audio_dec_recovery_t_ESP_AUDIO_DEC_RECOVERY_NONE,
        };
        // SAFETY: a plain-data struct the decoder fills in.
        let mut info: c::esp_audio_dec_info_t = unsafe { std::mem::zeroed() };
        let decoded = loop {
            let mut frame = c::esp_audio_dec_out_frame_t {
                buffer: self.pcm.as_mut_ptr(),
                len: self.pcm.len() as u32,
                needed_size: 0,
                decoded_size: 0,
            };
            // SAFETY: `raw` points into `self.frame` for the call's
            // duration, `frame.buffer` into `self.pcm` with its true length.
            let ret = unsafe {
                c::esp_aac_dec_decode(
                    self.handle,
                    &mut raw,
                    &mut frame,
                    &mut info,
                )
            };
            match ret {
                c::esp_audio_err_t_ESP_AUDIO_ERR_OK => {
                    break frame.decoded_size as usize
                }
                c::esp_audio_err_t_ESP_AUDIO_ERR_BUFF_NOT_ENOUGH => {
                    self.pcm.resize(frame.needed_size as usize, 0);
                }
                _ => {
                    log::warn!("esp aac: decode failed ({ret})");
                    return decode_error("esp aac: decode failed");
                }
            }
        };
        if decoded == 0 {
            self.buf.clear();
            return Ok(0);
        }
        if info.bits_per_sample != 16 || info.channel == 0 {
            return unsupported_error("esp aac: output is not 16-bit PCM");
        }
        let channels = usize::from(info.channel);
        let frames = decoded / (channels * 2);
        let spec = AudioSpec::new(info.sample_rate, channels_for(info.channel));
        if self.buf.is_unused()
            || self.buf.spec() != &spec
            || self.buf.capacity() < frames
        {
            self.buf = AudioBuffer::new(spec, frames.max(MAX_FRAMES));
        }
        self.buf.clear();
        let pcm = &self.pcm;
        self.buf.render_with(Some(frames), |i, planes| {
            for (ch, plane) in planes.iter_mut().enumerate().take(channels) {
                let at = (i * channels + ch) * 2;
                plane[i] = i16::from_le_bytes([pcm[at], pcm[at + 1]]);
            }
            Ok(())
        })?;
        Ok(frames)
    }
}

impl AudioDecoder for EspAac {
    fn codec_info(&self) -> &CodecInfo {
        &Self::supported_codecs()[0].info
    }

    fn codec_params(&self) -> &AudioCodecParameters {
        &self.params
    }

    fn reset(&mut self) {
        // SAFETY: the handle is open; reset is the decoder's own seek path.
        let ret = unsafe { c::esp_aac_dec_reset(self.handle) };
        if ret != c::esp_audio_err_t_ESP_AUDIO_ERR_OK {
            log::warn!("esp aac: reset failed ({ret})");
        }
        self.buf.clear();
    }

    fn decode_ref(
        &mut self,
        packet: &PacketRef<'_>,
    ) -> Result<GenericAudioBufferRef<'_>> {
        if let Err(e) = self.decode_inner(packet) {
            self.buf.clear();
            return Err(e);
        }
        Ok(self.buf.as_generic_audio_buffer_ref())
    }

    fn finalize(&mut self) -> FinalizeResult {
        FinalizeResult::default()
    }

    fn last_decoded(&self) -> GenericAudioBufferRef<'_> {
        self.buf.as_generic_audio_buffer_ref()
    }
}

impl RegisterableAudioDecoder for EspAac {
    fn try_registry_new(
        params: &AudioCodecParameters,
        _opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>> {
        Ok(Box::new(Self::open(params)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(
            CODEC_ID_AAC,
            "aac",
            "Espressif AAC (AAC-Plus)"
        )]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_header_is_as_adts_has_it() {
        // 44.1 kHz stereo, a 7 + 353 byte frame.
        let h = with_length(&adts_header(44100, 2).unwrap(), 360);
        assert_eq!(h[0], 0xFF);
        assert_eq!(h[1], 0xF1, "MPEG-4, layer 0, no CRC");
        assert_eq!(h[2] >> 6, 1, "AAC-LC");
        assert_eq!((h[2] >> 2) & 0xF, 4, "44.1 kHz");
        assert_eq!(((h[2] & 0x3) << 2) | (h[3] >> 6), 2, "stereo");
        let len = (u32::from(h[3] & 0x3) << 11)
            | (u32::from(h[4]) << 3)
            | u32::from(h[5] >> 5);
        assert_eq!(len, 360);
        assert_eq!(h[5] & 0x1F, 0x1F);
        assert_eq!(h[6], 0xFC, "buffer fullness 0x7FF, one block");
        assert!(adts_header(11000, 2).is_none());
    }
}

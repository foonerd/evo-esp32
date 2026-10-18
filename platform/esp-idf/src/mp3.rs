// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Espressif's MP3 decoder as a Symphonia [`AudioDecoder`].
//!
//! Symphonia's MP3 demuxer hands one MPEG audio frame per packet; each
//! is passed whole to `esp_mp3_dec_decode`, which keeps the bit
//! reservoir across calls and returns 16-bit interleaved PCM. The PCM
//! is de-interleaved into an [`AudioBuffer<i16>`] whose signal spec
//! follows what the decoder reports, so a stream whose rate or channel
//! count differs from the container's parameters is still right.
//!
//! The decoder is registered at [`Tier::Preferred`], above Symphonia's
//! own, by [`register`].

use std::ffi::c_void;
use std::ptr;

use esp_idf_svc::sys::esp_audio_codec as c;
use symphonia_core::audio::{
    AsGenericAudioBufferRef, Audio, AudioBuffer, AudioSpec, Channels,
    GenericAudioBufferRef, Position,
};
use symphonia_core::codecs::audio::well_known::CODEC_ID_MP3;
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

/// Frames in the longest MPEG audio frame (layer 3, MPEG-1).
const MAX_FRAMES: usize = 1152;

/// Put Espressif's MP3 decoder in `registry`, preferred over any other.
pub fn register(registry: &mut CodecRegistry) {
    registry.register_audio_decoder_at_tier::<EspMp3>(Tier::Preferred);
}

/// Espressif's MP3 decoder, one stream's worth.
pub struct EspMp3 {
    handle: *mut c_void,
    params: AudioCodecParameters,
    /// What the decoder wrote, interleaved; grown if it asks for more.
    pcm: Vec<u8>,
    buf: AudioBuffer<i16>,
}

// SAFETY: the handle is only ever used through `&mut self`, by one task.
unsafe impl Send for EspMp3 {}
// SAFETY: as above; nothing is shared behind `&self` but plain data.
unsafe impl Sync for EspMp3 {}

impl Drop for EspMp3 {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful open and is closed once.
        unsafe { c::esp_mp3_dec_close(self.handle) };
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

impl EspMp3 {
    fn open(params: &AudioCodecParameters) -> Result<Self> {
        if params.codec != CODEC_ID_MP3 {
            return unsupported_error("esp mp3: not mp3");
        }
        let mut handle: *mut c_void = ptr::null_mut();
        // SAFETY: the decoder takes no configuration; `handle` receives the
        // instance on success.
        let ret =
            unsafe { c::esp_mp3_dec_open(ptr::null_mut(), 0, &mut handle) };
        if ret != c::esp_audio_err_t_ESP_AUDIO_ERR_OK || handle.is_null() {
            log::error!("esp mp3: open failed ({ret})");
            return decode_error("esp mp3: open failed");
        }
        Ok(Self {
            handle,
            params: params.clone(),
            pcm: vec![0; MAX_FRAMES * 2 * 2],
            buf: AudioBuffer::default(),
        })
    }

    /// Decode one packet into `self.buf`; frames decoded.
    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<usize> {
        let mut raw = c::esp_audio_dec_in_raw_t {
            buffer: packet.data.as_ptr().cast_mut(),
            len: packet.data.len() as u32,
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
            // SAFETY: `raw` points into the packet for the call's duration,
            // `frame.buffer` into `self.pcm` with its true length.
            let ret = unsafe {
                c::esp_mp3_dec_decode(
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
                    log::warn!("esp mp3: decode failed ({ret})");
                    return decode_error("esp mp3: decode failed");
                }
            }
        };
        if decoded == 0 {
            // A frame that only fed the bit reservoir.
            self.buf.clear();
            return Ok(0);
        }
        if info.bits_per_sample != 16 || info.channel == 0 {
            return unsupported_error("esp mp3: output is not 16-bit PCM");
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

impl AudioDecoder for EspMp3 {
    fn codec_info(&self) -> &CodecInfo {
        &Self::supported_codecs()[0].info
    }

    fn codec_params(&self) -> &AudioCodecParameters {
        &self.params
    }

    fn reset(&mut self) {
        // SAFETY: the handle is open; reset is the decoder's own seek path.
        let ret = unsafe { c::esp_mp3_dec_reset(self.handle) };
        if ret != c::esp_audio_err_t_ESP_AUDIO_ERR_OK {
            log::warn!("esp mp3: reset failed ({ret})");
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

impl RegisterableAudioDecoder for EspMp3 {
    fn try_registry_new(
        params: &AudioCodecParameters,
        _opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>> {
        Ok(Box::new(Self::open(params)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(
            CODEC_ID_MP3,
            "mp3",
            "MPEG Audio Layer 3 (Espressif)"
        )]
    }
}

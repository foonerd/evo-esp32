// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The board's frame sink: I2S0 with its DMA as the buffer.
//!
//! The decoder offers interleaved 16-bit frames; the sink hands the DMA
//! what it has room for with a zero timeout and says how many frames
//! that was, so the I2S clock paces playback and a tick never waits on
//! it. A frame cut by the DMA's boundary is carried whole into the next
//! call, and a mono stream is doubled into both slots on the way.
//!
//! The channel is made for one sample rate. A stream at another rate
//! gets the channel rebuilt (`new_std_tx` again over a conjured `I2S0`)
//! and the codec told; the header mirror survives, because the GPIO
//! matrix routes signals, not channels.

use esp_idf_svc::hal::gpio::AnyIOPin;
use esp_idf_svc::hal::i2s::config::{
    Config as ChannelConfig, DataBitWidth, SlotMode, StdClkConfig, StdConfig,
    StdGpioConfig, StdSlotConfig,
};
use esp_idf_svc::hal::i2s::{I2sDriver, I2sTx, I2S0};
use esp_idf_svc::sys;
use evo_esp32_sdk::contract::audio::playback::{Format, FrameSink};
use log::{info, warn};

/// Frames per DMA buffer and how many; together the lead the sink holds
/// (six of 1023 frames is 139 ms at 44.1 kHz).
const FRAMES_PER_BUFFER: u32 = 1023;
const BUFFERS: u32 = 6;
/// Bytes per stereo 16-bit frame on the wire.
const FRAME_BYTES: usize = 4;

/// What the sink needs to say to the codec when the rate changes.
pub trait RateFollower {
    /// The stream's rate and width from here on.
    fn rate_changed(&mut self, rate: u32, bits: u8);
}

/// I2S0 as a [`FrameSink`].
pub struct I2sSink<F> {
    /// `None` only while the channel is being remade for a new rate.
    i2s: Option<I2sDriver<'static, I2sTx>>,
    rate: u32,
    channels: usize,
    /// Bytes of a frame the DMA took in part; go out first next time.
    carry: Vec<u8>,
    scratch: Vec<u8>,
    follower: F,
    /// Frames taken since `start`, for the log.
    pub taken: u64,
}

impl<F: RateFollower> I2sSink<F> {
    /// The channel at `rate`, master, Philips, 16-bit stereo, on the
    /// board's codec pins; `follower` is told each rate the stream takes.
    pub fn new(
        i2s0: I2S0<'static>,
        bclk: impl esp_idf_svc::hal::gpio::InputPin
            + esp_idf_svc::hal::gpio::OutputPin
            + 'static,
        dout: impl esp_idf_svc::hal::gpio::OutputPin + 'static,
        ws: impl esp_idf_svc::hal::gpio::InputPin
            + esp_idf_svc::hal::gpio::OutputPin
            + 'static,
        rate: u32,
        follower: F,
    ) -> Result<Self, sys::EspError> {
        let i2s = open(i2s0, bclk, dout, ws, rate)?;
        Ok(Self {
            i2s: Some(i2s),
            rate,
            channels: 2,
            carry: Vec::with_capacity(FRAME_BYTES),
            scratch: Vec::new(),
            follower,
            taken: 0,
        })
    }

    fn driver(&mut self) -> &mut I2sDriver<'static, I2sTx> {
        self.i2s.as_mut().expect("the I2S channel is open")
    }

    /// Write `bytes` with no waiting; what the DMA took. A full DMA is
    /// a timeout with nothing taken, which is the contract's "ask again".
    fn offer(&mut self, bytes: &[u8]) -> usize {
        match self.driver().write(bytes, 0) {
            Ok(n) => n,
            Err(e) if e.code() == sys::ESP_ERR_TIMEOUT => 0,
            Err(e) => {
                warn!("i2s: write: {e}");
                0
            }
        }
    }

    /// Play a blocking tone through the channel; the boot's self-test.
    pub fn tone_blocking(&mut self, bytes: &[u8]) -> Result<(), sys::EspError> {
        self.driver().write_all(bytes, sys::TickType_t::MAX)
    }
}

fn open(
    i2s0: I2S0<'static>,
    bclk: impl esp_idf_svc::hal::gpio::InputPin
        + esp_idf_svc::hal::gpio::OutputPin
        + 'static,
    dout: impl esp_idf_svc::hal::gpio::OutputPin + 'static,
    ws: impl esp_idf_svc::hal::gpio::InputPin
        + esp_idf_svc::hal::gpio::OutputPin
        + 'static,
    rate: u32,
) -> Result<I2sDriver<'static, I2sTx>, sys::EspError> {
    let config = StdConfig::new(
        ChannelConfig::default()
            .auto_clear(true)
            .dma_buffer_count(BUFFERS)
            .frames_per_buffer(FRAMES_PER_BUFFER),
        StdClkConfig::from_sample_rate_hz(rate),
        StdSlotConfig::philips_slot_default(
            DataBitWidth::Bits16,
            SlotMode::Stereo,
        ),
        StdGpioConfig::default(),
    );
    let mut i2s =
        I2sDriver::new_std_tx(i2s0, &config, bclk, dout, None::<AnyIOPin>, ws)?;
    i2s.tx_enable()?;
    Ok(i2s)
}

impl<F: RateFollower> FrameSink for I2sSink<F> {
    fn start(&mut self, format: &Format) {
        self.channels = usize::from(format.channels).max(1);
        self.carry.clear();
        self.taken = 0;
        if format.sample_rate != self.rate && format.sample_rate > 0 {
            info!("i2s: {} Hz -> {} Hz", self.rate, format.sample_rate);
            // The channel is remade for the new rate. The pins were moved
            // into the first driver; the matrix keeps their routing, and
            // the conjured peripheral and pins name what this sink owns.
            // SAFETY: this sink is the only owner of I2S0 and of these pins.
            let (i2s0, bclk, dout, ws) = unsafe {
                (
                    I2S0::steal(),
                    esp_idf_svc::hal::gpio::Gpio24::steal(),
                    esp_idf_svc::hal::gpio::Gpio26::steal(),
                    esp_idf_svc::hal::gpio::Gpio25::steal(),
                )
            };
            // The old channel goes before the new one takes the peripheral.
            drop(self.i2s.take());
            match open(i2s0, bclk, dout, ws, format.sample_rate) {
                Ok(i2s) => {
                    self.i2s = Some(i2s);
                    self.rate = format.sample_rate;
                }
                Err(e) => {
                    // No channel is no player; say so and stop.
                    panic!(
                        "i2s: could not reopen at {} Hz: {e}",
                        format.sample_rate
                    );
                }
            }
        }
        self.follower.rate_changed(self.rate, 16);
        info!(
            "sink: {} {} Hz {} ch {} bit",
            format.codec, format.sample_rate, format.channels, format.bits
        );
    }

    fn write(&mut self, interleaved: &[i16]) -> usize {
        if !self.carry.is_empty() {
            let carry = std::mem::take(&mut self.carry);
            let n = self.offer(&carry);
            if n < carry.len() {
                self.carry.extend_from_slice(&carry[n..]);
                return 0;
            }
        }
        let ch = self.channels;
        let frames = interleaved.len() / ch;
        if frames == 0 {
            return 0;
        }
        self.scratch.clear();
        self.scratch.reserve(frames * FRAME_BYTES);
        for f in interleaved[..frames * ch].chunks_exact(ch) {
            let l = f[0].to_le_bytes();
            let r = if ch >= 2 { f[1].to_le_bytes() } else { l };
            self.scratch.extend_from_slice(&l);
            self.scratch.extend_from_slice(&r);
        }
        let scratch = std::mem::take(&mut self.scratch);
        let n = self.offer(&scratch);
        let whole = n / FRAME_BYTES;
        let part = n % FRAME_BYTES;
        if part != 0 {
            self.carry.extend_from_slice(
                &scratch[whole * FRAME_BYTES + part..(whole + 1) * FRAME_BYTES],
            );
        }
        self.scratch = scratch;
        let took = whole + usize::from(part != 0);
        self.taken += took as u64;
        took
    }

    fn flush(&mut self) {
        // What the DMA already holds (up to BUFFERS of audio) still plays;
        // dropping it means a channel restart, which is not worth the click
        // for a seek. The part-frame is dropped.
        self.carry.clear();
    }

    fn stop(&mut self) {
        // auto_clear sends silence once the DMA runs dry.
        self.carry.clear();
    }
}

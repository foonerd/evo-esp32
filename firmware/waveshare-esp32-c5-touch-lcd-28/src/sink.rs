// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The board's frame sink: I2S0 with its DMA as the buffer.
//!
//! The decoder offers interleaved 16-bit frames; the sink hands the DMA
//! what it has room for with a zero timeout and says how many frames
//! that was, so the I2S clock paces playback and a tick never waits on
//! it. A frame cut by the DMA's boundary is carried whole into the next
//! call, and a mono stream is doubled into both slots on the way, each
//! sample scaled by the shared [`Volume`] (the DACs sit at their 100%;
//! the speaker and a HAT share the stream, so one level serves both). The
//! ring is sized in time, a quarter second at the stream's rate, so a
//! slow packet's decode is covered at any rate the chip can afford.
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
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;

use esp_idf_svc::hal::i2s::{I2sDriver, I2sTx, I2S0};
use esp_idf_svc::sys;
use evo_esp32_sdk::contract::audio::playback::{Format, FrameSink};
use log::{info, warn};

/// Frames per DMA buffer: the most the driver allows at four bytes a
/// frame. The buffer count follows the rate so the ring holds [`LEAD_MS`]
/// of audio, within [`MIN_BUFFERS`]..=[`MAX_BUFFERS`]. DMA memory is
/// internal SRAM, 4 KB a buffer, and 64 KB of it is what the rest of the
/// firmware can spare: at 96 kHz that is 160 ms of lead, three times
/// the longest packet decode seen, and above that the rate is already
/// more than the chip decodes in time.
const FRAMES_PER_BUFFER: u32 = 1023;
const LEAD_MS: u32 = 250;
const MIN_BUFFERS: u32 = 6;
const MAX_BUFFERS: u32 = 16;
/// Bytes per stereo 16-bit frame on the wire.
const FRAME_BYTES: usize = 4;

/// Times the DMA certainly ran dry since the last report, and the worst
/// starvation in milliseconds; read and cleared by [`report_dry`].
static DRY_COUNT: AtomicU32 = AtomicU32::new(0);
static DRY_WORST_MS: AtomicU32 = AtomicU32::new(0);
/// Set by [`expect_gap`]: the writer is going quiet on purpose, so the
/// gap before its next write is not a dry spell.
static EXPECTED_GAP: AtomicBool = AtomicBool::new(false);

/// How many DMA buffers hold [`LEAD_MS`] at `rate`.
fn buffers_for(rate: u32) -> u32 {
    let frames = u64::from(rate) * u64::from(LEAD_MS) / 1000;
    let n = frames.div_ceil(u64::from(FRAMES_PER_BUFFER)) as u32;
    n.clamp(MIN_BUFFERS, MAX_BUFFERS)
}

/// The lead a ring of `buffers` holds at `rate`, in milliseconds. The
/// driver hands the writer each buffer as it finishes, so the ring plays
/// one less than it has while the writer is away.
fn lead_ms(rate: u32, buffers: u32) -> u32 {
    (u64::from(buffers - 1) * u64::from(FRAMES_PER_BUFFER) * 1000
        / u64::from(rate.max(1))) as u32
}

/// Unity on the Q15 gain scale.
const UNITY: u16 = 32767;

/// The volume the sink applies to every frame, shared with whoever owns
/// the `output` subject: the one level both the speaker and a HAT hear,
/// since they share the stream. The DACs themselves sit at their 100%.
#[derive(Clone, Debug)]
pub struct Volume(Arc<AtomicU16>);

impl Volume {
    /// A level on the house curve: 0 is silence, 1..=100 run from
    /// -49.5 dB to 0 dB in half-decibel steps; `muted` is silence too.
    pub fn set(&self, percent: u8, muted: bool) {
        let gain = if muted || percent == 0 {
            0
        } else {
            let half_db = f32::from(percent.min(100)) - 100.0;
            (f32::from(UNITY) * 10f32.powf(half_db / 40.0)).round() as u16
        };
        self.0.store(gain, Ordering::Relaxed);
    }

    fn gain(&self) -> u16 {
        self.0.load(Ordering::Relaxed)
    }
}

/// Dry-outs since the last call, as a line for the log; `None` if none.
pub fn report_dry() -> Option<String> {
    let count = DRY_COUNT.swap(0, Ordering::Relaxed);
    let worst = DRY_WORST_MS.swap(0, Ordering::Relaxed);
    (count > 0).then(|| {
        format!("the DMA ran dry {count} times, the worst by {worst} ms")
    })
}

/// The writer is about to go quiet on purpose (a pause, a start from
/// idle): the gap before its next write is not counted as a dry spell.
pub fn expect_gap() {
    EXPECTED_GAP.store(true, Ordering::Relaxed);
}

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
    volume: Volume,
    /// What the ring holds when full, and when the DMA last took bytes:
    /// a longer gap than the lead means it played silence meanwhile.
    lead_ms: u32,
    last_taken: Option<Instant>,
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
        let (i2s, lead) = open(i2s0, bclk, dout, ws, rate)?;
        Ok(Self {
            i2s: Some(i2s),
            rate,
            channels: 2,
            carry: Vec::with_capacity(FRAME_BYTES),
            scratch: Vec::new(),
            follower,
            taken: 0,
            volume: Volume(Arc::new(AtomicU16::new(UNITY))),
            lead_ms: lead,
            last_taken: None,
        })
    }

    /// The volume this sink applies, to hand to the output's owner.
    pub fn volume(&self) -> Volume {
        self.volume.clone()
    }

    fn driver(&mut self) -> &mut I2sDriver<'static, I2sTx> {
        self.i2s.as_mut().expect("the I2S channel is open")
    }

    /// Write `bytes` with no waiting; what the DMA took. A full DMA is
    /// a timeout with nothing taken, which is the contract's "ask again".
    fn offer(&mut self, bytes: &[u8]) -> usize {
        let n = match self.driver().write(bytes, 0) {
            Ok(n) => n,
            Err(e) if e.code() == sys::ESP_ERR_TIMEOUT => 0,
            Err(e) => {
                warn!("i2s: write: {e}");
                0
            }
        };
        if n > 0 {
            let now = Instant::now();
            if EXPECTED_GAP.swap(false, Ordering::Relaxed) {
                // Quiet on purpose; measure from here.
            } else if let Some(last) = self.last_taken {
                let gap = now.duration_since(last).as_millis() as u32;
                if gap > self.lead_ms {
                    let dry = gap - self.lead_ms;
                    info!("sink: the DMA ran dry for {dry} ms");
                    DRY_COUNT.fetch_add(1, Ordering::Relaxed);
                    DRY_WORST_MS.fetch_max(dry, Ordering::Relaxed);
                }
            }
            self.last_taken = Some(now);
        }
        n
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
) -> Result<(I2sDriver<'static, I2sTx>, u32), sys::EspError> {
    let buffers = buffers_for(rate);
    let lead = lead_ms(rate, buffers);
    info!("i2s: {rate} Hz, {buffers} DMA buffers of {FRAMES_PER_BUFFER} frames, {lead} ms of lead");
    let config = StdConfig::new(
        ChannelConfig::default()
            .auto_clear(true)
            .dma_buffer_count(buffers)
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
    Ok((i2s, lead))
}

impl<F: RateFollower> FrameSink for I2sSink<F> {
    fn start(&mut self, format: &Format) {
        self.channels = usize::from(format.channels).max(1);
        self.carry.clear();
        self.taken = 0;
        self.last_taken = None;
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
                Ok((i2s, lead)) => {
                    self.i2s = Some(i2s);
                    self.rate = format.sample_rate;
                    self.lead_ms = lead;
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
        let gain = i32::from(self.volume.gain());
        let scale = |s: i16| -> [u8; 2] {
            if gain == i32::from(UNITY) {
                s.to_le_bytes()
            } else {
                let scaled = ((i32::from(s) * gain) + (1 << 14)) >> 15;
                (scaled as i16).to_le_bytes()
            }
        };
        for f in interleaved[..frames * ch].chunks_exact(ch) {
            let l = scale(f[0]);
            let r = if ch >= 2 { scale(f[1]) } else { l };
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

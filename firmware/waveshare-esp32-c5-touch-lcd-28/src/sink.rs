// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The board's frame sink: a reservoir of decoded audio in PSRAM, fed to
//! I2S0's DMA by a task above everything else on the chip.
//!
//! The decoder offers interleaved 16-bit frames; the sink takes what the
//! reservoir has room for, with no waiting, and says how many frames
//! that was, so the decoder runs ahead of the ear by up to [`LEAD_MS`]
//! and a tick never waits. A feeder task takes the reservoir's bytes a
//! chunk at a time, scales each sample by the shared [`Volume`] (the
//! DACs sit at their 100%; the speaker and a HAT share the stream, so
//! one level serves both), and blocks on the DMA, which is short: a
//! stall of the whole chip for the length of a flash erase is what it
//! must cover, not a decode. The reservoir covers the rest: a packet
//! that decodes slowly, a full repaint of the face, the Wi-Fi driver's
//! key exchange at a join, a TLS handshake, anything that holds the
//! core for hundreds of milliseconds. The feeder runs at the highest
//! priority there is, above the Wi-Fi driver, so nothing holds it; its
//! work is a copy and a multiply.
//!
//! A mono stream is doubled into both slots on the way in. A pause stops
//! the feeder, and the DMA plays silence until the resume; what is held
//! waits. A seek drops what is held. A stream at another rate waits for
//! the reservoir and the DMA to drain, then has the channel remade
//! (`new_std_tx` again over a conjured `I2S0`) and the codec told; the
//! header mirror survives, because the GPIO matrix routes signals, not
//! channels.

use std::cell::UnsafeCell;
use std::sync::atomic::{
    AtomicBool, AtomicU16, AtomicU32, AtomicUsize, Ordering,
};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use esp_idf_svc::hal::gpio::AnyIOPin;
use esp_idf_svc::hal::i2s::config::{
    Config as ChannelConfig, DataBitWidth, SlotMode, StdClkConfig, StdConfig,
    StdGpioConfig, StdSlotConfig,
};
use esp_idf_svc::hal::i2s::{I2sDriver, I2sTx, I2S0};
use esp_idf_svc::hal::task::thread::ThreadSpawnConfiguration;
use esp_idf_svc::sys;
use evo_esp32_sdk::contract::audio::playback::{Format, FrameSink};
use log::{info, warn};

/// How far ahead of the ear the decoder may run: what the reservoir
/// holds when full, at the stream's rate.
const LEAD_MS: u32 = 600;
/// The highest rate the chip decodes in time; the reservoir is sized
/// for it once and used up to [`LEAD_MS`] at the rate in hand.
const CEILING_HZ: u32 = 96_000;
/// Frames per DMA buffer, the most the driver allows at four bytes a
/// frame, and the buffer count: the DMA is in internal SRAM and only
/// has to outlast a stall of the chip (a flash erase is split into
/// 20 ms pieces), since the feeder refills it within microseconds.
const FRAMES_PER_BUFFER: u32 = 1023;
const DMA_MS: u32 = 80;
const MIN_BUFFERS: u32 = 4;
const MAX_BUFFERS: u32 = 8;
/// Bytes per stereo 16-bit frame on the wire.
const FRAME_BYTES: usize = 4;
/// What the feeder takes from the reservoir at a time: 4 KB is one DMA
/// buffer, and sits in internal SRAM by the allocator's rule.
const CHUNK: usize = 4096;
/// The feeder's stack, internal: small, and it never touches flash.
const FEEDER_STACK: usize = 4 * 1024;
/// The feeder's priority: above the Wi-Fi driver's 23, the top the
/// kernel allows.
const FEEDER_PRIORITY: u8 = 24;
/// The feeder's wait when there is nothing to feed.
const EMPTY_NAP: Duration = Duration::from_millis(2);
/// What a stream starting into an empty reservoir gathers before the
/// feeder begins: the loop paints the new track's face between the
/// first packets, and at 96 kHz the decoder gains lead slowly. The
/// first stream after boot gathers the whole lead, since no one is
/// waiting for it and the first frame is the longest paint; a stream
/// following another's tail starts at once, the tail standing in.
const PRIME_MS: u32 = 250;

/// Times the reservoir certainly ran dry since the last report, and the
/// worst starvation in milliseconds; read and cleared by [`report_dry`].
static DRY_COUNT: AtomicU32 = AtomicU32::new(0);
static DRY_WORST_MS: AtomicU32 = AtomicU32::new(0);

/// How many DMA buffers hold [`DMA_MS`] at `rate`.
fn buffers_for(rate: u32) -> u32 {
    let frames = u64::from(rate) * u64::from(DMA_MS) / 1000;
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

/// What the sink needs to say to the codec when the rate changes.
pub trait RateFollower {
    /// The stream's rate and width from here on.
    fn rate_changed(&mut self, rate: u32, bits: u8);
}

/// The reservoir: a ring of bytes in PSRAM between one writer (the
/// decoder, on the main task) and one reader (the feeder). The counters
/// are byte positions that wrap; the ring's usable length is set while
/// it is empty, for the rate in hand.
struct Reservoir {
    /// The bytes; the writer and the reader touch disjoint parts.
    buf: UnsafeCell<Box<[u8]>>,
    /// The bytes allocated.
    capacity: usize,
    /// Bytes usable, at most `capacity`; a whole number of frames.
    len: AtomicUsize,
    /// Bytes written and read since the start, wrapping.
    written: AtomicUsize,
    read: AtomicUsize,
    /// The reader is to drop everything it holds, then clear this.
    flush: AtomicBool,
    /// The reader is to let the DMA run silent.
    paused: AtomicBool,
    /// The writer has written since the last start, pause or stop, so
    /// an empty reservoir is starvation and not a quiet on purpose.
    armed: AtomicBool,
    /// What the DMA ring holds, milliseconds: an empty reservoir for
    /// less than this is not heard.
    dma_lead_ms: AtomicU32,
    /// Bytes the reservoir gathers before the feeder takes any; zero
    /// once met.
    prime: AtomicUsize,
    /// The stream's rate, to say the lead in time.
    rate: AtomicU32,
    /// A stream has started since boot.
    started: AtomicBool,
}

// SAFETY: one writer and one reader, each touching only the bytes the
// counters give it; the counters are atomics with release/acquire pairs.
unsafe impl Sync for Reservoir {}

impl Reservoir {
    fn new(bytes: usize) -> Self {
        Self {
            buf: UnsafeCell::new(vec![0u8; bytes].into_boxed_slice()),
            capacity: bytes,
            len: AtomicUsize::new(bytes),
            written: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
            flush: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            armed: AtomicBool::new(false),
            dma_lead_ms: AtomicU32::new(0),
            prime: AtomicUsize::new(0),
            rate: AtomicU32::new(0),
            started: AtomicBool::new(false),
        }
    }

    fn readable(&self) -> usize {
        self.written
            .load(Ordering::Acquire)
            .wrapping_sub(self.read.load(Ordering::Acquire))
    }

    fn writable(&self) -> usize {
        self.len.load(Ordering::Relaxed) - self.readable()
    }

    /// Append as much of `src` as fits; the writer's side.
    fn push(&self, src: &[u8]) -> usize {
        if self.flush.load(Ordering::Acquire) {
            // The reader has not yet dropped what the flush asked; new
            // bytes now would go with it.
            return 0;
        }
        let len = self.len.load(Ordering::Relaxed);
        let n = src.len().min(self.writable());
        let at = self.written.load(Ordering::Relaxed) % len;
        let first = n.min(len - at);
        // SAFETY: the writer alone writes the bytes between `written` and
        // `read + len`, which these are; the reader reads the others.
        unsafe {
            let base = (*self.buf.get()).as_mut_ptr();
            core::ptr::copy_nonoverlapping(src.as_ptr(), base.add(at), first);
            core::ptr::copy_nonoverlapping(
                src.as_ptr().add(first),
                base,
                n - first,
            );
        }
        self.written.fetch_add(n, Ordering::Release);
        n
    }

    /// Take as much as `dst` holds; the reader's side.
    fn pop(&self, dst: &mut [u8]) -> usize {
        let len = self.len.load(Ordering::Relaxed);
        let n = dst.len().min(self.readable());
        let at = self.read.load(Ordering::Relaxed) % len;
        let first = n.min(len - at);
        // SAFETY: the reader alone reads the bytes between `read` and
        // `written`, which these are; the writer writes the others.
        unsafe {
            let base = (*self.buf.get()).as_ptr();
            core::ptr::copy_nonoverlapping(
                base.add(at),
                dst.as_mut_ptr(),
                first,
            );
            core::ptr::copy_nonoverlapping(
                base,
                dst.as_mut_ptr().add(first),
                n - first,
            );
        }
        self.read.fetch_add(n, Ordering::Release);
        n
    }

    /// The reader drops everything held, as the writer asked.
    fn drop_all(&self) {
        let written = self.written.load(Ordering::Acquire);
        self.read.store(written, Ordering::Release);
        self.flush.store(false, Ordering::Release);
    }
}

/// How far ahead of the ear the decoder is, for the loop that also
/// paints: when the lead is short, the audio comes first.
#[derive(Clone)]
pub struct Lead(Arc<Reservoir>);

impl Lead {
    /// Milliseconds of audio held while a stream is being written;
    /// `None` when nothing is streaming (idle, paused, between tracks).
    pub fn ms(&self) -> Option<u32> {
        if !self.0.armed.load(Ordering::Relaxed) {
            return None;
        }
        let rate = self.0.rate.load(Ordering::Relaxed).max(1) as usize;
        Some((self.0.readable() / FRAME_BYTES * 1000 / rate) as u32)
    }
}

/// What the main task asks of the feeder, besides feeding.
enum Command {
    /// Remake the channel at this rate; the reservoir is empty and the
    /// DMA has run out.
    Reopen(u32),
}

/// I2S0 as a [`FrameSink`].
pub struct I2sSink<F> {
    reservoir: Arc<Reservoir>,
    commands: Sender<Command>,
    /// Counts the channels the feeder has opened; the main task waits
    /// for it to move after a `Reopen`.
    opened: Arc<AtomicU32>,
    rate: u32,
    channels: usize,
    /// Bytes of a frame the reservoir took in part; go out first next time.
    carry: Vec<u8>,
    scratch: Vec<u8>,
    follower: F,
    /// Frames taken since `start`, for the log.
    pub taken: u64,
    volume: Volume,
    /// What the DMA ring holds when full, at the rate in hand.
    dma_lead_ms: u32,
}

impl<F: RateFollower> I2sSink<F> {
    /// The channel at `rate`, master, Philips, 16-bit stereo, on the
    /// board's codec pins, and the feeder behind it; `follower` is told
    /// each rate the stream takes.
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
        let (i2s, dma_lead) = open(i2s0, bclk, dout, ws, rate)?;
        let bytes = reservoir_bytes(CEILING_HZ);
        let reservoir = Arc::new(Reservoir::new(bytes));
        reservoir
            .len
            .store(reservoir_bytes(rate).min(bytes), Ordering::Relaxed);
        reservoir.dma_lead_ms.store(dma_lead, Ordering::Relaxed);
        reservoir.rate.store(rate, Ordering::Relaxed);
        info!(
            "sink: reservoir of {} KB in PSRAM, {LEAD_MS} ms of lead",
            bytes >> 10
        );
        let volume = Volume(Arc::new(AtomicU16::new(UNITY)));
        let (commands, inbox) = channel::<Command>();
        let opened = Arc::new(AtomicU32::new(1));
        fn fail<E>(_: E) -> sys::EspError {
            sys::EspError::from_infallible::<{ sys::ESP_FAIL }>()
        }
        {
            let reservoir = reservoir.clone();
            let volume = volume.clone();
            let opened = opened.clone();
            // The caller's configuration comes back after: the main task
            // gives every thread spawned from it PSRAM stacks, and the
            // plugins' threads spawn from it later.
            let before = ThreadSpawnConfiguration::get();
            ThreadSpawnConfiguration {
                name: Some(c"feeder"),
                stack_size: FEEDER_STACK,
                priority: FEEDER_PRIORITY,
                ..Default::default()
            }
            .set()
            .map_err(fail)?;
            let spawned = thread::Builder::new()
                .name("feeder".into())
                .stack_size(FEEDER_STACK)
                .spawn(move || feed(i2s, &reservoir, &volume, &inbox, &opened));
            before.unwrap_or_default().set().map_err(fail)?;
            spawned.map_err(fail)?;
        }
        Ok(Self {
            reservoir,
            commands,
            opened,
            rate,
            channels: 2,
            carry: Vec::with_capacity(FRAME_BYTES),
            scratch: Vec::new(),
            follower,
            taken: 0,
            volume,
            dma_lead_ms: dma_lead,
        })
    }

    /// The volume this sink applies, to hand to the output's owner.
    pub fn volume(&self) -> Volume {
        self.volume.clone()
    }

    /// The lead the decoder has, for the loop to read.
    pub fn lead(&self) -> Lead {
        Lead(self.reservoir.clone())
    }

    /// Wait until the reservoir and the DMA have played out; bounded.
    fn drain(&self) {
        let deadline = Instant::now()
            + Duration::from_millis(u64::from(LEAD_MS) * 2 + 500);
        while self.reservoir.readable() > 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(2));
        }
        thread::sleep(Duration::from_millis(u64::from(self.dma_lead_ms) + 5));
    }
}

/// The reservoir's size in bytes for [`LEAD_MS`] at `rate`, whole frames.
fn reservoir_bytes(rate: u32) -> usize {
    (rate as usize * LEAD_MS as usize / 1000) * FRAME_BYTES
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

/// The channel remade at `rate` over the conjured peripheral and pins:
/// the matrix keeps their routing from the first open.
fn reopen(
    rate: u32,
) -> Result<(I2sDriver<'static, I2sTx>, u32), sys::EspError> {
    // SAFETY: the feeder is the only owner of I2S0 and of these pins, and
    // the previous driver is dropped before this is called.
    let (i2s0, bclk, dout, ws) = unsafe {
        (
            I2S0::steal(),
            esp_idf_svc::hal::gpio::Gpio24::steal(),
            esp_idf_svc::hal::gpio::Gpio26::steal(),
            esp_idf_svc::hal::gpio::Gpio25::steal(),
        )
    };
    open(i2s0, bclk, dout, ws, rate)
}

/// The feeder: the reservoir's bytes, scaled, into the DMA, for as long
/// as the sink lives.
fn feed(
    mut i2s: I2sDriver<'static, I2sTx>,
    reservoir: &Reservoir,
    volume: &Volume,
    commands: &Receiver<Command>,
    opened: &AtomicU32,
) {
    let mut chunk = vec![0u8; CHUNK];
    let mut dry_since: Option<Instant> = None;
    loop {
        match commands.try_recv() {
            Ok(Command::Reopen(rate)) => {
                drop(i2s);
                match reopen(rate) {
                    Ok((new, _)) => i2s = new,
                    // No channel is no player; say so and stop.
                    Err(e) => panic!("i2s: could not reopen at {rate} Hz: {e}"),
                }
                opened.fetch_add(1, Ordering::Release);
                continue;
            }
            Err(TryRecvError::Disconnected) => return,
            Err(TryRecvError::Empty) => {}
        }
        if reservoir.flush.load(Ordering::Acquire) {
            reservoir.drop_all();
            dry_since = None;
        }
        if reservoir.paused.load(Ordering::Relaxed) {
            dry_since = None;
            thread::sleep(EMPTY_NAP);
            continue;
        }
        let prime = reservoir.prime.load(Ordering::Relaxed);
        if prime > 0 {
            if reservoir.readable() < prime {
                thread::sleep(EMPTY_NAP);
                continue;
            }
            reservoir.prime.store(0, Ordering::Relaxed);
        }
        let n = reservoir.pop(&mut chunk);
        if n == 0 {
            if reservoir.armed.load(Ordering::Relaxed) && dry_since.is_none() {
                dry_since = Some(Instant::now());
            }
            thread::sleep(EMPTY_NAP);
            continue;
        }
        if let Some(since) = dry_since.take() {
            // Shorter than the DMA ring, the gap was not heard: the ring
            // played on while the reservoir refilled.
            let empty = since.elapsed().as_millis() as u32;
            let lead = reservoir.dma_lead_ms.load(Ordering::Relaxed);
            if empty > lead {
                let dry = empty - lead;
                info!("sink: the DMA ran dry for {dry} ms");
                DRY_COUNT.fetch_add(1, Ordering::Relaxed);
                DRY_WORST_MS.fetch_max(dry, Ordering::Relaxed);
            }
        }
        scale(&mut chunk[..n], volume.gain());
        let mut offset = 0;
        while offset < n {
            match i2s.write(&chunk[offset..n], 200) {
                Ok(written) => offset += written,
                Err(e) if e.code() == sys::ESP_ERR_TIMEOUT => {}
                Err(e) => {
                    warn!("i2s: write: {e}");
                    break;
                }
            }
        }
    }
}

/// Scale 16-bit little-endian samples in place by a Q15 gain.
fn scale(bytes: &mut [u8], gain: u16) {
    if gain == UNITY {
        return;
    }
    let gain = i32::from(gain);
    for s in bytes.as_chunks_mut::<2>().0 {
        let v = i16::from_le_bytes(*s);
        let scaled = ((i32::from(v) * gain) + (1 << 14)) >> 15;
        *s = (scaled as i16).to_le_bytes();
    }
}

impl<F: RateFollower> FrameSink for I2sSink<F> {
    fn start(&mut self, format: &Format) {
        self.channels = usize::from(format.channels).max(1);
        self.carry.clear();
        self.taken = 0;
        self.reservoir.armed.store(false, Ordering::Relaxed);
        self.reservoir.paused.store(false, Ordering::Relaxed);
        if format.sample_rate != self.rate && format.sample_rate > 0 {
            info!("i2s: {} Hz -> {} Hz", self.rate, format.sample_rate);
            // The previous stream's tail plays out first, then the feeder
            // remakes the channel and the reservoir is resized, empty.
            self.drain();
            let before = self.opened.load(Ordering::Acquire);
            if self
                .commands
                .send(Command::Reopen(format.sample_rate))
                .is_ok()
            {
                let deadline = Instant::now() + Duration::from_secs(2);
                while self.opened.load(Ordering::Acquire) == before
                    && Instant::now() < deadline
                {
                    thread::sleep(Duration::from_millis(2));
                }
            }
            self.rate = format.sample_rate;
            self.reservoir.rate.store(self.rate, Ordering::Relaxed);
            self.dma_lead_ms = lead_ms(self.rate, buffers_for(self.rate));
            self.reservoir
                .dma_lead_ms
                .store(self.dma_lead_ms, Ordering::Relaxed);
            let bytes = reservoir_bytes(self.rate).min(self.reservoir.capacity);
            self.reservoir.len.store(bytes, Ordering::Relaxed);
        }
        self.follower.rate_changed(self.rate, 16);
        let prime_ms = if self.reservoir.started.swap(true, Ordering::Relaxed) {
            PRIME_MS
        } else {
            LEAD_MS
        };
        let prime =
            (self.rate as usize * prime_ms as usize / 1000) * FRAME_BYTES;
        self.reservoir.prime.store(prime, Ordering::Relaxed);
        info!(
            "sink: {} {} Hz {} ch {} bit",
            format.codec, format.sample_rate, format.channels, format.bits
        );
    }

    fn write(&mut self, interleaved: &[i16]) -> usize {
        if !self.carry.is_empty() {
            let carry = std::mem::take(&mut self.carry);
            let n = self.reservoir.push(&carry);
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
        // Only what fits is converted: the reservoir is the pace.
        let room = self.reservoir.writable() / FRAME_BYTES;
        let frames = frames.min(room);
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
        let n = self.reservoir.push(&self.scratch);
        let whole = n / FRAME_BYTES;
        let part = n % FRAME_BYTES;
        if part != 0 {
            self.carry.extend_from_slice(
                &self.scratch
                    [whole * FRAME_BYTES + part..(whole + 1) * FRAME_BYTES],
            );
        }
        let took = whole + usize::from(part != 0);
        if took > 0 {
            self.reservoir.armed.store(true, Ordering::Relaxed);
        }
        self.taken += took as u64;
        took
    }

    fn flush(&mut self) {
        // The feeder drops what the reservoir holds; the DMA's few tens of
        // milliseconds still play, which is not worth a channel restart.
        // No priming after a seek: the ear is waiting.
        self.carry.clear();
        self.reservoir.armed.store(false, Ordering::Relaxed);
        self.reservoir.prime.store(0, Ordering::Relaxed);
        self.reservoir.flush.store(true, Ordering::Release);
    }

    fn stop(&mut self) {
        // The tail plays out, however short; the DMA sends silence after.
        self.carry.clear();
        self.reservoir.armed.store(false, Ordering::Relaxed);
        self.reservoir.prime.store(0, Ordering::Relaxed);
    }

    fn pause(&mut self) {
        self.reservoir.armed.store(false, Ordering::Relaxed);
        self.reservoir.paused.store(true, Ordering::Relaxed);
    }

    fn resume(&mut self) {
        self.reservoir.paused.store(false, Ordering::Relaxed);
    }

    fn held(&self) -> usize {
        self.reservoir.readable() / FRAME_BYTES
    }
}

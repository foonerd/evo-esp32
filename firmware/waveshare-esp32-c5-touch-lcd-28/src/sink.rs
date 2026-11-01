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
//! the reservoir and the DMA to drain, then has the channel's clock set
//! again in place (`i2s_channel_reconfig_std_clock`, over the raw
//! driver: the HAL remakes the channel, which frees the DMA ring into
//! the heap and asks for a new one, and whatever the decoder allocated
//! in between took the ring's place in internal SRAM — 33 KB of it lost
//! at every change of rate, measured) and the codec told.

use std::cell::UnsafeCell;
use std::sync::atomic::{
    AtomicBool, AtomicU16, AtomicU32, AtomicU8, AtomicUsize, Ordering,
};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use esp_idf_svc::hal::gpio::{InputPin, OutputPin};
use esp_idf_svc::hal::i2s::I2S0;
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
/// Frames per DMA buffer and the buffer count: the DMA is in internal
/// SRAM and only has to outlast a stall of the chip (a flash erase is
/// split into 20 ms pieces), since the feeder refills it within
/// microseconds. The count is for [`DMA_MS`] at [`CEILING_HZ`] and the
/// same at every rate: a reopen at a higher rate then never asks
/// internal SRAM for more than the ring it just gave back, whatever
/// grew meanwhile. A receiver keeping time with other rooms asks the
/// sink what it holds, and the ring's content is known to a buffer
/// after a starvation (21 ms at 48 kHz); shorter buffers would tighten
/// that but cost the core at 96 kHz, where the decoder has none to
/// spare (measured: 480-frame buffers starve a 96 kHz FLAC every few
/// hundred milliseconds).
const FRAMES_PER_BUFFER: u32 = 1023;
const DMA_MS: u32 = 80;
const BUFFERS: u32 = (CEILING_HZ * DMA_MS / 1000).div_ceil(FRAMES_PER_BUFFER);
/// Bytes per stereo 16-bit frame on the wire.
const FRAME_BYTES: usize = 4;
/// What the feeder takes from the reservoir at a time: 4 KB, a few DMA
/// buffers' worth, in internal SRAM by the allocator's rule.
const CHUNK: usize = 4096;
/// The feeder's stack, internal: small, and it never touches flash.
/// (6 KB was tried for the codec drivers and took the last of the
/// DMA-capable SRAM from the card and the panel; the codecs are
/// written from the main task instead, in the window the feeder opens.)
const FEEDER_STACK: usize = 4 * 1024;
/// The feeder's wait when there is nothing to feed.
const EMPTY_NAP: Duration = Duration::from_millis(2);
/// What a stream starting into an empty reservoir gathers before the
/// feeder begins: the loop paints the new track's face between the
/// first packets, and at 96 kHz the decoder gains lead slowly. The
/// first stream after boot gathers the whole lead, since no one is
/// waiting for it and the first frame is the longest paint; a stream
/// following another's tail starts at once, the tail standing in. A
/// seek gathers the same: the frames the demuxer read on the way to
/// the target decode at once, and what follows them waits on the
/// source — a ring restarted on a share takes a few hundred
/// milliseconds to reach its low-water mark — so without the gather
/// the seek was heard as a burst, a gap and the music.
const PRIME_MS: u32 = 250;

/// Times the reservoir certainly ran dry since the last report, and the
/// worst starvation in milliseconds; read and cleared by [`report_dry`].
static DRY_COUNT: AtomicU32 = AtomicU32::new(0);
static DRY_WORST_MS: AtomicU32 = AtomicU32::new(0);

/// The lead a ring of `buffers` holds at `rate`, in milliseconds. The
/// driver hands the writer each buffer as it finishes, so the ring plays
/// one less than it has while the writer is away.
fn lead_ms(rate: u32, buffers: u32) -> u32 {
    (u64::from(buffers - 1) * u64::from(FRAMES_PER_BUFFER) * 1000
        / u64::from(rate.max(1))) as u32
}

/// Unity on the Q15 gain scale.
const UNITY: u16 = 32767;

/// The width of each I2S slot on the wire: 32 bits, so the bit clock is
/// 64 times the sample rate. The samples are 16-bit; the codec and a HAT
/// are told both.
pub const SLOT_BITS: u32 = 32;

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

/// How long the feeder holds the clocks stopped for the main task's
/// codec writes before going on without them.
const GATE_WAIT: Duration = Duration::from_millis(500);
/// How long the main task waits for the feeder to stop the clocks.
const GATE_OPEN_WAIT: Duration = Duration::from_secs(2);
/// The gate's states: closed; open (the clocks are stopped, the feeder
/// waits); done (the codecs are written, the feeder may go on).
const GATE_CLOSED: u8 = 0;
const GATE_OPEN: u8 = 1;
const GATE_DONE: u8 = 2;

/// What the sink needs to say to the codecs when the rate changes.
/// Called between the I2S clocks stopping and starting again: a codec
/// on the HAT header hears the bit clock as noise on its control lines
/// (a WM8960 took every write with the clocks stopped and none with
/// them running), so the codecs are written to in the gap.
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
    /// What the DMA ring holds, by account.
    dma: Mutex<DmaModel>,
    /// The clock-off window of a reopen, handed from the feeder to the
    /// main task for the codec writes (`GATE_*`).
    gate: AtomicU8,
}

/// The DMA ring's content, kept by account since the driver does not
/// say where the DMA is: frames handed to it, less frames it has
/// played, which run at the rate from an anchor. The anchor is the feed
/// that found the ring empty: what that feed added began to play then,
/// within a buffer. Between starvations the account is exact, the
/// DMA's clock and the chip's being one crystal.
#[derive(Debug, Clone, Copy)]
struct DmaModel {
    /// Frames handed to the DMA since the channel opened.
    fed: u64,
    /// Frames handed to the DMA before the anchor.
    anchor_fed: u64,
    /// When the frames after `anchor_fed` began to play.
    anchor: Instant,
}

impl DmaModel {
    fn new() -> Self {
        Self {
            fed: 0,
            anchor_fed: 0,
            anchor: Instant::now(),
        }
    }

    /// Frames the DMA still holds at `rate`.
    fn held(&self, rate: u32) -> usize {
        let played = self.anchor_fed
            + self.anchor.elapsed().as_micros() as u64 * u64::from(rate)
                / 1_000_000;
        (self.fed.saturating_sub(played) as usize)
            .min((BUFFERS * FRAMES_PER_BUFFER) as usize)
    }

    /// `frames` were just handed over, at `rate`.
    fn fed(&mut self, frames: usize, rate: u32) {
        if self.held(rate) == 0 {
            self.anchor_fed = self.fed;
            self.anchor = Instant::now();
        }
        self.fed += frames as u64;
    }
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
            dma: Mutex::new(DmaModel::new()),
            gate: AtomicU8::new(GATE_CLOSED),
        }
    }

    /// Frames the DMA ring holds, by account.
    fn in_dma(&self) -> usize {
        let rate = self.rate.load(Ordering::Relaxed);
        self.dma.lock().map_or(0, |m| m.held(rate))
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
    /// each rate the stream takes, while the clocks are stopped for the
    /// change.
    pub fn new(
        i2s0: I2S0<'static>,
        bclk: impl InputPin + OutputPin + 'static,
        dout: impl OutputPin + 'static,
        ws: impl InputPin + OutputPin + 'static,
        rate: u32,
        follower: F,
    ) -> Result<Self, sys::EspError> {
        // The peripheral and the pins are taken for good; the channel is
        // the raw driver's from here.
        let _ = i2s0;
        let (i2s, dma_lead) = Channel::open(
            i32::from(bclk.pin()),
            i32::from(dout.pin()),
            i32::from(ws.pin()),
            rate,
        )?;
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
                priority: crate::threads::FEEDER_PRIORITY,
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

    /// Reservoir bytes of `ms` at the current rate.
    fn prime_bytes(&self, ms: u32) -> usize {
        (self.rate as usize * ms as usize / 1000) * FRAME_BYTES
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

/// I2S0's transmit channel: master, Philips, 16-bit stereo, over the
/// raw driver, so that a change of rate is a change of clock and the
/// DMA ring allocated at the open is the one for the life of the player.
struct Channel(sys::i2s_chan_handle_t);

// The handle is used by the feeder thread alone once made.
unsafe impl Send for Channel {}

fn esp(code: sys::esp_err_t) -> Result<(), sys::EspError> {
    sys::EspError::convert(code)
}

impl Channel {
    /// The channel at `rate` on the pins given, enabled.
    fn open(
        bclk: i32,
        dout: i32,
        ws: i32,
        rate: u32,
    ) -> Result<(Self, u32), sys::EspError> {
        let lead = lead_ms(rate, BUFFERS);
        info!("i2s: {rate} Hz, 16-bit samples in {SLOT_BITS}-bit slots (bit clock {} fs), {BUFFERS} DMA buffers of {FRAMES_PER_BUFFER} frames, {lead} ms of lead", 2 * SLOT_BITS);
        let chan = sys::i2s_chan_config_t {
            id: sys::i2s_port_t_I2S_NUM_0,
            role: sys::i2s_role_t_I2S_ROLE_MASTER,
            dma_desc_num: BUFFERS,
            dma_frame_num: FRAMES_PER_BUFFER,
            __bindgen_anon_1: sys::i2s_chan_config_t__bindgen_ty_1 {
                auto_clear_after_cb: true,
            },
            auto_clear_before_cb: false,
            allow_pd: false,
            intr_priority: 0,
        };
        let mut handle: sys::i2s_chan_handle_t = core::ptr::null_mut();
        // SAFETY: the configuration structs are filled whole, as the HAL
        // fills them; the handle outlives every use (never deleted).
        unsafe {
            esp(sys::i2s_new_channel(
                &chan,
                &mut handle,
                core::ptr::null_mut(),
            ))?;
            let std = sys::i2s_std_config_t {
                clk_cfg: Self::clock(rate),
                // 16-bit samples in 32-bit slots: the bit clock is 64 fs,
                // the frame every Pi-HAT DAC takes (a Sabre in hardware
                // mode reads 32 fs as a wrong word and plays it loud);
                // the samples sit at the top of the slot, Philips-shifted.
                slot_cfg: sys::i2s_std_slot_config_t {
                    data_bit_width:
                        sys::i2s_data_bit_width_t_I2S_DATA_BIT_WIDTH_16BIT,
                    slot_bit_width:
                        sys::i2s_slot_bit_width_t_I2S_SLOT_BIT_WIDTH_32BIT,
                    slot_mode: sys::i2s_slot_mode_t_I2S_SLOT_MODE_STEREO,
                    slot_mask: 0b11,
                    ws_width: SLOT_BITS,
                    ws_pol: false,
                    bit_shift: true,
                    left_align: true,
                    big_endian: false,
                    bit_order_lsb: false,
                },
                gpio_cfg: sys::i2s_std_gpio_config_t {
                    mclk: sys::gpio_num_t_GPIO_NUM_NC,
                    bclk,
                    ws,
                    dout,
                    din: sys::gpio_num_t_GPIO_NUM_NC,
                    invert_flags: Default::default(),
                },
            };
            let made = esp(sys::i2s_channel_init_std_mode(handle, &std))
                .and_then(|()| esp(sys::i2s_channel_enable(handle)));
            if let Err(e) = made {
                sys::i2s_del_channel(handle);
                return Err(e);
            }
        }
        Ok((Self(handle), lead))
    }

    fn clock(rate: u32) -> sys::i2s_std_clk_config_t {
        sys::i2s_std_clk_config_t {
            sample_rate_hz: rate,
            clk_src: sys::soc_module_clk_t_SOC_MOD_CLK_PLL_F160M
                as sys::i2s_clock_src_t,
            ext_clk_freq_hz: 0,
            mclk_multiple: sys::i2s_mclk_multiple_t_I2S_MCLK_MULTIPLE_256,
            bclk_div: 8,
        }
    }

    /// The clock at `rate`, the ring kept; what the ring holds in time.
    /// `between` runs with the clocks stopped, before they start again.
    fn set_rate(
        &mut self,
        rate: u32,
        between: impl FnOnce(),
    ) -> Result<u32, sys::EspError> {
        // SAFETY: the channel is ours and enabled; disabled around the
        // change as the driver requires, then enabled again.
        unsafe {
            esp(sys::i2s_channel_disable(self.0))?;
            let set = esp(sys::i2s_channel_reconfig_std_clock(
                self.0,
                &Self::clock(rate),
            ));
            between();
            esp(sys::i2s_channel_enable(self.0))?;
            set?;
        }
        let lead = lead_ms(rate, BUFFERS);
        info!(
            "i2s: {rate} Hz, the same {BUFFERS} DMA buffers, {lead} ms of lead"
        );
        Ok(lead)
    }

    /// Write what the DMA takes within `timeout_ms`; how much it took.
    fn write(
        &mut self,
        bytes: &[u8],
        timeout_ms: u32,
    ) -> Result<usize, sys::EspError> {
        let mut written = 0usize;
        // SAFETY: a valid buffer and length; the driver copies out of it.
        let code = unsafe {
            sys::i2s_channel_write(
                self.0,
                bytes.as_ptr().cast(),
                bytes.len(),
                &mut written,
                timeout_ms,
            )
        };
        if code == sys::ESP_ERR_TIMEOUT && written > 0 {
            return Ok(written);
        }
        esp(code).map(|()| written)
    }
}

/// The feeder: the reservoir's bytes, scaled, into the DMA, for as long
/// as the sink lives. On a reopen it holds the clocks stopped while the
/// main task writes the codecs.
fn feed(
    mut i2s: Channel,
    reservoir: &Reservoir,
    volume: &Volume,
    commands: &Receiver<Command>,
    opened: &AtomicU32,
) {
    let mut chunk = vec![0u8; CHUNK];
    let mut dry_since: Option<Instant> = None;
    // The task watchdog watches the feeder too: no feeder, no sound.
    // SAFETY: a null handle names the calling task.
    if let Err(e) = sys::EspError::convert(unsafe {
        sys::esp_task_wdt_add(core::ptr::null_mut())
    }) {
        warn!("watchdog: the feeder could not subscribe: {e}");
    }
    loop {
        // SAFETY: resets this task's entry; harmless if it has none.
        unsafe { sys::esp_task_wdt_reset() };
        match commands.try_recv() {
            Ok(Command::Reopen(rate)) => {
                let gate = &reservoir.gate;
                let hold = || {
                    gate.store(GATE_OPEN, Ordering::Release);
                    let deadline = Instant::now() + GATE_WAIT;
                    while gate.load(Ordering::Acquire) != GATE_DONE
                        && Instant::now() < deadline
                    {
                        thread::sleep(Duration::from_millis(1));
                    }
                    gate.store(GATE_CLOSED, Ordering::Release);
                };
                // No channel is no player; say so and stop.
                if let Err(e) = i2s.set_rate(rate, hold) {
                    panic!("i2s: could not set {rate} Hz: {e}");
                }
                if let Ok(mut m) = reservoir.dma.lock() {
                    *m = DmaModel::new();
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
                Ok(written) => {
                    offset += written;
                    if let Ok(mut m) = reservoir.dma.lock() {
                        let rate = reservoir.rate.load(Ordering::Relaxed);
                        m.fed(written / FRAME_BYTES, rate);
                    }
                }
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
                // The feeder stops the clocks and opens the gate; the
                // codecs are written in that window, then it goes on.
                let gate = &self.reservoir.gate;
                let deadline = Instant::now() + GATE_OPEN_WAIT;
                while gate.load(Ordering::Acquire) != GATE_OPEN
                    && Instant::now() < deadline
                {
                    thread::sleep(Duration::from_millis(1));
                }
                if gate.load(Ordering::Acquire) != GATE_OPEN {
                    warn!("i2s: the feeder did not stop the clocks for the codecs; writing anyway");
                }
                self.follower.rate_changed(format.sample_rate, 16);
                gate.store(GATE_DONE, Ordering::Release);
                let deadline = Instant::now() + Duration::from_secs(2);
                while self.opened.load(Ordering::Acquire) == before
                    && Instant::now() < deadline
                {
                    thread::sleep(Duration::from_millis(2));
                }
            }
            self.rate = format.sample_rate;
            self.reservoir.rate.store(self.rate, Ordering::Relaxed);
            self.dma_lead_ms = lead_ms(self.rate, BUFFERS);
            self.reservoir
                .dma_lead_ms
                .store(self.dma_lead_ms, Ordering::Relaxed);
            let bytes = reservoir_bytes(self.rate).min(self.reservoir.capacity);
            self.reservoir.len.store(bytes, Ordering::Relaxed);
        }
        let prime_ms = if self.reservoir.started.swap(true, Ordering::Relaxed) {
            PRIME_MS
        } else {
            LEAD_MS
        };
        self.reservoir
            .prime
            .store(self.prime_bytes(prime_ms), Ordering::Relaxed);
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
        // Then it gathers a start's lead before it plays again.
        self.carry.clear();
        self.reservoir.armed.store(false, Ordering::Relaxed);
        self.reservoir
            .prime
            .store(self.prime_bytes(PRIME_MS), Ordering::Relaxed);
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
        self.reservoir.readable() / FRAME_BYTES + self.reservoir.in_dma()
    }
}

/// One sink held by two wardens: the decoder's and the multiroom
/// receiver's. Only one has custody at a time, and both run on the
/// loop's thread, so the lock is never contended; it is the type's
/// way of saying the sink is shared. Whoever `start`s it owns it.
pub struct Shared<F>(Arc<Mutex<I2sSink<F>>>);

impl<F> Shared<F> {
    /// Wrap `sink` for sharing.
    pub fn new(sink: I2sSink<F>) -> Self {
        Self(Arc::new(Mutex::new(sink)))
    }
}

impl<F> Clone for Shared<F> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<F: RateFollower> FrameSink for Shared<F> {
    fn start(&mut self, format: &Format) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .start(format);
    }
    fn write(&mut self, interleaved: &[i16]) -> usize {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write(interleaved)
    }
    fn flush(&mut self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).flush();
    }
    fn stop(&mut self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).stop();
    }
    fn pause(&mut self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).pause();
    }
    fn resume(&mut self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).resume();
    }
    fn held(&self) -> usize {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).held()
    }
}

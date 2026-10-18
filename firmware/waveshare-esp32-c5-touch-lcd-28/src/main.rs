// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! evo-esp32 on the Waveshare ESP32-C5-Touch-LCD-2.8.
//!
//! The first thing this firmware does is make a sound, because nothing
//! else about a player matters until it does. On boot:
//! 1. The I2C bus (SDA GPIO0, SCL GPIO1) shared by the IO expander and
//!    the codec, as the vendor BSP wires it.
//! 2. The CH32V003 expander up with its outputs high; EXIO3 is the
//!    NS4150B amplifier's enable.
//! 3. The ES8311 answers its chip ID, is opened, told the stream's
//!    rate and width, started, unmuted, set to a modest volume.
//! 4. I2S0 as master in Philips format, 16-bit stereo slots, on the
//!    BSP's pins: BCLK GPIO24, WS GPIO25, data to the codec GPIO26, no
//!    MCLK (the codec derives its own from the bit clock).
//! 5. The same three I2S signals mirrored through the GPIO matrix onto
//!    the header pins a Pi-HAT DAC can reach: BCLK on GPIO2 and WS on
//!    GPIO3 (J8 pins 7 and 8), data on GPIO11 (J6's TXD, free once the
//!    console is on USB). The speaker and the HAT play the same frames.
//!    If an ES9018K2M answers on the bus (a LightSABRE or an I-Sabre K2M
//!    HAT on J5's SDA/SCL), it is brought up and set to a modest volume;
//!    a HAT without one, a bare PCM5102A, needs nothing said to it.
//! 6. A 1 kHz tone for two seconds every ten, each one logged with how
//!    long the write took: paced by the I2S clock it takes two seconds,
//!    and that figure is the proof the clock is right before any
//!    decoder is asked for frames.
//!
//! Everything here is the device layer; the fabric arrives once this
//! is heard.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use ch32v003_expander::{Expander, Pin as Exio};
use embedded_hal_bus::i2c::RefCellDevice;
use es8311::Es8311;
use es9018k2m::Es9018k2m;
use esp_idf_svc::hal::gpio::{AnyIOPin, Pin};
use esp_idf_svc::hal::i2c::{I2cConfig, I2cDriver};
use esp_idf_svc::hal::i2s::config::{
    Config as I2sChannelConfig, DataBitWidth, SlotMode, StdClkConfig,
    StdConfig, StdGpioConfig, StdSlotConfig,
};
use esp_idf_svc::hal::i2s::I2sDriver;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::hal::units::FromValueType;
use esp_idf_svc::log::EspLogger;
use esp_idf_svc::sys;
use log::{info, warn};

/// The stream the tone is made in.
const RATE: u32 = 44_100;
/// Tone pitch and length, and the silence between tones.
const TONE_HZ: f32 = 1000.0;
const TONE: Duration = Duration::from_secs(2);
const REST: Duration = Duration::from_secs(8);
/// Tone amplitude as a fraction of full scale; the amplifier is small
/// and the speaker smaller.
const AMPLITUDE: f32 = 0.3;
/// Playback volume, percent of the codec's curve.
const VOLUME_PERCENT: u8 = 40;
/// The board's hardware gain the codec driver takes off, in half-dB
/// (the vendor's 5 V amplifier against the codec's 3.3 V output).
const HW_GAIN_HALF_DB: i16 = -7;

/// The expander lines this board drives.
const TOUCH_RST: Exio = Exio::P0;
const PANEL_RST: Exio = Exio::P1;
const AMP_EN: Exio = Exio::P3;

fn main() -> anyhow::Result<()> {
    sys::link_patches();
    EspLogger::initialize_default();
    info!(
        "{} {} starting",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION")
    );

    let Peripherals {
        i2c0, i2s0, pins, ..
    } = Peripherals::take()?;

    // The bus, leaked: it lives as long as the firmware, and both the
    // expander and the codec borrow it.
    let bus: &'static RefCell<I2cDriver<'static>> =
        Box::leak(Box::new(RefCell::new(I2cDriver::new(
            i2c0,
            pins.gpio0,
            pins.gpio1,
            &I2cConfig::new().baudrate(400.kHz().into()),
        )?)));

    let mut expander = Expander::new(RefCellDevice::new(bus));
    expander
        .init(TOUCH_RST.mask() | PANEL_RST.mask() | AMP_EN.mask())
        .map_err(|e| anyhow::anyhow!("expander init: {e:?}"))?;
    expander
        .write_outputs(TOUCH_RST.mask() | PANEL_RST.mask() | AMP_EN.mask())
        .map_err(|e| anyhow::anyhow!("expander outputs: {e:?}"))?;
    info!("expander: up, outputs high (amplifier enabled on EXIO3)");

    let mut codec = Es8311::new(RefCellDevice::new(bus));
    match codec.chip_id() {
        Ok(id) => {
            info!("es8311: chip id {:02x} {:02x} {:02x}", id[0], id[1], id[2])
        }
        Err(e) => warn!("es8311: no chip id: {e:?}"),
    }
    codec
        .open()
        .map_err(|e| anyhow::anyhow!("es8311 open: {e:?}"))?;
    codec
        .set_format(RATE, 16)
        .map_err(|e| anyhow::anyhow!("es8311 format: {e:?}"))?;
    codec
        .start()
        .map_err(|e| anyhow::anyhow!("es8311 start: {e:?}"))?;
    codec
        .set_mute(false)
        .map_err(|e| anyhow::anyhow!("es8311 unmute: {e:?}"))?;
    let half_db = es8311::percent_to_half_db(VOLUME_PERCENT, HW_GAIN_HALF_DB);
    codec
        .set_volume(half_db)
        .map_err(|e| anyhow::anyhow!("es8311 volume: {e:?}"))?;
    info!(
        "es8311: open, {RATE} Hz 16-bit, started, volume {VOLUME_PERCENT}% ({half_db} half-dB), bit clock {} Hz",
        es8311::bit_clock_for(RATE)
    );

    let config = StdConfig::new(
        I2sChannelConfig::default().auto_clear(true),
        StdClkConfig::from_sample_rate_hz(RATE),
        StdSlotConfig::philips_slot_default(
            DataBitWidth::Bits16,
            SlotMode::Stereo,
        ),
        StdGpioConfig::default(),
    );
    let mut i2s = I2sDriver::new_std_tx(
        i2s0,
        &config,
        pins.gpio24,
        pins.gpio26,
        None::<AnyIOPin>,
        pins.gpio25,
    )?;
    i2s.tx_enable()?;
    info!("i2s0: master, Philips 16-bit stereo, BCLK GPIO24, WS GPIO25, DOUT GPIO26");

    // The HAT's copy of the bus. The Pi header's I2S is BCLK pin 12, LRCLK
    // pin 35, DIN pin 40, 5 V pin 2, GND pin 6.
    mirror(pins.gpio2.pin(), sys::I2SO_BCK_OUT_IDX);
    mirror(pins.gpio3.pin(), sys::I2SO_WS_OUT_IDX);
    mirror(pins.gpio11.pin(), sys::I2SO_SD_OUT_IDX);
    info!("i2s0: mirrored for a HAT: BCLK GPIO2 (J8.7), WS GPIO3 (J8.8), DATA GPIO11 (J6 TXD)");

    let mut sabre = Es9018k2m::new(RefCellDevice::new(bus));
    match sabre.detect() {
        Ok(Some(id)) => {
            info!(
                "es9018k2m: found on the bus, status {:#04x}, revision {}",
                id.status, id.revision
            );
            match sabre.init().and_then(|()| sabre.set_volume(VOLUME_PERCENT)) {
                Ok(()) => {
                    info!("es9018k2m: initialised, volume {VOLUME_PERCENT}%")
                }
                Err(e) => warn!("es9018k2m: init failed: {e:?}"),
            }
        }
        Ok(None) => {
            info!("es9018k2m: something answers at 0x48 but is not one")
        }
        Err(_) => info!("es9018k2m: none on the bus"),
    }

    let tone = tone_bytes();
    let mut n = 0u32;
    loop {
        n += 1;
        let started = Instant::now();
        i2s.write_all(&tone, sys::TickType_t::MAX)?;
        let took = started.elapsed();
        info!(
            "tone {n}: {} bytes written in {} ms (expected {} ms)",
            tone.len(),
            took.as_millis(),
            TONE.as_millis()
        );
        std::thread::sleep(REST);
    }
}

/// Drive `gpio` from the I2S output `signal` as well as the pin the
/// driver was given; the GPIO matrix fans one signal out to any number
/// of pins.
fn mirror(gpio: u8, signal: u32) {
    // SAFETY: plain configuration calls on a pin this firmware owns and
    // nothing else drives.
    unsafe {
        sys::gpio_reset_pin(i32::from(gpio));
        sys::gpio_set_direction(
            i32::from(gpio),
            sys::gpio_mode_t_GPIO_MODE_OUTPUT,
        );
        sys::esp_rom_gpio_connect_out_signal(
            u32::from(gpio),
            signal,
            false,
            false,
        );
    }
}

/// `TONE` of `TONE_HZ` at `RATE`, 16-bit stereo interleaved, little-endian.
/// One exact cycle of the tone, tiled: 441 samples at 44.1 kHz is ten
/// periods of 1 kHz. The C5 has no FPU; 88 200 software sines took eight
/// seconds and tripped the idle task's watchdog on the first bench run.
fn tone_bytes() -> Vec<u8> {
    let frames = (RATE as u64 * TONE.as_millis() as u64 / 1000) as usize;
    let cycle = (RATE / 100) as usize;
    let step = core::f32::consts::TAU * TONE_HZ / RATE as f32;
    let table: Vec<[u8; 2]> = (0..cycle)
        .map(|i| {
            (((i as f32 * step).sin() * AMPLITUDE * i16::MAX as f32) as i16)
                .to_le_bytes()
        })
        .collect();
    let mut out = Vec::with_capacity(frames * 4);
    for i in 0..frames {
        let s = table[i % cycle];
        out.extend_from_slice(&s);
        out.extend_from_slice(&s);
    }
    out
}

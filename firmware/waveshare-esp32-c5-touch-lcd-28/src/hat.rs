// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The register of DAC boards for the header, and what the card's `dac`
//! and `output` lines do with it.
//!
//! A board that answers on the bus is found and brought up without
//! being named: a PCM5122 (at 0x4C or 0x4D), an ES9018K2M (0x48), a
//! WM8960 (it takes writes only; its reset is the probe). A board on
//! the three I2S lines alone — a PCM5102A module, an ES9038Q2M board in
//! hardware mode — has nothing to find and nothing to configure; the
//! card's `dac` line names it so the player can say what it has. Every
//! board hears the same stream at the sink's level: the slider is one
//! control, and the speaker and the HAT stay level with each other.
//!
//! `output` chooses between the board's speaker and the HAT: `hat`
//! switches the speaker's amplifier off and mutes its codec; `speaker`
//! mutes a HAT that has a mute (every one found on the bus); a board on
//! the lines alone has none and plays on, and the status says so.

use std::fmt::Write as _;

use es9018k2m::Es9018k2m;
use evo_esp32_config::Output;
use pcm512x::Pcm512x;

/// One kind of board the player knows.
pub struct Entry {
    /// The `dac` line's value.
    pub name: &'static str,
    /// The boards of this kind seen on the bench.
    pub boards: &'static str,
    /// The DAC or codec on it.
    pub chip: &'static str,
    /// How the player knows one is there.
    pub how: How,
    /// What the bring-up has to know.
    pub quirks: &'static str,
}

/// How a board is known to be on the header.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum How {
    /// It answers on the bus at this address, and is brought up.
    Bus(&'static str),
    /// Nothing answers; the card names it.
    Named,
}

/// Every board proven on the bench, by the name the `dac` line takes.
pub static REGISTER: [Entry; 5] = [
    Entry {
        name: "pcm5102a",
        boards: "GY-PCM5102 and CJMCU-5102 modules",
        chip: "TI PCM5102A",
        how: How::Named,
        quirks: "pins only: SCK to ground, FMT, FLT and DEMP low, XSMT high (GY-PCM5102: H1 L, H2 L, H3 H, H4 L); no mute",
    },
    Entry {
        name: "pcm5122",
        boards: "Waveshare PCM5122 Audio Board (A), Raspberry Pi DAC+ (IQaudIO), HiFiBerry DAC+, DAC+ Pro and DAC2 Pro, Allo Boss, InnoMaker DAC Mini",
        chip: "TI PCM5122",
        how: How::Bus("0x4C or 0x4D"),
        quirks: "PLL on BCK, 32-bit I2S; a board with its own oscillators (Boss class) has them switched off through the chip's GPIO3 and GPIO6; the Pi DAC+ needs 3.3 V on header pin 1 or 17 as well as 5 V",
    },
    Entry {
        name: "es9018k2m",
        boards: "Audiophonics I-Sabre K2M class, bare modules",
        chip: "ESS ES9018K2M",
        how: How::Bus("0x48"),
        quirks: "its own clock on the board",
    },
    Entry {
        name: "es9038q2m",
        boards: "InnoMaker HiFi DAC Pro in hardware mode",
        chip: "ESS ES9038Q2M",
        how: How::Named,
        quirks: "64 fs only (32 fs overdrives it); nothing on the bus to write; no mute",
    },
    Entry {
        name: "wm8960",
        boards: "Waveshare WM8960 Audio HAT",
        chip: "Cirrus WM8960",
        how: How::Bus("0x1A, writes only; the reset is the probe"),
        quirks: "PLL from its 24 MHz, 32-bit I2S; written with the clocks stopped; silent above 48 kHz; headphones and speakers at 0 dB",
    },
];

/// The entry named `name`, case aside.
pub fn entry(name: &str) -> Option<&'static Entry> {
    REGISTER
        .iter()
        .find(|e| e.name.eq_ignore_ascii_case(name.trim()))
}

/// `pcm5102a, pcm5122, …`
pub fn names() -> String {
    REGISTER
        .iter()
        .map(|e| e.name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The boards found on the bus and brought up, held so `output` can
/// mute them. The WM8960 lives with the sink's codec and is muted
/// there; here it is only counted.
#[derive(Default)]
pub struct Found {
    pub pcm512x: Option<Pcm512x<board::Device>>,
    pub es9018k2m: Option<Es9018k2m<board::Device>>,
    pub wm8960: bool,
}

impl Found {
    /// The register names of what the bus found.
    pub fn names(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.pcm512x.is_some() {
            v.push("pcm5122");
        }
        if self.es9018k2m.is_some() {
            v.push("es9018k2m");
        }
        if self.wm8960 {
            v.push("wm8960");
        }
        v
    }

    /// Mute or unmute every board found that has a mute (the WM8960
    /// is the sink's codec's to do).
    pub fn set_mute(&mut self, mute: bool) {
        if let Some(d) = self.pcm512x.as_mut() {
            if let Err(e) = d.set_mute(mute) {
                log::warn!("pcm512x: mute: {e:?}");
            }
        }
        if let Some(d) = self.es9018k2m.as_mut() {
            if let Err(e) = d.set_mute(mute) {
                log::warn!("es9018k2m: mute: {e:?}");
            }
        }
    }
}

/// What `dac` and `output` on the card come to, against what the bus
/// found: whether the HAT is to play, and the lines for the status
/// file. The speaker's side is the codec's and the amplifier's, settled
/// before the bus is surveyed.
pub struct Choice {
    pub hat: bool,
    pub notes: Vec<String>,
}

pub fn choose(
    found: &Found,
    dac: Option<&str>,
    output: Option<Output>,
) -> Choice {
    let mut notes = Vec::new();
    let on_bus = found.names();
    let mut hat_present = !on_bus.is_empty();
    let mut hat_mutes = hat_present;
    let mut line = String::new();
    if !on_bus.is_empty() {
        let _ = write!(line, "hat: on the bus: {}", on_bus.join(", "));
    }
    match dac.map(|d| (d, entry(d))) {
        None => {}
        Some((name, None)) => {
            notes.push(format!(
                "dac = {name}: not in the register ({}); read and set aside",
                names()
            ));
        }
        Some((_, Some(e))) => match e.how {
            How::Named => {
                hat_present = true;
                hat_mutes = false;
                let _ = write!(
                    line,
                    "{}dac: {} ({}; {}) on the lines alone: {}; the level is the sink's",
                    if line.is_empty() { "hat: " } else { "; " },
                    e.name,
                    e.chip,
                    e.boards,
                    e.quirks
                );
            }
            How::Bus(at) => {
                if !on_bus.contains(&e.name) {
                    notes.push(format!(
                        "dac = {}: none answers at {at}; the board is found by itself when it does",
                        e.name
                    ));
                }
            }
        },
    }
    if !line.is_empty() {
        notes.push(line);
    }
    let hat = match output.unwrap_or(Output::Both) {
        Output::Both => true,
        Output::Speaker => {
            if hat_present && !hat_mutes {
                notes.push(
                    "output = speaker: the named board has no mute and plays on"
                        .into(),
                );
            }
            false
        }
        Output::Hat => {
            if !hat_present {
                notes.push(
                    "output = hat: nothing on the header is known; the speaker is off all the same"
                        .into(),
                );
            }
            true
        }
    };
    if let Some(o) = output {
        notes.push(format!(
            "output = {}: speaker {}, hat {}",
            o.name(),
            if o == Output::Hat { "off" } else { "on" },
            if hat { "on" } else { "muted where it can be" }
        ));
    }
    Choice { hat, notes }
}

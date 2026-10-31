//! The Everest ES8311 audio codec on Waveshare's ESP32 touch panels.
//!
//! A mono codec (one DAC, one ADC) on the board's shared I2C bus at
//! `0x18`, fed over I2S. On the ESP32-C5-Touch-LCD-2.8 it drives the
//! speaker through an amplifier whose enable is the IO expander's EXIO3,
//! and its MCLK pin is not wired: the codec makes its master clock from
//! the I2S bit clock, which this driver configures it for.
//!
//! The register sequences are Espressif's `esp_codec_dev` component
//! ([espressif/esp-adf](https://github.com/espressif/esp-adf) at
//! `af7b72f`, `components/esp_codec_dev/device/es8311/`, Apache-2.0,
//! copyright Espressif Systems), the driver the vendor's BSP uses for
//! this board, in the configuration the BSP gives it: slave, no MCLK,
//! DAC mode, nothing inverted. Transcribed for that configuration, not
//! copied; the ADC path and master mode are not here.
//!
//! What a device does with it, in order: [`Es8311::open`] once after
//! power, [`Es8311::set_format`] for the stream's rate and width,
//! [`Es8311::start`], then the amplifier on and [`Es8311::set_mute`]
//! off; [`Es8311::set_volume`] at any time; [`Es8311::suspend`] when the
//! output is closed. The I2S side is the device's: 16-bit slots, two of
//! them, so the bit clock is 32 times the sample rate; at 8 kHz the codec
//! wants 32-bit slots instead, as the clock table below says.
//!
//! The driver is `no_std`, has no `unsafe`, and speaks only
//! `embedded_hal::i2c::I2c`.
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

use embedded_hal::i2c::I2c;

/// The 7-bit address with the CE pin low, as the board wires it.
pub const ADDRESS: u8 = 0x18;

/// MCLK as a multiple of the sample rate: the codec's clock table is
/// indexed by it, and without an MCLK pin the codec multiplies the bit
/// clock up to it, so the bit clock must be `MCLK_PER_FRAME / 8` times
/// the rate (32, for 16-bit stereo slots), or `MCLK_PER_FRAME / 4` at
/// 8 kHz.
pub const MCLK_PER_FRAME: u32 = 256;

/// The bit clock the codec expects for `rate`, in Hz, given how it
/// multiplies it up; what a device sets its I2S slots to produce.
pub const fn bit_clock_for(rate: u32) -> u32 {
    if rate == 8000 {
        rate * MCLK_PER_FRAME / 4
    } else {
        rate * MCLK_PER_FRAME / 8
    }
}

/// Why a call failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error<E> {
    /// The bus said no.
    Bus(E),
    /// No clock setting exists for the rate and width asked.
    Unsupported,
}

impl<E> From<E> for Error<E> {
    fn from(e: E) -> Self {
        Error::Bus(e)
    }
}

/// The registers this driver touches, by the reference's names.
#[allow(clippy::upper_case_acronyms)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Reg {
    Reset00 = 0x00,
    Clk01 = 0x01,
    Clk02 = 0x02,
    Clk03 = 0x03,
    Clk04 = 0x04,
    Clk05 = 0x05,
    Clk06 = 0x06,
    Clk07 = 0x07,
    Clk08 = 0x08,
    SdpIn09 = 0x09,
    SdpOut0A = 0x0A,
    Sys0B = 0x0B,
    Sys0C = 0x0C,
    Sys0D = 0x0D,
    Sys0E = 0x0E,
    Sys10 = 0x10,
    Sys11 = 0x11,
    Sys12 = 0x12,
    Sys13 = 0x13,
    Sys14 = 0x14,
    Adc15 = 0x15,
    Adc16 = 0x16,
    Adc17 = 0x17,
    Adc1B = 0x1B,
    Adc1C = 0x1C,
    Dac31 = 0x31,
    Dac32 = 0x32,
    Dac37 = 0x37,
    Gpio44 = 0x44,
    Gp45 = 0x45,
    ChipId1 = 0xFD,
    ChipId2 = 0xFE,
    ChipVersion = 0xFF,
}

/// One row of the codec's clock table, for MCLK = 256 x rate.
#[derive(Debug, Clone, Copy)]
struct Coeff {
    rate: u32,
    pre_div: u8,
    adc_div: u8,
    dac_div: u8,
    fs_mode: u8,
    lrck_h: u8,
    lrck_l: u8,
    bclk_div: u8,
    adc_osr: u8,
    dac_osr: u8,
}

/// The rows of the reference's table whose MCLK is 256 x rate; the
/// multiplier column is not here because without an MCLK pin the codec
/// is told x8 (x4 at 8 kHz) regardless of it.
const COEFFS: &[Coeff] = &[
    Coeff {
        rate: 8000,
        pre_div: 1,
        adc_div: 1,
        dac_div: 1,
        fs_mode: 0,
        lrck_h: 0x00,
        lrck_l: 0xff,
        bclk_div: 4,
        adc_osr: 0x10,
        dac_osr: 0x20,
    },
    Coeff {
        rate: 11025,
        pre_div: 1,
        adc_div: 1,
        dac_div: 1,
        fs_mode: 0,
        lrck_h: 0x00,
        lrck_l: 0xff,
        bclk_div: 4,
        adc_osr: 0x10,
        dac_osr: 0x20,
    },
    Coeff {
        rate: 12000,
        pre_div: 1,
        adc_div: 1,
        dac_div: 1,
        fs_mode: 0,
        lrck_h: 0x00,
        lrck_l: 0xff,
        bclk_div: 4,
        adc_osr: 0x10,
        dac_osr: 0x20,
    },
    Coeff {
        rate: 16000,
        pre_div: 1,
        adc_div: 1,
        dac_div: 1,
        fs_mode: 0,
        lrck_h: 0x00,
        lrck_l: 0xff,
        bclk_div: 4,
        adc_osr: 0x10,
        dac_osr: 0x20,
    },
    Coeff {
        rate: 22050,
        pre_div: 1,
        adc_div: 1,
        dac_div: 1,
        fs_mode: 0,
        lrck_h: 0x00,
        lrck_l: 0xff,
        bclk_div: 4,
        adc_osr: 0x10,
        dac_osr: 0x10,
    },
    Coeff {
        rate: 24000,
        pre_div: 1,
        adc_div: 1,
        dac_div: 1,
        fs_mode: 0,
        lrck_h: 0x00,
        lrck_l: 0xff,
        bclk_div: 4,
        adc_osr: 0x10,
        dac_osr: 0x10,
    },
    Coeff {
        rate: 32000,
        pre_div: 1,
        adc_div: 1,
        dac_div: 1,
        fs_mode: 0,
        lrck_h: 0x00,
        lrck_l: 0xff,
        bclk_div: 4,
        adc_osr: 0x10,
        dac_osr: 0x10,
    },
    Coeff {
        rate: 44100,
        pre_div: 1,
        adc_div: 1,
        dac_div: 1,
        fs_mode: 0,
        lrck_h: 0x00,
        lrck_l: 0xff,
        bclk_div: 4,
        adc_osr: 0x10,
        dac_osr: 0x10,
    },
    Coeff {
        rate: 48000,
        pre_div: 1,
        adc_div: 1,
        dac_div: 1,
        fs_mode: 0,
        lrck_h: 0x00,
        lrck_l: 0xff,
        bclk_div: 4,
        adc_osr: 0x10,
        dac_osr: 0x10,
    },
    Coeff {
        rate: 64000,
        pre_div: 1,
        adc_div: 1,
        dac_div: 1,
        fs_mode: 0,
        lrck_h: 0x00,
        lrck_l: 0xff,
        bclk_div: 4,
        adc_osr: 0x10,
        dac_osr: 0x10,
    },
    Coeff {
        rate: 96000,
        pre_div: 2,
        adc_div: 1,
        dac_div: 1,
        fs_mode: 0,
        lrck_h: 0x00,
        lrck_l: 0xff,
        bclk_div: 4,
        adc_osr: 0x10,
        dac_osr: 0x10,
    },
];

/// The DAC volume register's scale: half-decibels, `0x00` is -95.5 dB,
/// `0xBF` is 0 dB, `0xFF` is +32 dB.
const VOLUME_ZERO_DB: i16 = 0xBF;

/// The DAC volume register for a level in half-decibels at the DAC,
/// clamped to the register's range.
pub const fn volume_register(half_db: i16) -> u8 {
    let v = half_db + VOLUME_ZERO_DB;
    if v < 0 {
        0
    } else if v > 0xFF {
        0xFF
    } else {
        v as u8
    }
}

/// A 0 to 100 level as half-decibels at the DAC, on the reference's
/// curve: 0 is silence (-96 dB), 1 to 100 run from -49.5 dB to 0 dB in
/// half-decibel steps, and the board's hardware gain is taken off so
/// that 100 is the loudest the amplifier can take cleanly.
/// `hw_gain_half_db` is `20 log10(Vdac / Vpa)` plus any amplifier gain,
/// in half-decibels: -7 for a 3.3 V DAC into a 5 V amplifier.
pub const fn percent_to_half_db(percent: u8, hw_gain_half_db: i16) -> i16 {
    if percent == 0 {
        return -192;
    }
    let p = if percent > 100 { 100 } else { percent } as i16;
    (p - 100) - hw_gain_half_db
}

/// The codec, over a bus the caller provides.
#[derive(Debug)]
pub struct Es8311<I2C> {
    i2c: I2C,
    address: u8,
}

impl<I2C: I2c> Es8311<I2C> {
    /// The codec at its board address.
    pub fn new(i2c: I2C) -> Self {
        Self::at(i2c, ADDRESS)
    }

    /// The codec at another 7-bit address (CE pin high gives `0x19`).
    pub fn at(i2c: I2C, address: u8) -> Self {
        Self { i2c, address }
    }

    /// Give the bus back.
    pub fn release(self) -> I2C {
        self.i2c
    }

    fn write(&mut self, reg: Reg, value: u8) -> Result<(), I2C::Error> {
        self.i2c.write(self.address, &[reg as u8, value])
    }

    fn read(&mut self, reg: Reg) -> Result<u8, I2C::Error> {
        let mut v = [0u8];
        self.i2c.write_read(self.address, &[reg as u8], &mut v)?;
        Ok(v[0])
    }

    fn update(
        &mut self,
        reg: Reg,
        f: impl FnOnce(u8) -> u8,
    ) -> Result<(), I2C::Error> {
        let v = self.read(reg)?;
        self.write(reg, f(v))
    }

    /// The chip's two ID bytes and its version, as it reports them.
    pub fn chip_id(&mut self) -> Result<[u8; 3], I2C::Error> {
        Ok([
            self.read(Reg::ChipId1)?,
            self.read(Reg::ChipId2)?,
            self.read(Reg::ChipVersion)?,
        ])
    }

    /// Bring the codec from power-on to configured and powered down:
    /// slave on the I2S bus, master clock made from the bit clock, no
    /// inversions, the ADC's analogue front end at its defaults. Once
    /// after power.
    pub fn open(&mut self) -> Result<(), I2C::Error> {
        if self.read(Reg::Sys0D)? != 0xFA {
            self.write(Reg::Sys0D, 0xFA)?;
        }
        // I2C noise immunity; written twice because the first write
        // after power is sometimes lost (the reference's note).
        self.write(Reg::Gpio44, 0x08)?;
        self.write(Reg::Gpio44, 0x08)?;
        self.write(Reg::Clk01, 0x30)?;
        self.write(Reg::Clk02, 0x00)?;
        self.write(Reg::Clk03, 0x10)?;
        self.write(Reg::Adc16, 0x24)?;
        self.write(Reg::Clk04, 0x10)?;
        self.write(Reg::Clk05, 0x00)?;
        self.write(Reg::Sys0B, 0x00)?;
        self.write(Reg::Sys0C, 0x00)?;
        self.write(Reg::Sys10, 0x1F)?;
        self.write(Reg::Sys11, 0x7F)?;
        self.write(Reg::Reset00, 0x80)?;
        // Slave: bit 6 clear.
        self.update(Reg::Reset00, |v| v & 0xBF)?;
        // Internal MCLK from SCLK (bit 7), not inverted (bit 6), all
        // clocks on.
        self.write(Reg::Clk01, 0xBF)?;
        // SCLK not inverted.
        self.update(Reg::Clk06, |v| v & !0x20)?;
        self.write(Reg::Sys13, 0x10)?;
        self.write(Reg::Adc1B, 0x0A)?;
        self.write(Reg::Adc1C, 0x6A)?;
        // The ADC's right channel carries the DAC's output as a
        // reference, as the BSP configures it.
        self.write(Reg::Gpio44, 0x58)
    }

    /// Set the stream's sample rate and sample width, I2S (Philips)
    /// framing, in slots as wide as the samples (32-bit slots at 8 kHz,
    /// where the bit clock would otherwise be too slow to multiply).
    /// The rate must be in the clock table: 8, 11.025, 12, 16, 22.05,
    /// 24, 32, 44.1, 48, 64 or 96 kHz; the width 16, 24 or 32.
    pub fn set_format(
        &mut self,
        rate: u32,
        bits: u8,
    ) -> Result<(), Error<I2C::Error>> {
        let slot_bits = if rate == 8000 { 32 } else { bits };
        self.set_format_in_slots(rate, bits, slot_bits)
    }

    /// [`set_format`](Self::set_format) with the slot width said: the
    /// bit clock is `2 x slot_bits x rate`, and without an MCLK pin the
    /// codec multiplies it to `256 x rate` — by 8 for 16-bit slots, by 4
    /// for 32-bit slots. Other slot widths are refused.
    pub fn set_format_in_slots(
        &mut self,
        rate: u32,
        bits: u8,
        slot_bits: u8,
    ) -> Result<(), Error<I2C::Error>> {
        let coeff = *COEFFS
            .iter()
            .find(|c| c.rate == rate)
            .ok_or(Error::Unsupported)?;
        let multi: u8 = match slot_bits {
            16 => 3,
            32 => 2,
            _ => return Err(Error::Unsupported),
        };
        let width = |v: u8| match bits {
            24 => v & !0x1C,
            32 => v | 0x10,
            _ => v | 0x0C,
        };
        // Width, then I2S framing (low two bits clear), on both ports.
        for reg in [Reg::SdpIn09, Reg::SdpOut0A] {
            self.update(reg, width)?;
        }
        for reg in [Reg::SdpIn09, Reg::SdpOut0A] {
            self.update(reg, |v| v & 0xFC)?;
        }
        // Pre-divider and multiplier. Without an MCLK pin the codec
        // multiplies the bit clock up to MCLK = LRCK x 256: x8 from
        // 16-bit stereo slots (32 fs), x4 from 32-bit slots (64 fs).
        self.update(Reg::Clk02, |v| {
            (v & 0x07) | ((coeff.pre_div - 1) << 5) | (multi << 3)
        })?;
        self.write(
            Reg::Clk05,
            ((coeff.adc_div - 1) << 4) | (coeff.dac_div - 1),
        )?;
        self.update(Reg::Clk03, |v| {
            (v & 0x80) | (coeff.fs_mode << 6) | coeff.adc_osr
        })?;
        self.update(Reg::Clk04, |v| (v & 0x80) | coeff.dac_osr)?;
        self.update(Reg::Clk07, |v| (v & 0xC0) | coeff.lrck_h)?;
        self.write(Reg::Clk08, coeff.lrck_l)?;
        let bclk = if coeff.bclk_div < 19 {
            coeff.bclk_div - 1
        } else {
            coeff.bclk_div
        };
        self.update(Reg::Clk06, |v| (v & 0xE0) | bclk)?;
        Ok(())
    }

    /// Power the DAC path up and let the clocks run. The amplifier's
    /// enable and the unmute are the caller's next two steps, in that
    /// order.
    pub fn start(&mut self) -> Result<(), I2C::Error> {
        self.write(Reg::Reset00, 0x80)?;
        self.write(Reg::Clk01, 0xBF)?;
        // Serial ports out of power-down.
        for reg in [Reg::SdpIn09, Reg::SdpOut0A] {
            self.update(reg, |v| v & 0xBF)?;
        }
        self.write(Reg::Adc17, 0xBF)?;
        self.write(Reg::Sys0E, 0x02)?;
        // DAC enabled.
        self.write(Reg::Sys12, 0x00)?;
        self.write(Reg::Sys14, 0x1A)?;
        // Analogue microphone, not digital.
        self.update(Reg::Sys14, |v| v & !0x40)?;
        self.write(Reg::Sys0D, 0x01)?;
        self.write(Reg::Adc15, 0x40)?;
        self.write(Reg::Dac37, 0x08)?;
        self.write(Reg::Gp45, 0x00)
    }

    /// Power everything down. The reverse of [`start`](Self::start);
    /// mute and the amplifier off first.
    pub fn suspend(&mut self) -> Result<(), I2C::Error> {
        self.write(Reg::Dac32, 0x00)?;
        self.write(Reg::Adc17, 0x00)?;
        self.write(Reg::Sys0E, 0xFF)?;
        self.write(Reg::Sys12, 0x02)?;
        self.write(Reg::Sys14, 0x00)?;
        self.write(Reg::Sys0D, 0xFA)?;
        self.write(Reg::Adc15, 0x00)?;
        self.write(Reg::Clk02, 0x10)?;
        self.write(Reg::Reset00, 0x00)?;
        self.write(Reg::Reset00, 0x1F)?;
        self.write(Reg::Clk01, 0x30)?;
        self.write(Reg::Clk01, 0x00)?;
        self.write(Reg::Gp45, 0x00)?;
        self.write(Reg::Sys0D, 0xFC)?;
        self.write(Reg::Clk02, 0x00)
    }

    /// Mute or unmute the DAC.
    pub fn set_mute(&mut self, mute: bool) -> Result<(), I2C::Error> {
        self.update(Reg::Dac31, |v| {
            let v = v & 0x9F;
            if mute {
                v | 0x60
            } else {
                v
            }
        })
    }

    /// Set the DAC level in half-decibels; see [`volume_register`].
    pub fn set_volume(&mut self, half_db: i16) -> Result<(), I2C::Error> {
        self.write(Reg::Dac32, volume_register(half_db))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embedded_hal_mock::eh1::i2c::{Mock, Transaction};

    fn w(reg: u8, v: u8) -> Transaction {
        Transaction::write(ADDRESS, vec![reg, v])
    }

    fn r(reg: u8, v: u8) -> Transaction {
        Transaction::write_read(ADDRESS, vec![reg], vec![v])
    }

    #[test]
    fn open_is_the_reference_sequence_for_slave_without_mclk() {
        let mut i2c = Mock::new(&[
            r(0x0D, 0x00),
            w(0x0D, 0xFA),
            w(0x44, 0x08),
            w(0x44, 0x08),
            w(0x01, 0x30),
            w(0x02, 0x00),
            w(0x03, 0x10),
            w(0x16, 0x24),
            w(0x04, 0x10),
            w(0x05, 0x00),
            w(0x0B, 0x00),
            w(0x0C, 0x00),
            w(0x10, 0x1F),
            w(0x11, 0x7F),
            w(0x00, 0x80),
            r(0x00, 0xC0),
            w(0x00, 0x80),
            w(0x01, 0xBF),
            r(0x06, 0x20),
            w(0x06, 0x00),
            w(0x13, 0x10),
            w(0x1B, 0x0A),
            w(0x1C, 0x6A),
            w(0x44, 0x58),
        ]);
        let mut c = Es8311::new(i2c.clone());
        c.open().unwrap();
        c.release();
        i2c.done();
    }

    #[test]
    fn open_skips_the_power_write_when_already_fa() {
        let mut i2c = Mock::new(&[
            r(0x0D, 0xFA),
            w(0x44, 0x08),
            w(0x44, 0x08),
            w(0x01, 0x30),
            w(0x02, 0x00),
            w(0x03, 0x10),
            w(0x16, 0x24),
            w(0x04, 0x10),
            w(0x05, 0x00),
            w(0x0B, 0x00),
            w(0x0C, 0x00),
            w(0x10, 0x1F),
            w(0x11, 0x7F),
            w(0x00, 0x80),
            r(0x00, 0x80),
            w(0x00, 0x80),
            w(0x01, 0xBF),
            r(0x06, 0x00),
            w(0x06, 0x00),
            w(0x13, 0x10),
            w(0x1B, 0x0A),
            w(0x1C, 0x6A),
            w(0x44, 0x58),
        ]);
        let mut c = Es8311::new(i2c.clone());
        c.open().unwrap();
        c.release();
        i2c.done();
    }

    #[test]
    fn format_44100_16_bit_sets_the_dividers_for_bclk_x8() {
        let mut i2c = Mock::new(&[
            r(0x09, 0x00),
            w(0x09, 0x0C),
            r(0x0A, 0x00),
            w(0x0A, 0x0C),
            r(0x09, 0x0C),
            w(0x09, 0x0C),
            r(0x0A, 0x0C),
            w(0x0A, 0x0C),
            // pre_div 1 -> 0 << 5; multi x8 -> 3 << 3 = 0x18; low bits kept.
            r(0x02, 0xFF),
            w(0x02, 0x07 | 0x18),
            w(0x05, 0x00),
            r(0x03, 0x80),
            w(0x03, 0x80 | 0x10),
            r(0x04, 0x00),
            w(0x04, 0x10),
            r(0x07, 0xFF),
            w(0x07, 0xC0),
            w(0x08, 0xFF),
            // bclk_div 4 -> 3; SCLK invert bit (0x20) kept.
            r(0x06, 0x20),
            w(0x06, 0x23),
        ]);
        let mut c = Es8311::new(i2c.clone());
        c.set_format(44_100, 16).unwrap();
        c.release();
        i2c.done();
    }

    #[test]
    fn format_8000_uses_x4_and_24_bit_clears_the_width_bits() {
        let mut i2c = Mock::new(&[
            r(0x09, 0x1C),
            w(0x09, 0x00),
            r(0x0A, 0x1C),
            w(0x0A, 0x00),
            r(0x09, 0x03),
            w(0x09, 0x00),
            r(0x0A, 0x03),
            w(0x0A, 0x00),
            r(0x02, 0x00),
            w(0x02, 2 << 3),
            w(0x05, 0x00),
            r(0x03, 0x00),
            w(0x03, 0x10),
            r(0x04, 0x00),
            w(0x04, 0x20),
            r(0x07, 0x00),
            w(0x07, 0x00),
            w(0x08, 0xFF),
            r(0x06, 0x00),
            w(0x06, 0x03),
        ]);
        let mut c = Es8311::new(i2c.clone());
        c.set_format(8000, 24).unwrap();
        c.release();
        i2c.done();
    }

    #[test]
    fn an_unlisted_rate_is_refused_before_any_write() {
        let mut i2c = Mock::new(&[]);
        let mut c = Es8311::new(i2c.clone());
        assert_eq!(c.set_format(88_200, 16), Err(Error::Unsupported));
        assert_eq!(
            c.set_format_in_slots(44_100, 16, 24),
            Err(Error::Unsupported)
        );
        c.release();
        i2c.done();
    }

    #[test]
    fn sixteen_bit_data_in_32_bit_slots_multiplies_bclk_x4() {
        let mut i2c = Mock::new(&[
            r(0x09, 0x00),
            w(0x09, 0x0C),
            r(0x0A, 0x00),
            w(0x0A, 0x0C),
            r(0x09, 0x0C),
            w(0x09, 0x0C),
            r(0x0A, 0x0C),
            w(0x0A, 0x0C),
            // pre_div 1 -> 0 << 5; multi x4 -> 2 << 3 = 0x10.
            r(0x02, 0x00),
            w(0x02, 0x10),
            w(0x05, 0x00),
            r(0x03, 0x80),
            w(0x03, 0x80 | 0x10),
            r(0x04, 0x00),
            w(0x04, 0x10),
            r(0x07, 0xFF),
            w(0x07, 0xC0),
            w(0x08, 0xFF),
            r(0x06, 0x20),
            w(0x06, 0x23),
        ]);
        let mut c = Es8311::new(i2c.clone());
        c.set_format_in_slots(44_100, 16, 32).unwrap();
        c.release();
        i2c.done();
    }

    #[test]
    fn start_powers_the_dac_path() {
        let mut i2c = Mock::new(&[
            w(0x00, 0x80),
            w(0x01, 0xBF),
            r(0x09, 0x4C),
            w(0x09, 0x0C),
            r(0x0A, 0x4C),
            w(0x0A, 0x0C),
            w(0x17, 0xBF),
            w(0x0E, 0x02),
            w(0x12, 0x00),
            w(0x14, 0x1A),
            r(0x14, 0x5A),
            w(0x14, 0x1A),
            w(0x0D, 0x01),
            w(0x15, 0x40),
            w(0x37, 0x08),
            w(0x45, 0x00),
        ]);
        let mut c = Es8311::new(i2c.clone());
        c.start().unwrap();
        c.release();
        i2c.done();
    }

    #[test]
    fn suspend_is_the_reference_sequence() {
        let mut i2c = Mock::new(&[
            w(0x32, 0x00),
            w(0x17, 0x00),
            w(0x0E, 0xFF),
            w(0x12, 0x02),
            w(0x14, 0x00),
            w(0x0D, 0xFA),
            w(0x15, 0x00),
            w(0x02, 0x10),
            w(0x00, 0x00),
            w(0x00, 0x1F),
            w(0x01, 0x30),
            w(0x01, 0x00),
            w(0x45, 0x00),
            w(0x0D, 0xFC),
            w(0x02, 0x00),
        ]);
        let mut c = Es8311::new(i2c.clone());
        c.suspend().unwrap();
        c.release();
        i2c.done();
    }

    #[test]
    fn mute_sets_both_bits_and_keeps_the_rest() {
        let mut i2c = Mock::new(&[
            r(0x31, 0x81),
            w(0x31, 0xE1),
            r(0x31, 0xE1),
            w(0x31, 0x81),
        ]);
        let mut c = Es8311::new(i2c.clone());
        c.set_mute(true).unwrap();
        c.set_mute(false).unwrap();
        c.release();
        i2c.done();
    }

    #[test]
    fn the_bit_clock_is_32_frames_except_at_8k() {
        assert_eq!(bit_clock_for(44_100), 44_100 * 32);
        assert_eq!(bit_clock_for(48_000), 1_536_000);
        assert_eq!(bit_clock_for(8000), 512_000);
    }

    #[test]
    fn volume_register_scale() {
        assert_eq!(volume_register(0), 0xBF);
        assert_eq!(volume_register(-191), 0x00);
        assert_eq!(volume_register(-500), 0x00);
        assert_eq!(volume_register(64), 0xFF);
        assert_eq!(volume_register(100), 0xFF);
    }

    #[test]
    fn percent_curve_matches_the_reference_with_the_boards_gain() {
        // 3.3 V DAC into a 5 V amplifier: 20 log10(3.3/5) = -3.6 dB, -7
        // half-steps, so full scale asks the DAC for +3.5 dB.
        let g = -7;
        assert_eq!(percent_to_half_db(0, g), -192);
        assert_eq!(percent_to_half_db(100, g), 7);
        assert_eq!(percent_to_half_db(1, g), -99 + 7);
        assert_eq!(percent_to_half_db(50, g), -50 + 7);
        assert_eq!(percent_to_half_db(200, g), 7);
        assert_eq!(volume_register(percent_to_half_db(0, g)), 0x00);
        assert_eq!(volume_register(percent_to_half_db(100, g)), 0xC6);
        // Without hardware gain, 100 is exactly 0 dB.
        assert_eq!(volume_register(percent_to_half_db(100, 0)), 0xBF);
    }

    #[test]
    fn volume_writes_the_register() {
        let mut i2c = Mock::new(&[w(0x32, 0xBF), w(0x32, 0x00)]);
        let mut c = Es8311::new(i2c.clone());
        c.set_volume(0).unwrap();
        c.set_volume(-192).unwrap();
        c.release();
        i2c.done();
    }

    #[test]
    fn chip_id_reads_the_three_bytes() {
        let mut i2c = Mock::new(&[r(0xFD, 0x83), r(0xFE, 0x11), r(0xFF, 0x01)]);
        let mut c = Es8311::new(i2c.clone());
        assert_eq!(c.chip_id().unwrap(), [0x83, 0x11, 0x01]);
        c.release();
        i2c.done();
    }
}

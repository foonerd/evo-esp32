//! The TI PCM5121/PCM5122 stereo DAC as Raspberry Pi HATs carry it.
//!
//! A 112 dB DirectPath converter controlled over I2C (`0x4C` to `0x4F`
//! by its ADR pins; IQaudIO's DAC+ and Waveshare's PCM5122 Audio Board
//! wire `0x4C`, HiFiBerry's DAC+ `0x4D`), fed I2S from the header with
//! no master clock: the chip's PLL runs from BCK and its clock dividers
//! set themselves from what it sees on BCK and LRCK. The HAT has no
//! oscillator of its own; everything the chip needs is on the three
//! I2S lines once it has been told to look there.
//!
//! The register sequence is the Linux `pcm512x` codec driver's for a
//! clock-consumer DAC with no SCLK (`pcm512x_probe`,
//! `pcm512x_dai_startup_slave`, `pcm512x_set_fmt`, `pcm512x_hw_params`,
//! the bias levels), which is what every Raspberry Pi runs for these
//! boards: reset; standby; ignore the missing SCK; PLL reference BCK;
//! BCK and LRCK as inputs; I2S format and the word length; the clock
//! dividers left to the chip; out of standby. The data sheet (SLAS763C,
//! 8.3.6.3, Table 33) allows BCK at 32 or 64 times the sample rate for
//! the PLL, 32 kHz to 384 kHz.
//!
//! What a device does with it: [`Pcm512x::detect`] to make sure the
//! chip is there before a register is written, [`Pcm512x::init`] once
//! after power with the word length the I2S carries,
//! [`Pcm512x::set_attenuation`] and [`Pcm512x::set_mute`] at any time,
//! [`Pcm512x::clocks`] for what the chip makes of its clocks. Nothing
//! here starts a clock: the I2S side is the device's.
//!
//! The driver is `no_std`, has no `unsafe`, and speaks only
//! `embedded_hal::i2c::I2c`.
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

use embedded_hal::i2c::I2c;

/// The 7-bit address with ADR1 and ADR2 low: IQaudIO DAC+, Waveshare
/// PCM5122 Audio Board (A).
pub const ADDRESS: u8 = 0x4C;
/// The 7-bit address HiFiBerry's DAC+ wires (ADR1 high).
pub const ADDRESS_ADR1: u8 = 0x4D;

/// The digital volume register value for 0 dB; each step above it is
/// 0.5 dB of attenuation (data sheet 8.3.5.4).
pub const VOLUME_0DB: u8 = 0x30;
/// The deepest attenuation the register expresses, -103 dB.
pub const ATTEN_MAX_HALF_DB: u8 = 0xFE - VOLUME_0DB;
/// The register value that mutes a channel.
pub const VOLUME_MUTE: u8 = 0xFF;

/// Page 0 registers this driver touches, by the data sheet's numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Reg {
    Page = 0,
    Reset = 1,
    Power = 2,
    Mute = 3,
    PllEn = 4,
    BckLrckCfg = 9,
    MasterMode = 12,
    PllRef = 13,
    ErrorDetect = 37,
    I2s1 = 40,
    I2s2 = 41,
    DacRouting = 42,
    DigitalVolumeLeft = 61,
    DigitalVolumeRight = 62,
    RateDet1 = 91,
    RateDet4 = 94,
    ClockStatus = 95,
}

/// Register 1: reset the registers and the modules.
const RESET_ALL: u8 = 0x11;
/// Register 2: request standby.
const POWER_STANDBY: u8 = 0x10;
/// Register 3: mute both channels.
const MUTE_BOTH: u8 = 0x11;
/// Register 13: the PLL's reference is BCK.
const PLL_REF_BCK: u8 = 0x10;
/// Register 37: a missing SCK is not a clock error (IDCH); the clock
/// divider auto-set (DCAS = 0) stays on.
const IGNORE_SCK_HALT: u8 = 0x08;
/// Register 12 after reset, its two clock-provider bits masked.
const MASTER_MODE_DEFAULT: u8 = 0x7C;
/// Register 42 after reset: left to left, right to right.
const DAC_ROUTING_DEFAULT: u8 = 0x11;
/// Register 94: set while no SCK arrives (the HiFiBerry machine driver
/// reads this bit to tell a DAC+ Pro's oscillators).
const SCK_MISSING: u8 = 0x40;

/// How many bits of each I2S slot carry audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum WordLength {
    /// 16-bit words.
    Sixteen = 0,
    /// 20-bit words.
    Twenty = 1,
    /// 24-bit words.
    TwentyFour = 2,
    /// 32-bit words.
    ThirtyTwo = 3,
}

/// What the chip said about itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    /// Register 12 as read.
    pub master_mode: u8,
    /// Register 42 as read.
    pub routing: u8,
}

/// What the chip makes of its clocks, raw, for a log line: register 4
/// (PLL enable and lock flag), 91 (the detected rate) and 95 (clock
/// status), as the data sheet lays them out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clocks {
    /// Register 4.
    pub pll: u8,
    /// Register 91.
    pub rate: u8,
    /// Register 95.
    pub status: u8,
}

/// The DAC, over a bus the caller provides.
#[derive(Debug)]
pub struct Pcm512x<I2C> {
    i2c: I2C,
    address: u8,
}

impl<I2C: I2c> Pcm512x<I2C> {
    /// The DAC at [`ADDRESS`].
    pub fn new(i2c: I2C) -> Self {
        Self::at(i2c, ADDRESS)
    }

    /// The DAC at another 7-bit address.
    pub fn at(i2c: I2C, address: u8) -> Self {
        Self { i2c, address }
    }

    /// The address this driver speaks to.
    pub fn address(&self) -> u8 {
        self.address
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

    /// Read two registers the driver never changes and say whether a
    /// PCM512x answers: register 12 reads `0x7C` in its upper six bits
    /// and register 42 reads `0x11`. The chip has no identity register;
    /// these are its reset values, held through this driver's own
    /// configuration, so a warm restart finds it too. `Ok(None)` is a
    /// device at the address that is not this chip; a bus error (no
    /// acknowledge) is a bus error.
    pub fn detect(&mut self) -> Result<Option<Identity>, I2C::Error> {
        let master_mode = self.read(Reg::MasterMode)?;
        let routing = self.read(Reg::DacRouting)?;
        let is_one = (master_mode & 0xFC) == MASTER_MODE_DEFAULT
            && routing == DAC_ROUTING_DEFAULT;
        Ok(is_one.then_some(Identity {
            master_mode,
            routing,
        }))
    }

    /// The Linux driver's bring-up for a HAT with no master clock, in
    /// its order: page 0; reset registers and modules; standby; a
    /// missing SCK is not an error; the PLL on BCK; BCK and LRCK as
    /// inputs; I2S with `word` bits; no data offset; both channels at
    /// 0 dB, unmuted; out of standby. Register 12's clock-provider bits
    /// are already clear after the reset. The clock dividers are not
    /// written: the chip sets them from BCK and LRCK as they run, so a
    /// change of sample rate needs nothing said. Once after power.
    pub fn init(&mut self, word: WordLength) -> Result<(), I2C::Error> {
        self.write(Reg::Page, 0)?;
        self.write(Reg::Reset, RESET_ALL)?;
        self.write(Reg::Reset, 0)?;
        self.write(Reg::Power, POWER_STANDBY)?;
        self.write(Reg::ErrorDetect, IGNORE_SCK_HALT)?;
        self.write(Reg::PllRef, PLL_REF_BCK)?;
        self.write(Reg::BckLrckCfg, 0)?;
        self.write(Reg::I2s1, word as u8)?;
        self.write(Reg::I2s2, 0)?;
        self.set_attenuation(0)?;
        self.set_mute(false)?;
        self.write(Reg::Power, 0)
    }

    /// Mute or unmute both channels through register 3; the chip ramps.
    pub fn set_mute(&mut self, mute: bool) -> Result<(), I2C::Error> {
        self.write(Reg::Mute, if mute { MUTE_BOTH } else { 0 })
    }

    /// Both channels at `half_db` half-decibels below 0 dB, up to
    /// [`ATTEN_MAX_HALF_DB`]; more is held there. The register's gain
    /// range above 0 dB is not offered: the host's level is the host's.
    pub fn set_attenuation(&mut self, half_db: u8) -> Result<(), I2C::Error> {
        let v = VOLUME_0DB + half_db.min(ATTEN_MAX_HALF_DB);
        self.write(Reg::DigitalVolumeLeft, v)?;
        self.write(Reg::DigitalVolumeRight, v)
    }

    /// Registers 4, 91 and 95 as read, for a log line.
    pub fn clocks(&mut self) -> Result<Clocks, I2C::Error> {
        Ok(Clocks {
            pll: self.read(Reg::PllEn)?,
            rate: self.read(Reg::RateDet1)?,
            status: self.read(Reg::ClockStatus)?,
        })
    }

    /// Whether a system clock arrives on SCK: a board with an oscillator
    /// of its own that is running (a DAC+ Pro, an Allo Boss and their
    /// kin), which this bring-up does not expect; a 3-wire board reads
    /// `false`.
    pub fn sck_present(&mut self) -> Result<bool, I2C::Error> {
        Ok(self.read(Reg::RateDet4)? & SCK_MISSING == 0)
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
    fn detect_reads_two_reset_values_and_masks_the_provider_bits() {
        let mut i2c = Mock::new(&[
            r(12, 0x7C),
            r(42, 0x11),
            r(12, 0x7F),
            r(42, 0x11),
            r(12, 0x00),
            r(42, 0x00),
        ]);
        let mut dac = Pcm512x::new(&mut i2c);
        assert_eq!(
            dac.detect().unwrap(),
            Some(Identity {
                master_mode: 0x7C,
                routing: 0x11
            })
        );
        assert!(dac.detect().unwrap().is_some());
        assert_eq!(dac.detect().unwrap(), None);
        i2c.done();
    }

    #[test]
    fn init_is_the_linux_slave_sequence_for_a_hat_without_sck() {
        let mut i2c = Mock::new(&[
            w(0, 0x00),
            w(1, 0x11),
            w(1, 0x00),
            w(2, 0x10),
            w(37, 0x08),
            w(13, 0x10),
            w(9, 0x00),
            w(40, 0x00),
            w(41, 0x00),
            w(61, 0x30),
            w(62, 0x30),
            w(3, 0x00),
            w(2, 0x00),
        ]);
        Pcm512x::new(&mut i2c).init(WordLength::Sixteen).unwrap();
        i2c.done();
    }

    #[test]
    fn the_word_length_is_register_40s_low_bits() {
        let mut i2c = Mock::new(&[w(40, 0x03)]);
        let mut dac = Pcm512x::new(&mut i2c);
        dac.write(Reg::I2s1, WordLength::ThirtyTwo as u8).unwrap();
        i2c.done();
    }

    #[test]
    fn attenuation_is_half_decibels_above_0x30_and_saturates() {
        let mut i2c = Mock::new(&[
            w(61, 0x30),
            w(62, 0x30),
            w(61, 0x3C),
            w(62, 0x3C),
            w(61, 0xFE),
            w(62, 0xFE),
        ]);
        let mut dac = Pcm512x::new(&mut i2c);
        dac.set_attenuation(0).unwrap();
        dac.set_attenuation(12).unwrap(); // -6 dB
        dac.set_attenuation(255).unwrap();
        i2c.done();
    }

    #[test]
    fn sck_presence_is_register_94s_bit_6_inverted() {
        let mut i2c = Mock::new(&[r(94, 0x40), r(94, 0x00)]);
        let mut dac = Pcm512x::new(&mut i2c);
        assert!(!dac.sck_present().unwrap());
        assert!(dac.sck_present().unwrap());
        i2c.done();
    }

    #[test]
    fn mute_writes_register_3_and_another_address_is_honoured() {
        let mut i2c = Mock::new(&[
            Transaction::write(ADDRESS_ADR1, vec![3, 0x11]),
            Transaction::write(ADDRESS_ADR1, vec![3, 0x00]),
            Transaction::write_read(ADDRESS_ADR1, vec![4], vec![0x01]),
            Transaction::write_read(ADDRESS_ADR1, vec![91], vec![0x12]),
            Transaction::write_read(ADDRESS_ADR1, vec![95], vec![0x00]),
        ]);
        let mut dac = Pcm512x::at(&mut i2c, ADDRESS_ADR1);
        assert_eq!(dac.address(), 0x4D);
        dac.set_mute(true).unwrap();
        dac.set_mute(false).unwrap();
        assert_eq!(
            dac.clocks().unwrap(),
            Clocks {
                pll: 0x01,
                rate: 0x12,
                status: 0x00
            }
        );
        i2c.done();
    }
}

//! The ESS ES9018K2M stereo DAC as Raspberry Pi HATs carry it.
//!
//! A SABRE32 converter with its own oscillator on the HAT (LightSABRE,
//! Audiophonics I-Sabre K2M and their kin), fed I2S from the header and
//! controlled over I2C at `0x48` (the `0x90` of its data sheet, in
//! 8-bit). The host supplies no master clock and asks for nothing
//! clever: the chip auto-detects the serial format, runs its ASRC and
//! jitter eliminator, and takes volume as attenuation per channel.
//!
//! The register sequence is the one
//! [es9018k2m-plugin](https://github.com/foonerd/es9018k2m-plugin)
//! (MIT, Just a Nerd) has run on Volumio players: detection by the
//! status register, the init writes in its order, the mute bit in
//! register 7, the 0.5 dB attenuation registers for volume. Transcribed
//! for a device that owns the bus; the plugin's filter choices and
//! graceful ramps are not here yet.
//!
//! What a device does with it: [`Es9018k2m::detect`] to make sure the
//! chip is there before a register is written, [`Es9018k2m::init`] once
//! after power, [`Es9018k2m::set_volume`] and [`Es9018k2m::set_mute`] at
//! any time. Nothing here starts a clock: the I2S side is the device's,
//! and the HAT's oscillator is its own.
//!
//! The driver is `no_std`, has no `unsafe`, and speaks only
//! `embedded_hal::i2c::I2c`.
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

use embedded_hal::i2c::I2c;

/// The 7-bit address the HATs wire.
pub const ADDRESS: u8 = 0x48;

/// The attenuation register value for 0 dB.
pub const ATTEN_0DB: u8 = 0x00;
/// The deepest attenuation the plugin uses as its floor, -49.5 dB.
pub const ATTEN_FLOOR: u8 = 0x63;
/// The register value that mutes a channel outright.
pub const ATTEN_MUTE: u8 = 0xFF;

/// Registers this driver touches, by the data sheet's numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Reg {
    System = 0x00,
    InputConfig = 0x01,
    AutomuteTime = 0x04,
    AutomuteLevel = 0x05,
    DeemphasisRamp = 0x06,
    General = 0x07,
    Gpio = 0x08,
    Dpll = 0x0C,
    SoftStart = 0x0E,
    VolumeLeft = 0x0F,
    VolumeRight = 0x10,
    GpioOsf = 0x15,
    AsrcLatch = 0x1B,
    Status = 0x40,
}

/// What the chip said about itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    /// The status register as read.
    pub status: u8,
    /// The two revision bits.
    pub revision: u8,
}

/// The DAC, over a bus the caller provides.
#[derive(Debug)]
pub struct Es9018k2m<I2C> {
    i2c: I2C,
    address: u8,
    /// Register 7 as last written: the mute bit lives among the filter
    /// bits, so it is kept rather than read back.
    general: u8,
}

/// The reset value of register 7 the plugin starts from.
const GENERAL_DEFAULT: u8 = 0x80;
const MUTE_BIT: u8 = 0x01;

impl<I2C: I2c> Es9018k2m<I2C> {
    /// The DAC at the HATs' address.
    pub fn new(i2c: I2C) -> Self {
        Self::at(i2c, ADDRESS)
    }

    /// The DAC at another 7-bit address.
    pub fn at(i2c: I2C, address: u8) -> Self {
        Self {
            i2c,
            address,
            general: GENERAL_DEFAULT,
        }
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

    /// Read the status register and say whether an ES9018K2M answers:
    /// bits 4..2 read `100`. `Ok(None)` is a device at the address that
    /// is not this chip; a bus error is a bus error.
    pub fn detect(&mut self) -> Result<Option<Identity>, I2C::Error> {
        let status = self.read(Reg::Status)?;
        Ok(((status & 0x1C) == 0x10).then_some(Identity {
            status,
            revision: status & 0x03,
        }))
    }

    /// The plugin's bring-up, in its order: muted, then the system,
    /// input (32-bit I2S, auto-detect), automute, de-emphasis and ramp,
    /// GPIO, DPLL, soft start, OSF and ASRC registers, then both
    /// channels at 0 dB, then unmuted. Once after power.
    pub fn init(&mut self) -> Result<(), I2C::Error> {
        self.set_mute(true)?;
        self.write(Reg::System, 0x00)?;
        self.write(Reg::InputConfig, 0xC4)?;
        self.write(Reg::AutomuteTime, 0x10)?;
        self.write(Reg::AutomuteLevel, 0x68)?;
        self.write(Reg::DeemphasisRamp, 0x47)?;
        self.write(Reg::Gpio, 0x01)?;
        self.write(Reg::Dpll, 0x5F)?;
        self.write(Reg::SoftStart, 0x8A)?;
        self.write(Reg::GpioOsf, 0x00)?;
        self.write(Reg::AsrcLatch, 0xD4)?;
        self.set_attenuation(ATTEN_0DB, ATTEN_0DB)?;
        self.set_mute(false)
    }

    /// Mute or unmute through register 7's mute bit; the filter bits
    /// stay as they were.
    pub fn set_mute(&mut self, mute: bool) -> Result<(), I2C::Error> {
        self.general = if mute {
            self.general | MUTE_BIT
        } else {
            self.general & !MUTE_BIT
        };
        self.write(Reg::General, self.general)
    }

    /// Attenuation per channel in half-decibels: `0` is 0 dB, `0x63` is
    /// -49.5 dB, `0xFF` mutes the channel.
    pub fn set_attenuation(
        &mut self,
        left: u8,
        right: u8,
    ) -> Result<(), I2C::Error> {
        self.write(Reg::VolumeLeft, left)?;
        self.write(Reg::VolumeRight, right)
    }

    /// Volume on the plugin's 0 to 100 curve: 100 is 0 dB, 1 is a step
    /// above the -49.5 dB floor, 0 mutes both channels.
    pub fn set_volume(&mut self, percent: u8) -> Result<(), I2C::Error> {
        let a = attenuation_for(percent);
        self.set_attenuation(a, a)
    }
}

/// The plugin's volume curve: linear in attenuation from the floor at
/// 1 % to 0 dB at 100 %, rounded as it rounds; 0 % is the mute value.
pub const fn attenuation_for(percent: u8) -> u8 {
    if percent == 0 {
        return ATTEN_MUTE;
    }
    let p = if percent > 100 { 100 } else { percent } as u32;
    let floor = ATTEN_FLOOR as u32;
    // round(floor - p * floor / 100), in integers.
    ((floor * 100 - p * floor + 50) / 100) as u8
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
    fn detect_reads_the_status_register_and_checks_the_id_bits() {
        let mut i2c = Mock::new(&[r(0x40, 0x12), r(0x40, 0x20)]);
        let mut dac = Es9018k2m::new(&mut i2c);
        assert_eq!(
            dac.detect().unwrap(),
            Some(Identity {
                status: 0x12,
                revision: 2
            })
        );
        assert_eq!(dac.detect().unwrap(), None);
        i2c.done();
    }

    #[test]
    fn init_is_the_plugin_sequence_muted_then_unmuted() {
        let mut i2c = Mock::new(&[
            w(0x07, 0x81),
            w(0x00, 0x00),
            w(0x01, 0xC4),
            w(0x04, 0x10),
            w(0x05, 0x68),
            w(0x06, 0x47),
            w(0x08, 0x01),
            w(0x0C, 0x5F),
            w(0x0E, 0x8A),
            w(0x15, 0x00),
            w(0x1B, 0xD4),
            w(0x0F, 0x00),
            w(0x10, 0x00),
            w(0x07, 0x80),
        ]);
        Es9018k2m::new(&mut i2c).init().unwrap();
        i2c.done();
    }

    #[test]
    fn mute_toggles_one_bit_and_keeps_the_rest() {
        let mut i2c = Mock::new(&[w(0x07, 0x81), w(0x07, 0x80), w(0x07, 0x81)]);
        let mut dac = Es9018k2m::new(&mut i2c);
        dac.set_mute(true).unwrap();
        dac.set_mute(false).unwrap();
        dac.set_mute(true).unwrap();
        i2c.done();
    }

    #[test]
    fn the_volume_curve_is_the_plugins() {
        assert_eq!(attenuation_for(0), ATTEN_MUTE);
        assert_eq!(attenuation_for(100), 0x00);
        assert_eq!(attenuation_for(50), 0x32); // round(99 - 49.5) = 50
        assert_eq!(attenuation_for(1), 0x62); // round(99 - 0.99) = 98
        assert_eq!(attenuation_for(200), 0x00);
        let mut i2c = Mock::new(&[w(0x0F, 0x32), w(0x10, 0x32)]);
        Es9018k2m::new(&mut i2c).set_volume(50).unwrap();
        i2c.done();
    }
}

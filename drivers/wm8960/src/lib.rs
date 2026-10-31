//! The Cirrus (Wolfson) WM8960 stereo codec as Raspberry Pi HATs carry it.
//!
//! Waveshare's WM8960 Audio HAT and the ReSpeaker 2-Mics class: a stereo
//! DAC and ADC with a headphone amplifier and 1 W class-D speaker drivers,
//! controlled over I2C at `0x1A`, fed over I2S. This driver is the DAC
//! path only: the stream in, the headphone jack and the speakers out.
//!
//! Three facts shape it, all from the datasheet (WM8960 Rev 4.2):
//!
//! - The control interface is **write-only**: a read to the address is
//!   ignored, so the codec never shows up to a bus scan that reads, and
//!   a driver keeps a shadow of every register it touches. Detection is
//!   a write: the reset register is written and the bus says whether
//!   anything acknowledged (see [`Wm8960::reset`]).
//! - Sample rates run **8 to 48 kHz**. Nothing above plays on it.
//! - SYSCLK comes from MCLK directly or through the codec's PLL. The
//!   Waveshare HAT carries a 24 MHz oscillator on MCLK (its schematic;
//!   its Linux driver fixes `freq_in` at 24 MHz and always uses the
//!   PLL), so this driver's clock table is the PLL from 24 MHz: SYSCLK
//!   12.288 MHz for the 48 kHz family and 11.2896 MHz for the 44.1 kHz
//!   family, the codec's DAC divider for the rates below. [`MCLK_HZ`]
//!   says what the table assumes.
//!
//! The order a device uses: [`Wm8960::reset`] (the probe), [`Wm8960::open`]
//! (bias up; the datasheet wants the reference to ramp before anything
//! else is enabled, so the caller waits [`OPEN_SETTLE_MS`]), then
//! [`Wm8960::start`] with the stream's rate; [`Wm8960::set_rate`] when the
//! stream's rate changes; [`Wm8960::set_mute`], the volumes, and
//! [`Wm8960::suspend`] at any time. The I2S side is the device's: Philips
//! format, 16-bit samples, the codec a slave to the device's clocks.
//!
//! The register sequence follows the Linux ASoC driver's power path
//! (the driver Waveshare ships for this HAT), transcribed for the DAC
//! alone and the HAT's clock; nothing is copied from it.
//!
//! The driver is `no_std`, has no `unsafe`, and speaks only
//! `embedded_hal::i2c::I2c` and `embedded_hal::delay::DelayNs`.
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

use embedded_hal::delay::DelayNs;
use embedded_hal::i2c::I2c;

/// The 7-bit address; the WM8960 has one.
pub const ADDRESS: u8 = 0x1A;

/// The master clock the clock table is computed for: the Waveshare HAT's
/// oscillator.
pub const MCLK_HZ: u32 = 24_000_000;

/// The highest sample rate the codec plays (datasheet: 8 to 48 kHz).
pub const MAX_RATE: u32 = 48_000;

/// How long the caller waits after [`Wm8960::open`] before
/// [`Wm8960::start`]: the reference ramps at 2 × 50 kΩ (what the vendor
/// driver sleeps).
pub const OPEN_SETTLE_MS: u32 = 100;

/// How long [`Wm8960::start`] and [`Wm8960::set_rate`] give the PLL
/// after enabling it before SYSCLK is switched to it. The datasheet
/// gives no lock time; the vendor driver sleeps 250 ms; the DAC is muted
/// meanwhile, so a short wait costs a glitch nobody hears.
const PLL_SETTLE_MS: u32 = 20;

/// Why a call failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error<E> {
    /// The bus said no.
    Bus(E),
    /// No clock setting exists for the rate asked: above 48 kHz, or not
    /// a rate the DAC divider makes.
    Unsupported,
}

impl<E> From<E> for Error<E> {
    fn from(e: E) -> Self {
        Error::Bus(e)
    }
}

// Registers, by the datasheet's numbering.
const LOUT1: u8 = 0x02;
const ROUT1: u8 = 0x03;
const CLOCK1: u8 = 0x04;
const DACCTL1: u8 = 0x05;
const IFACE1: u8 = 0x07;
const LDAC: u8 = 0x0A;
const RDAC: u8 = 0x0B;
const RESET: u8 = 0x0F;
const POWER1: u8 = 0x19;
const POWER2: u8 = 0x1A;
const APOP1: u8 = 0x1C;
const LOUTMIX: u8 = 0x22;
const ROUTMIX: u8 = 0x25;
const LOUT2: u8 = 0x28;
const ROUT2: u8 = 0x29;
const POWER3: u8 = 0x2F;
const CLASSD1: u8 = 0x31;
const PLL1: u8 = 0x34;
const PLL2: u8 = 0x35;
const PLL3: u8 = 0x36;
const PLL4: u8 = 0x37;

// R4 Clocking (1).
const CLKSEL_PLL: u16 = 1;
const SYSCLKDIV_2: u16 = 0b10 << 1;
const DACDIV_SHIFT: u16 = 3;
// R5 ADC & DAC control (1).
const DACMU: u16 = 1 << 3;
// R7 Audio interface: I2S, 16-bit words, slave, nothing inverted.
const IFACE_I2S_16_SLAVE: u16 = 0b10;
// R25 Power (1).
const VMID_50K: u16 = 0b01 << 7;
const VMID_250K: u16 = 0b10 << 7;
const VREF: u16 = 1 << 6;
// R26 Power (2).
const DACL: u16 = 1 << 8;
const DACR: u16 = 1 << 7;
const LOUT1_EN: u16 = 1 << 6;
const ROUT1_EN: u16 = 1 << 5;
const SPKL_EN: u16 = 1 << 4;
const SPKR_EN: u16 = 1 << 3;
const PLL_EN: u16 = 1;
// R28 Anti-pop (1).
const POBCTRL: u16 = 1 << 7;
const BUFDCOPEN: u16 = 1 << 4;
const BUFIOEN: u16 = 1 << 3;
const SOFT_ST: u16 = 1 << 2;
// R34 / R37 output mixers: the DAC into the mixer.
const D2O: u16 = 1 << 8;
// R47 Power (3).
const LOMIX: u16 = 1 << 3;
const ROMIX: u16 = 1 << 2;
// R49 Class D control (1): both speakers on over the fixed low bits.
const CLASSD_BOTH: u16 = 0b11 << 6;
const CLASSD_FIXED: u16 = 0b11_0111;
// R52 PLL N.
const SDM: u16 = 1 << 5;
const PLLPRESCALE: u16 = 1 << 4;
// Volume update latches.
const VU: u16 = 1 << 8;

/// The DAC's digital volume at 0 dB.
const DAC_0DB: u16 = 0xFF;
/// The analogue output volume at 0 dB: 1 dB steps from +6 dB at 0x7F
/// down to −73 dB at 0x30; below that, mute.
const OUT_0DB: u8 = 0x79;
const OUT_MAX_DB: i8 = 6;
const OUT_MIN_DB: i8 = -73;

/// The PLL and divider settings for a rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Clocks {
    /// PLLN, the integer part of f2/f1.
    n: u8,
    /// PLLK, the 24-bit fraction.
    k: u32,
    /// DACDIV, the code for SYSCLK / (d × 256).
    dacdiv: u8,
}

/// The 48 kHz family: SYSCLK 12.288 MHz. f1 = 24 MHz / 2 = 12 MHz,
/// f2 = 4 × 2 × 12.288 = 98.304 MHz, R = 8.192: the datasheet's own
/// example.
const PLL_48K: (u8, u32) = (8, 0x3126E9);
/// The 44.1 kHz family: SYSCLK 11.2896 MHz. f2 = 90.3168 MHz,
/// R = 7.5264, K = int(2^24 × 0.5264).
const PLL_44K1: (u8, u32) = (7, 0x86C226);

/// The clocks for `rate`, or `None` where the codec has no setting
/// (above 48 kHz, or a rate the DAC divider does not make).
fn clocks_for(rate: u32) -> Option<Clocks> {
    // DACDIV codes, R4: 000 = /1 (44.1k/48k), 001 = /1.5 (32k),
    // 010 = /2 (22.05k/24k), 011 = /3 (16k), 100 = /4 (11.025k/12k),
    // 101 = /5.5 (8.018k), 110 = /6 (8k).
    let (family, dacdiv) = match rate {
        48_000 => (PLL_48K, 0b000),
        44_100 => (PLL_44K1, 0b000),
        32_000 => (PLL_48K, 0b001),
        24_000 => (PLL_48K, 0b010),
        22_050 => (PLL_44K1, 0b010),
        16_000 => (PLL_48K, 0b011),
        12_000 => (PLL_48K, 0b100),
        11_025 => (PLL_44K1, 0b100),
        8_000 => (PLL_48K, 0b110),
        _ => return None,
    };
    Some(Clocks {
        n: family.0,
        k: family.1,
        dacdiv,
    })
}

/// The analogue output volume code for `db`, clamped to the codec's
/// range; below the range is the codec's mute.
fn out_code(db: i8) -> u8 {
    if db < OUT_MIN_DB {
        return 0;
    }
    (OUT_0DB as i8 + db.min(OUT_MAX_DB)) as u8
}

/// The codec, over a bus the caller provides.
#[derive(Debug)]
pub struct Wm8960<I2C> {
    i2c: I2C,
    address: u8,
    /// What the codec holds, by register: the interface cannot be read.
    regs: [u16; 56],
}

/// The register defaults after a reset (datasheet register map).
const DEFAULTS: [(u8, u16); 20] = [
    (0x00, 0x0A7),
    (0x01, 0x0A7),
    (DACCTL1, 0x008),
    (IFACE1, 0x00A),
    (0x08, 0x1C0),
    (LDAC, 0x0FF),
    (RDAC, 0x0FF),
    (0x11, 0x07B),
    (0x12, 0x100),
    (0x13, 0x032),
    (0x15, 0x0C3),
    (0x16, 0x0C3),
    (0x17, 0x1C0),
    (0x20, 0x100),
    (0x21, 0x100),
    (LOUTMIX, 0x050),
    (ROUTMIX, 0x050),
    (0x2A, 0x040),
    (0x2D, 0x050),
    (0x2E, 0x050),
];

/// The rest of the defaults, where they are not zero.
const DEFAULTS_HIGH: [(u8, u16); 6] = [
    (0x30, 0x002),
    (CLASSD1, 0x037),
    (0x33, 0x080),
    (PLL1, 0x008),
    (PLL2, 0x031),
    (PLL3, 0x026),
];

impl<I2C: I2c> Wm8960<I2C> {
    /// A codec at [`ADDRESS`].
    pub fn new(i2c: I2C) -> Self {
        Self::at(i2c, ADDRESS)
    }

    /// A codec at another address (the chip has only one; for a bus
    /// behind a translator).
    pub fn at(i2c: I2C, address: u8) -> Self {
        let mut codec = Self {
            i2c,
            address,
            regs: [0; 56],
        };
        codec.load_defaults();
        codec
    }

    /// Give the bus back.
    pub fn release(self) -> I2C {
        self.i2c
    }

    fn load_defaults(&mut self) {
        self.regs = [0; 56];
        for (reg, value) in DEFAULTS.iter().chain(DEFAULTS_HIGH.iter()) {
            self.regs[usize::from(*reg)] = *value;
        }
        self.regs[usize::from(PLL4)] = 0x0E9;
    }

    /// One control word: seven address bits, nine data bits.
    fn write(&mut self, reg: u8, value: u16) -> Result<(), I2C::Error> {
        let value = value & 0x1FF;
        self.i2c.write(
            self.address,
            &[(reg << 1) | ((value >> 8) as u8), value as u8],
        )?;
        self.regs[usize::from(reg)] = value;
        Ok(())
    }

    /// The bits of `mask` in `reg` set to `bits`, the rest as held.
    fn update(
        &mut self,
        reg: u8,
        mask: u16,
        bits: u16,
    ) -> Result<(), I2C::Error> {
        let held = self.regs[usize::from(reg)];
        self.write(reg, (held & !mask) | (bits & mask))
    }

    /// Reset every register to its default. This is also the probe: the
    /// codec acknowledges a write and nothing else, so `Ok` means a
    /// WM8960 (or something at its address that takes writes) is there,
    /// and a bus error means nothing is.
    pub fn reset(&mut self) -> Result<(), I2C::Error> {
        self.write(RESET, 0)?;
        self.load_defaults();
        Ok(())
    }

    /// Bring the bias up: anti-pop on, the reference at 2 × 50 kΩ. The
    /// caller waits [`OPEN_SETTLE_MS`] before [`Wm8960::start`].
    pub fn open(&mut self) -> Result<(), I2C::Error> {
        self.write(APOP1, POBCTRL | SOFT_ST | BUFDCOPEN | BUFIOEN)?;
        self.write(POWER1, VMID_50K)
    }

    /// Start the DAC path for a stream at `rate`: the reference on, the
    /// clocks, the interface, the DAC into both output mixers, the
    /// headphone and speaker amplifiers at 0 dB, the DAC unmuted. The
    /// level is left to the device (the DAC at 0 dB, the outputs at
    /// 0 dB) unless the volumes are set.
    pub fn start(
        &mut self,
        rate: u32,
        delay: &mut impl DelayNs,
    ) -> Result<(), Error<I2C::Error>> {
        let clocks = clocks_for(rate).ok_or(Error::Unsupported)?;
        self.update(POWER1, VREF, VREF)?;
        self.write(APOP1, BUFIOEN)?;
        self.write(IFACE1, IFACE_I2S_16_SLAVE)?;
        self.set_clocks(clocks, delay)?;
        // The DAC into the mixers, the mixers on.
        self.write(LOUTMIX, D2O)?;
        self.write(ROUTMIX, D2O)?;
        self.write(POWER3, LOMIX | ROMIX)?;
        // The DAC at 0 dB, both channels latched by the second write.
        self.write(LDAC, DAC_0DB)?;
        self.write(RDAC, VU | DAC_0DB)?;
        // The outputs at 0 dB, likewise.
        self.write(LOUT1, u16::from(OUT_0DB))?;
        self.write(ROUT1, VU | u16::from(OUT_0DB))?;
        self.write(LOUT2, u16::from(OUT_0DB))?;
        self.write(ROUT2, VU | u16::from(OUT_0DB))?;
        // DACs and amplifiers on, the PLL kept.
        self.update(
            POWER2,
            DACL | DACR | LOUT1_EN | ROUT1_EN | SPKL_EN | SPKR_EN,
            DACL | DACR | LOUT1_EN | ROUT1_EN | SPKL_EN | SPKR_EN,
        )?;
        self.write(CLASSD1, CLASSD_BOTH | CLASSD_FIXED)?;
        self.set_mute(false)?;
        Ok(())
    }

    /// Follow the stream to `rate`: muted while the PLL and the divider
    /// move, then unmuted. `Unsupported` leaves the codec as it was,
    /// muted.
    pub fn set_rate(
        &mut self,
        rate: u32,
        delay: &mut impl DelayNs,
    ) -> Result<(), Error<I2C::Error>> {
        self.set_mute(true)?;
        let clocks = clocks_for(rate).ok_or(Error::Unsupported)?;
        self.set_clocks(clocks, delay)?;
        self.set_mute(false)?;
        Ok(())
    }

    /// The PLL off, its ratio written, the PLL on and given its time,
    /// then SYSCLK from it through the divider: the vendor driver's
    /// order.
    fn set_clocks(
        &mut self,
        c: Clocks,
        delay: &mut impl DelayNs,
    ) -> Result<(), I2C::Error> {
        self.update(CLOCK1, CLKSEL_PLL, 0)?;
        self.update(POWER2, PLL_EN, 0)?;
        // PLLK is 24 bits over R53..R55, a byte each (the register
        // descriptions and the vendor driver; the map's summary row says
        // six bits for R53 and is contradicted by its own default).
        self.write(PLL2, ((c.k >> 16) & 0xFF) as u16)?;
        self.write(PLL3, ((c.k >> 8) & 0xFF) as u16)?;
        self.write(PLL4, (c.k & 0xFF) as u16)?;
        self.write(PLL1, SDM | PLLPRESCALE | u16::from(c.n))?;
        self.update(POWER2, PLL_EN, PLL_EN)?;
        delay.delay_ms(PLL_SETTLE_MS);
        self.write(
            CLOCK1,
            (u16::from(c.dacdiv) << DACDIV_SHIFT) | SYSCLKDIV_2 | CLKSEL_PLL,
        )
    }

    /// Soft-mute the DAC (the codec ramps).
    pub fn set_mute(&mut self, mute: bool) -> Result<(), I2C::Error> {
        self.update(DACCTL1, DACMU, if mute { DACMU } else { 0 })
    }

    /// The headphone amplifier's gain, −73 to +6 dB in 1 dB steps; below
    /// the range mutes it.
    pub fn set_headphone_db(&mut self, db: i8) -> Result<(), I2C::Error> {
        let code = u16::from(out_code(db));
        self.write(LOUT1, code)?;
        self.write(ROUT1, VU | code)
    }

    /// The speaker drivers' gain, −73 to +6 dB in 1 dB steps; below the
    /// range mutes them.
    pub fn set_speaker_db(&mut self, db: i8) -> Result<(), I2C::Error> {
        let code = u16::from(out_code(db));
        self.write(LOUT2, code)?;
        self.write(ROUT2, VU | code)
    }

    /// The DAC muted, the amplifiers and DACs off, the reference held at
    /// 2 × 250 kΩ for a quick return.
    pub fn suspend(&mut self) -> Result<(), I2C::Error> {
        self.set_mute(true)?;
        self.write(CLASSD1, CLASSD_FIXED)?;
        self.update(
            POWER2,
            DACL | DACR | LOUT1_EN | ROUT1_EN | SPKL_EN | SPKR_EN,
            0,
        )?;
        self.write(POWER1, VMID_250K | VREF)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embedded_hal_mock::eh1::delay::NoopDelay;
    use embedded_hal_mock::eh1::i2c::{Mock, Transaction};

    /// A control word for `reg` with the 9-bit `value`.
    fn w(reg: u8, value: u16) -> Transaction {
        Transaction::write(
            ADDRESS,
            vec![(reg << 1) | ((value >> 8) as u8), (value & 0xFF) as u8],
        )
    }

    #[test]
    fn the_clock_table_is_the_datasheets() {
        // The datasheet's example: 24 MHz prescaled to 12, 12.288 MHz out.
        let c = clocks_for(48_000).unwrap();
        assert_eq!((c.n, c.k, c.dacdiv), (8, 0x3126E9, 0));
        // 11.2896 MHz: R = 90.3168 / 12 = 7.5264.
        let c = clocks_for(44_100).unwrap();
        assert_eq!((c.n, c.k), (7, 0x86C226));
        let frac = f64::from(c.k) / f64::from(1u32 << 24);
        assert!((f64::from(c.n) + frac - 7.5264).abs() < 1e-6);
        // The DAC divider below the two base rates.
        assert_eq!(clocks_for(22_050).unwrap().dacdiv, 0b010);
        assert_eq!(clocks_for(32_000).unwrap().dacdiv, 0b001);
        assert_eq!(clocks_for(16_000).unwrap().dacdiv, 0b011);
        assert_eq!(clocks_for(8_000).unwrap().dacdiv, 0b110);
        // Nothing above 48 kHz.
        assert_eq!(clocks_for(88_200), None);
        assert_eq!(clocks_for(96_000), None);
        assert_eq!(clocks_for(192_000), None);
    }

    #[test]
    fn the_output_volume_codes_follow_the_scale() {
        assert_eq!(out_code(0), 0x79);
        assert_eq!(out_code(6), 0x7F);
        assert_eq!(out_code(9), 0x7F);
        assert_eq!(out_code(-73), 0x30);
        assert_eq!(out_code(-74), 0x00);
        assert_eq!(out_code(-20), 0x79 - 20);
    }

    #[test]
    fn a_reset_is_one_write_and_the_probe() {
        let mut i2c = Mock::new(&[w(RESET, 0)]);
        let mut codec = Wm8960::new(i2c.clone());
        codec.reset().unwrap();
        assert_eq!(codec.regs[usize::from(IFACE1)], 0x00A);
        assert_eq!(codec.regs[usize::from(CLASSD1)], 0x037);
        i2c.done();
    }

    #[test]
    fn open_then_start_is_the_vendor_power_path_for_the_dac() {
        let mut i2c = Mock::new(&[
            // open
            w(APOP1, 0x09C),
            w(POWER1, 0x080),
            // start: VREF, anti-pop off but the bias buffer, I2S 16 slave
            w(POWER1, 0x0C0),
            w(APOP1, 0x008),
            w(IFACE1, 0x002),
            // clocks for 44.1 kHz
            w(CLOCK1, 0x000),
            w(POWER2, 0x000),
            w(PLL2, 0x086),
            w(PLL3, 0x0C2),
            w(PLL4, 0x026),
            w(PLL1, 0x037),
            w(POWER2, 0x001),
            w(CLOCK1, 0x005),
            // the DAC into the mixers
            w(LOUTMIX, 0x100),
            w(ROUTMIX, 0x100),
            w(POWER3, 0x00C),
            // volumes at 0 dB
            w(LDAC, 0x0FF),
            w(RDAC, 0x1FF),
            w(LOUT1, 0x079),
            w(ROUT1, 0x179),
            w(LOUT2, 0x079),
            w(ROUT2, 0x179),
            // DACs and amplifiers on with the PLL, class D both
            w(POWER2, 0x1F9),
            w(CLASSD1, 0x0F7),
            // unmute
            w(DACCTL1, 0x000),
        ]);
        let mut codec = Wm8960::new(i2c.clone());
        codec.open().unwrap();
        codec.start(44_100, &mut NoopDelay).unwrap();
        i2c.done();
    }

    #[test]
    fn a_rate_change_is_muted_and_an_unsupported_rate_stays_muted() {
        let mut i2c = Mock::new(&[
            w(DACCTL1, 0x008),
            w(CLOCK1, 0x000),
            w(POWER2, 0x000),
            w(PLL2, 0x031),
            w(PLL3, 0x026),
            w(PLL4, 0x0E9),
            w(PLL1, 0x038),
            w(POWER2, 0x001),
            w(CLOCK1, 0x00D),
            w(DACCTL1, 0x000),
            // 96 kHz: mute, then nothing
            w(DACCTL1, 0x008),
        ]);
        let mut codec = Wm8960::new(i2c.clone());
        codec.set_rate(32_000, &mut NoopDelay).unwrap();
        assert_eq!(
            codec.set_rate(96_000, &mut NoopDelay),
            Err(Error::Unsupported)
        );
        i2c.done();
    }

    #[test]
    fn volumes_and_suspend_write_what_the_registers_want() {
        let mut i2c = Mock::new(&[
            w(LOUT1, 0x06D),
            w(ROUT1, 0x16D),
            w(LOUT2, 0x000),
            w(ROUT2, 0x100),
            w(DACCTL1, 0x008),
            w(CLASSD1, 0x037),
            w(POWER2, 0x000),
            w(POWER1, 0x140),
        ]);
        let mut codec = Wm8960::new(i2c.clone());
        codec.set_headphone_db(-12).unwrap();
        codec.set_speaker_db(-80).unwrap();
        codec.suspend().unwrap();
        i2c.done();
    }
}

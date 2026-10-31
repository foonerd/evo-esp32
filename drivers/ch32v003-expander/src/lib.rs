//! The CH32V003 I2C IO expander on Waveshare's ESP32 touch panels.
//!
//! A small RISC-V microcontroller running Waveshare's firmware, on the
//! board's shared I2C bus at `0x24`. It holds the lines the main chip
//! has no pins to spare for: on the ESP32-C5-Touch-LCD-2.8 the touch
//! reset (EXIO0), the panel reset (EXIO1), the RTC interrupt (EXIO2) and
//! the audio amplifier enable (EXIO3), plus the backlight as a PWM output
//! and one ADC input. Nothing lights before this device is spoken to.
//!
//! The register map is Waveshare's, read from the board repository
//! ([waveshareteam/ESP32-C5-Touch-LCD-2.8](https://github.com/waveshareteam/ESP32-C5-Touch-LCD-2.8)
//! at `77a7dce`, Apache-2.0, copyright Waveshare): `examples/arduino/01_exio/io_extension.cpp`
//! for the registers, the BSP `esp32_c5_touch_lcd_2.8.c` for the directions,
//! the default levels and the reset pulses. Transcribed, not copied.
//!
//! | Register | Function |
//! | --- | --- |
//! | `0x02` | direction, one bit per pin, 1 = output |
//! | `0x03` | output levels, one bit per pin |
//! | `0x04` | input levels, one bit per pin |
//! | `0x05` | PWM duty, 0 to 255 |
//! | `0x06` | ADC reading, two bytes little-endian |
//! | `0x07` | RTC interrupt flag |
//!
//! The driver is `no_std`, has no `unsafe`, and speaks only
//! `embedded_hal::i2c::I2c`. It keeps a shadow of the direction and output
//! registers because the device offers no read-back of either; a board
//! that shares the expander between drivers shares one instance of this.
#![cfg_attr(not(test), no_std)]

use embedded_hal::i2c::I2c;

/// The 7-bit address Waveshare's firmware answers on.
pub const ADDRESS: u8 = 0x24;

/// The expander's registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Reg {
    Mode = 0x02,
    Output = 0x03,
    Input = 0x04,
    Pwm = 0x05,
    Adc = 0x06,
    RtcInt = 0x07,
}

/// One of the eight expander lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pin {
    /// EXIO0.
    P0 = 0,
    /// EXIO1.
    P1 = 1,
    /// EXIO2.
    P2 = 2,
    /// EXIO3.
    P3 = 3,
    /// EXIO4.
    P4 = 4,
    /// EXIO5.
    P5 = 5,
    /// EXIO6.
    P6 = 6,
    /// EXIO7.
    P7 = 7,
}

impl Pin {
    /// The pin's bit in the direction, output and input registers.
    pub const fn mask(self) -> u8 {
        1 << (self as u8)
    }
}

/// The expander, over a bus the caller provides.
#[derive(Debug)]
pub struct Expander<I2C> {
    i2c: I2C,
    address: u8,
    mode: u8,
    output: u8,
}

impl<I2C: I2c> Expander<I2C> {
    /// An expander at [`ADDRESS`]. Nothing is sent until [`init`](Self::init).
    pub fn new(i2c: I2C) -> Self {
        Self::at(i2c, ADDRESS)
    }

    /// An expander at another address, should a board move it.
    pub fn at(i2c: I2C, address: u8) -> Self {
        Self {
            i2c,
            address,
            mode: 0,
            output: 0,
        }
    }

    /// Put the device in a known state: `outputs` driven, every output low.
    ///
    /// A board calls this once at boot with the pins it drives, then raises
    /// the resets it holds. Pins not in `outputs` are inputs.
    pub fn init(&mut self, outputs: u8) -> Result<(), I2C::Error> {
        self.set_directions(outputs)?;
        self.write_outputs(0)
    }

    /// Set every pin's direction at once: a 1 bit is an output.
    pub fn set_directions(&mut self, outputs: u8) -> Result<(), I2C::Error> {
        self.write(Reg::Mode, outputs)?;
        self.mode = outputs;
        Ok(())
    }

    /// Make `pin` an output, leaving the others as they are.
    pub fn set_output_dir(&mut self, pin: Pin) -> Result<(), I2C::Error> {
        self.set_directions(self.mode | pin.mask())
    }

    /// Make `pin` an input, leaving the others as they are.
    pub fn set_input_dir(&mut self, pin: Pin) -> Result<(), I2C::Error> {
        self.set_directions(self.mode & !pin.mask())
    }

    /// Drive `pin` high or low. The other outputs keep their levels.
    pub fn set(&mut self, pin: Pin, high: bool) -> Result<(), I2C::Error> {
        let next = if high {
            self.output | pin.mask()
        } else {
            self.output & !pin.mask()
        };
        self.write_outputs(next)
    }

    /// Drive every output at once from a bit mask.
    pub fn write_outputs(&mut self, levels: u8) -> Result<(), I2C::Error> {
        self.write(Reg::Output, levels)?;
        self.output = levels;
        Ok(())
    }

    /// The levels last written, as a bit mask.
    pub fn outputs(&self) -> u8 {
        self.output
    }

    /// Read `pin`'s level. Meaningful for inputs.
    pub fn get(&mut self, pin: Pin) -> Result<bool, I2C::Error> {
        Ok(self.read_inputs()? & pin.mask() != 0)
    }

    /// Read every input level at once.
    pub fn read_inputs(&mut self) -> Result<u8, I2C::Error> {
        self.read_u8(Reg::Input)
    }

    /// Set the PWM output's duty, 0 (off) to 255 (full).
    pub fn set_pwm(&mut self, duty: u8) -> Result<(), I2C::Error> {
        self.write(Reg::Pwm, duty)
    }

    /// Set the PWM output as a percentage, 0 to 100, clamped.
    pub fn set_pwm_percent(&mut self, percent: u8) -> Result<(), I2C::Error> {
        self.set_pwm(duty_for_percent(percent))
    }

    /// Read the ADC input, as the device reports it.
    pub fn read_adc(&mut self) -> Result<u16, I2C::Error> {
        let mut buf = [0u8; 2];
        self.i2c
            .write_read(self.address, &[Reg::Adc as u8], &mut buf)?;
        Ok(u16::from_le_bytes(buf))
    }

    /// Read the RTC interrupt flag register.
    pub fn read_rtc_int(&mut self) -> Result<u8, I2C::Error> {
        self.read_u8(Reg::RtcInt)
    }

    /// Give the bus back.
    pub fn release(self) -> I2C {
        self.i2c
    }

    fn write(&mut self, reg: Reg, value: u8) -> Result<(), I2C::Error> {
        self.i2c.write(self.address, &[reg as u8, value])
    }

    fn read_u8(&mut self, reg: Reg) -> Result<u8, I2C::Error> {
        let mut buf = [0u8; 1];
        self.i2c.write_read(self.address, &[reg as u8], &mut buf)?;
        Ok(buf[0])
    }
}

/// The duty byte for a percentage: the BSP's `percent * 255 / 100`.
pub const fn duty_for_percent(percent: u8) -> u8 {
    let p = if percent > 100 { 100 } else { percent } as u16;
    (p * 255 / 100) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use embedded_hal_mock::eh1::i2c::{Mock, Transaction};

    #[test]
    fn init_sets_directions_then_clears_outputs() {
        let mut i2c = Mock::new(&[
            Transaction::write(ADDRESS, vec![0x02, 0b0000_1011]),
            Transaction::write(ADDRESS, vec![0x03, 0x00]),
        ]);
        let mut exp = Expander::new(i2c.clone());
        exp.init(Pin::P0.mask() | Pin::P1.mask() | Pin::P3.mask())
            .unwrap();
        assert_eq!(exp.outputs(), 0);
        exp.release();
        i2c.done();
    }

    #[test]
    fn set_keeps_the_other_outputs() {
        let mut i2c = Mock::new(&[
            Transaction::write(ADDRESS, vec![0x03, 0b0000_0010]),
            Transaction::write(ADDRESS, vec![0x03, 0b0000_1010]),
            Transaction::write(ADDRESS, vec![0x03, 0b0000_1000]),
        ]);
        let mut exp = Expander::new(i2c.clone());
        exp.set(Pin::P1, true).unwrap();
        exp.set(Pin::P3, true).unwrap();
        exp.set(Pin::P1, false).unwrap();
        assert_eq!(exp.outputs(), Pin::P3.mask());
        exp.release();
        i2c.done();
    }

    #[test]
    fn direction_edits_keep_the_other_pins() {
        let mut i2c = Mock::new(&[
            Transaction::write(ADDRESS, vec![0x02, 0b0000_0001]),
            Transaction::write(ADDRESS, vec![0x02, 0b0000_0101]),
            Transaction::write(ADDRESS, vec![0x02, 0b0000_0100]),
        ]);
        let mut exp = Expander::new(i2c.clone());
        exp.set_output_dir(Pin::P0).unwrap();
        exp.set_output_dir(Pin::P2).unwrap();
        exp.set_input_dir(Pin::P0).unwrap();
        exp.release();
        i2c.done();
    }

    #[test]
    fn reads_inputs_adc_and_rtc() {
        let mut i2c = Mock::new(&[
            Transaction::write_read(ADDRESS, vec![0x04], vec![0b0000_0100]),
            Transaction::write_read(ADDRESS, vec![0x04], vec![0b0000_0100]),
            Transaction::write_read(ADDRESS, vec![0x06], vec![0x34, 0x12]),
            Transaction::write_read(ADDRESS, vec![0x07], vec![0x01]),
        ]);
        let mut exp = Expander::new(i2c.clone());
        assert!(exp.get(Pin::P2).unwrap());
        assert!(!exp.get(Pin::P0).unwrap());
        assert_eq!(exp.read_adc().unwrap(), 0x1234);
        assert_eq!(exp.read_rtc_int().unwrap(), 1);
        exp.release();
        i2c.done();
    }

    #[test]
    fn pwm_takes_a_duty_or_a_percentage() {
        let mut i2c = Mock::new(&[
            Transaction::write(ADDRESS, vec![0x05, 200]),
            Transaction::write(ADDRESS, vec![0x05, 255]),
            Transaction::write(ADDRESS, vec![0x05, 127]),
            Transaction::write(ADDRESS, vec![0x05, 0]),
        ]);
        let mut exp = Expander::new(i2c.clone());
        exp.set_pwm(200).unwrap();
        exp.set_pwm_percent(150).unwrap();
        exp.set_pwm_percent(50).unwrap();
        exp.set_pwm_percent(0).unwrap();
        exp.release();
        i2c.done();
    }

    #[test]
    fn duty_matches_the_bsp_arithmetic() {
        assert_eq!(duty_for_percent(100), 255);
        assert_eq!(duty_for_percent(50), 127);
        assert_eq!(duty_for_percent(1), 2);
        assert_eq!(duty_for_percent(0), 0);
    }

    #[test]
    fn another_address_is_honoured() {
        let mut i2c = Mock::new(&[Transaction::write(0x25, vec![0x05, 1])]);
        let mut exp = Expander::at(i2c.clone(), 0x25);
        exp.set_pwm(1).unwrap();
        exp.release();
        i2c.done();
    }

    #[test]
    fn a_bus_error_comes_back() {
        use embedded_hal::i2c::ErrorKind;
        let mut i2c =
            Mock::new(&[Transaction::write(ADDRESS, vec![0x03, 0x01])
                .with_error(ErrorKind::Bus)]);
        let mut exp = Expander::new(i2c.clone());
        assert!(exp.set(Pin::P0, true).is_err());
        assert_eq!(exp.outputs(), 0, "a failed write does not move the shadow");
        exp.release();
        i2c.done();
    }
}

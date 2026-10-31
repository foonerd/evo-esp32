//! The Hynitron CST3530 capacitive touch controller.
//!
//! On the Waveshare ESP32-C5-Touch-LCD-2.8 it sits on the shared I2C bus
//! at `0x58`, with its interrupt on GPIO5 and its reset on the IO
//! expander's EXIO0. It reports up to five fingers in the panel's native
//! 240x320 frame; the BSP configures no axis swap or mirror, so a report
//! is a [`Sample`] as it stands.
//!
//! The protocol is Waveshare's `esp_lcd_touch_cst3530` component, read
//! from the board repository
//! ([waveshareteam/ESP32-C5-Touch-LCD-2.8](https://github.com/waveshareteam/ESP32-C5-Touch-LCD-2.8)
//! at `77a7dce`, Apache-2.0, copyright Waveshare and Espressif Systems),
//! `examples/esp-idf/components/waveshare__esp_lcd_touch_cst3530/`.
//! Transcribed, not copied. Registers
//! are 32 bits wide and sent big-endian; a read is a four-byte write
//! followed by a separate read; every report is closed with a write to
//! the end-read register, or the controller stops updating.
//!
//! The report at `0xD0070000`, nine bytes:
//!
//! | Byte | Meaning |
//! | --- | --- |
//! | 3 | bits 0..3: number of fingers |
//! | 4 | X low byte |
//! | 5 | Y low byte |
//! | 6 | strength |
//! | 7 | bits 0..3: X high nibble; bits 4..7: Y high nibble |
//! | 8 | bits 0..3: finger id; bits 4..7: event, 1 press, 2 hold, 0 lift |
//!
//! Further fingers follow at `0xD0070900`, five bytes each, in the same
//! shape. This driver reads the first finger, which is what the face
//! uses; the count is reported alongside.
//!
//! `no_std`, no `unsafe`, `embedded_hal::i2c::I2c` only. The reset line is
//! the board's business: on this board it is behind the expander, and the
//! BSP holds it low for 100 ms.
#![cfg_attr(not(test), no_std)]

use embedded_hal::i2c::I2c;

/// The 7-bit address the controller answers on.
pub const ADDRESS: u8 = 0x58;

/// The report: count, first finger, event.
const DATA_REG: u32 = 0xD007_0000;
/// Written after every report, with no payload.
const END_READ_REG: u32 = 0xD000_02AB;

/// A finger's event, from the report's top nibble.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// The finger left.
    Lift,
    /// The finger arrived.
    Press,
    /// The finger is still down.
    Hold,
    /// A value the datasheet does not name.
    Other(u8),
}

impl Event {
    fn from_nibble(n: u8) -> Self {
        match n {
            0 => Self::Lift,
            1 => Self::Press,
            2 => Self::Hold,
            other => Self::Other(other),
        }
    }
}

/// One report as the controller sent it, before it becomes a sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    /// Fingers down, 0 to 5.
    pub fingers: u8,
    /// First finger's X in the native frame.
    pub x: u16,
    /// First finger's Y in the native frame.
    pub y: u16,
    /// First finger's strength as reported.
    pub strength: u8,
    /// First finger's track id.
    pub id: u8,
    /// First finger's event.
    pub event: Event,
}

impl Report {
    /// Decode the nine-byte report.
    pub fn parse(buf: &[u8; 9]) -> Self {
        Self {
            fingers: buf[3] & 0x0F,
            x: u16::from(buf[4]) | (u16::from(buf[7] & 0x0F) << 8),
            y: u16::from(buf[5]) | (u16::from(buf[7] & 0xF0) << 4),
            strength: buf[6],
            id: buf[8] & 0x0F,
            event: Event::from_nibble(buf[8] >> 4),
        }
    }

    /// True when a finger is on the panel: the vendor's test, a non-zero
    /// count and an event other than lift.
    pub fn touching(&self) -> bool {
        self.fingers != 0 && self.event != Event::Lift
    }
}

/// The controller, over a bus the caller provides.
#[derive(Debug)]
pub struct Cst3530<I2C> {
    i2c: I2C,
    address: u8,
}

impl<I2C: I2c> Cst3530<I2C> {
    /// A controller at [`ADDRESS`].
    pub fn new(i2c: I2C) -> Self {
        Self::at(i2c, ADDRESS)
    }

    /// A controller at another address.
    pub fn at(i2c: I2C, address: u8) -> Self {
        Self { i2c, address }
    }

    /// Read one report and close it.
    pub fn report(&mut self) -> Result<Report, I2C::Error> {
        let mut buf = [0u8; 9];
        self.i2c.write(self.address, &DATA_REG.to_be_bytes())?;
        self.i2c.read(self.address, &mut buf)?;
        self.i2c.write(self.address, &END_READ_REG.to_be_bytes())?;
        Ok(Report::parse(&buf))
    }

    /// Give the bus back.
    pub fn release(self) -> I2C {
        self.i2c
    }
}

/// A finger on the panel, in the panel's native frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    /// X, 0 to 239.
    pub x: u16,
    /// Y, 0 to 319.
    pub y: u16,
    /// The controller's strength figure.
    pub pressure: u8,
    /// Fingers down.
    pub fingers: u8,
}

impl<I2C: I2c> Cst3530<I2C> {
    /// One report as a sample: `None` when no finger is on the panel.
    pub fn read(&mut self) -> Result<Option<Sample>, I2C::Error> {
        let r = self.report()?;
        Ok(r.touching().then_some(Sample {
            x: r.x,
            y: r.y,
            pressure: r.strength,
            fingers: r.fingers,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embedded_hal_mock::eh1::i2c::{Mock, Transaction};

    const REG: [u8; 4] = [0xD0, 0x07, 0x00, 0x00];
    const END: [u8; 4] = [0xD0, 0x00, 0x02, 0xAB];

    fn report(
        fingers: u8,
        x: u16,
        y: u16,
        strength: u8,
        id: u8,
        event: u8,
    ) -> Vec<u8> {
        vec![
            0,
            0,
            0,
            fingers,
            (x & 0xFF) as u8,
            (y & 0xFF) as u8,
            strength,
            ((x >> 8) as u8 & 0x0F) | (((y >> 8) as u8 & 0x0F) << 4),
            (event << 4) | id,
        ]
    }

    fn exchange(body: Vec<u8>) -> Mock {
        Mock::new(&[
            Transaction::write(ADDRESS, REG.to_vec()),
            Transaction::read(ADDRESS, body),
            Transaction::write(ADDRESS, END.to_vec()),
        ])
    }

    #[test]
    fn a_press_is_a_sample() {
        let mut i2c = exchange(report(1, 0x0E5, 0x12C, 40, 0, 1));
        let mut t = Cst3530::new(i2c.clone());
        let s = t.read().unwrap().unwrap();
        assert_eq!((s.x, s.y, s.pressure, s.fingers), (229, 300, 40, 1));
        t.release();
        i2c.done();
    }

    #[test]
    fn a_lift_or_an_empty_count_is_none_and_still_closed() {
        let mut i2c = exchange(report(1, 10, 10, 5, 0, 0));
        let mut t = Cst3530::new(i2c.clone());
        assert_eq!(t.read().unwrap(), None);
        t.release();
        i2c.done();

        let mut i2c = exchange(report(0, 10, 10, 5, 0, 2));
        let mut t = Cst3530::new(i2c.clone());
        assert_eq!(t.read().unwrap(), None);
        t.release();
        i2c.done();
    }

    #[test]
    fn the_high_nibbles_split_as_the_vendor_says() {
        let buf: [u8; 9] = [0, 0, 0, 2, 0x34, 0x56, 9, 0xA1, 0x23];
        let r = Report::parse(&buf);
        assert_eq!(r.fingers, 2);
        assert_eq!(r.x, 0x134);
        assert_eq!(r.y, 0xA56);
        assert_eq!(r.strength, 9);
        assert_eq!(r.id, 3);
        assert_eq!(r.event, Event::Hold);
        assert!(r.touching());
    }

    #[test]
    fn unknown_events_are_kept_not_dropped() {
        assert_eq!(Event::from_nibble(7), Event::Other(7));
        let buf: [u8; 9] = [0, 0, 0, 1, 1, 1, 1, 0, 0x70];
        assert!(Report::parse(&buf).touching());
    }

    #[test]
    fn a_bus_error_comes_back() {
        use embedded_hal::i2c::ErrorKind;
        let mut i2c = Mock::new(&[Transaction::write(ADDRESS, REG.to_vec())
            .with_error(ErrorKind::Bus)]);
        let mut t = Cst3530::new(i2c.clone());
        assert!(t.read().is_err());
        t.release();
        i2c.done();
    }
}

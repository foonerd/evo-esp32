//! A FAT-formatted card on an SPI bus the panel shares.
//!
//! The ESP32-C5 has no SD/MMC host, so the card runs in SPI mode on the
//! same bus as the display with its own chip select; ESP-IDF's SD-SPI
//! host attaches to a bus the firmware has already initialised, and
//! FatFs appears under a VFS path so `std::fs` reads and writes it.
//! FatFs in ESP-IDF v5.5 reads FAT12/16/32 only: a card over 32 GB must
//! be formatted FAT32 by the user, exFAT is not compiled in.
//!
//! A missing or unformatted card is an `Err` from [`Card::mount`] and
//! nothing else: the firmware logs it and carries on without.

use std::fs;
use std::io;
use std::rc::Rc;

use esp_idf_svc::fs::fatfs::Fatfs;
use esp_idf_svc::hal::gpio::{AnyIOPin, OutputPin};
use esp_idf_svc::hal::sd::spi::SdSpiHostDriver;
use esp_idf_svc::hal::sd::{SdCardConfiguration, SdCardDriver};
use esp_idf_svc::hal::spi::SpiDriver;
use esp_idf_svc::io::vfs::MountedFatfs;
use esp_idf_svc::sys::EspError;

/// Where the card appears in the VFS.
pub const MOUNT_POINT: &str = "/sdcard";

type Mounted = MountedFatfs<
    Fatfs<SdCardDriver<SdSpiHostDriver<'static, Rc<SpiDriver<'static>>>>>,
>;

/// A card, mounted for as long as this lives.
pub struct Card {
    _mounted: Mounted,
    /// Bytes on the card, from the card's own capacity record.
    pub capacity: u64,
}

impl Card {
    /// Attach to `spi` with `cs`, initialise the card and mount its first
    /// FAT volume at [`MOUNT_POINT`]. Fails when there is no card, or no
    /// FAT volume on it.
    pub fn mount(
        spi: Rc<SpiDriver<'static>>,
        cs: impl OutputPin + 'static,
    ) -> Result<Self, EspError> {
        let host = SdSpiHostDriver::new(
            spi,
            Some(cs),
            AnyIOPin::none(),
            AnyIOPin::none(),
            AnyIOPin::none(),
            None,
        )?;
        // Default speed is 20 MHz; the panel runs 40 MHz on the same bus
        // and spi_master clocks each device at its own rate.
        let card = SdCardDriver::new_spi(host, &SdCardConfiguration::new())?;
        let capacity = {
            let c = card.card();
            c.csd.capacity as u64 * c.csd.sector_size as u64
        };
        let mounted =
            MountedFatfs::mount(Fatfs::new_sdcard(0, card)?, MOUNT_POINT, 4)?;
        // f_mount is lazy: touch the volume so a card without FAT fails here.
        fs::read_dir(MOUNT_POINT).map_err(|_| {
            EspError::from_infallible::<{ esp_idf_svc::sys::ESP_FAIL }>()
        })?;
        Ok(Self {
            _mounted: mounted,
            capacity,
        })
    }

    /// The whole of `name` at the card's root, or `None` when absent.
    pub fn read(&self, name: &str) -> io::Result<Option<String>> {
        match fs::read_to_string(self.path(name)) {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Replace `name` at the card's root with `text`.
    pub fn write(&self, name: &str, text: &str) -> io::Result<()> {
        fs::write(self.path(name), text)
    }

    fn path(&self, name: &str) -> String {
        format!("{MOUNT_POINT}/{name}")
    }
}

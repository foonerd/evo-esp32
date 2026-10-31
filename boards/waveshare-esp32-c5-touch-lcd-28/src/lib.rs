//! The Waveshare ESP32-C5-Touch-LCD-2.8: pins, bring-up order, and the
//! parts a firmware needs. A library, so a firmware stands on the board
//! rather than carrying it.
//!
//! The facts are the schematic's and the vendor BSP's (board repository
//! `examples/esp-idf/components/esp32_c5_touch_lcd_2_8`):
//!
//! | Part | Where |
//! | --- | --- |
//! | I2C (expander, touch, codec, IMU, RTC; the HAT header) | SDA GPIO0, SCL GPIO1, 100 kHz |
//! | CH32V003 expander | I2C `0x24`: EXIO0 touch reset, EXIO1 panel reset, EXIO3 amp enable, PWM backlight |
//! | SPI2, shared by the panel and the TF card | SCK GPIO6, MOSI GPIO7, MISO GPIO8 |
//! | ST7789 panel | SPI2 CS GPIO10, DC GPIO9; RGB565, inverted |
//! | TF card | SPI2 CS GPIO23, SPI mode (the C5 has no SD/MMC host) |
//! | CST3530 touch | I2C `0x58`, INT GPIO5, reset on EXIO0 |
//!
//! The order is the BSP's: the expander up with its outputs high, the
//! panel reset pulsed 200 ms low and 200 ms settled, the panel
//! initialised, the touch reset pulsed 10 ms low and then waited out
//! until the controller answers on the bus, the backlight last so
//! nothing shows before the first frame. The SPI bus is made
//! before any of it ([`spi_bus`]) so the card can be read first.

use std::rc::Rc;
use std::sync::Mutex;

use ch32v003_expander::{Expander, Pin as Exio};
use cst3530::Cst3530;
use embedded_hal_bus::i2c::MutexDevice;
use esp_idf_svc::hal::delay::FreeRtos;
use esp_idf_svc::hal::gpio::{
    Gpio0, Gpio1, Gpio10, Gpio5, Gpio6, Gpio7, Gpio8, Gpio9, Input, Output,
    PinDriver, Pull,
};
use esp_idf_svc::hal::i2c::{I2cConfig, I2cDriver, I2cError, I2C0};
use esp_idf_svc::hal::spi::{
    config::{Config as SpiConfig, DriverConfig},
    Dma, SpiDeviceDriver, SpiDriver, SPI2,
};
use esp_idf_svc::hal::units::FromValueType;
use esp_idf_svc::sys::EspError;
use mipidsi::interface::SpiInterface;
use mipidsi::models::ST7789;
use mipidsi::options::{ColorInversion, ColorOrder, Orientation, Rotation};
use mipidsi::{Builder, NoResetPin};

/// SPI clock to the panel. The BSP runs 80 MHz; half that until the
/// first frames on the bench say the wiring takes more.
const PANEL_HZ: u32 = 40_000_000;

/// The longest the touch controller is given to answer after its reset.
const TOUCH_BOOT_MS: u32 = 500;

/// The expander lines this board drives.
const TOUCH_RST: Exio = Exio::P0;
const PANEL_RST: Exio = Exio::P1;
const AMP_EN: Exio = Exio::P3;

/// The I2C bus, shared by the expander and the touch controller for the
/// life of the firmware, and handed out so a firmware can put the codec
/// or a DAC on it too (`MutexDevice::new(board.bus)`). Behind a mutex,
/// not a `RefCell`, so the touch controller can be read from a thread of
/// its own while the main task drives the expander.
pub type Bus = &'static Mutex<I2cDriver<'static>>;

/// A device on [`Bus`].
pub type Device = MutexDevice<'static, I2cDriver<'static>>;

/// The SPI bus, shared by the panel and the card for the life of the
/// firmware.
pub type Spi = Rc<SpiDriver<'static>>;

/// The panel as a `DrawTarget`.
pub type Panel = mipidsi::Display<
    SpiInterface<
        'static,
        SpiDeviceDriver<'static, Spi>,
        PinDriver<'static, Output>,
    >,
    ST7789,
    NoResetPin,
>;

/// What bring-up hands back.
pub struct Board {
    /// The panel, oriented as the settings ask.
    pub panel: Panel,
    /// The touch controller on the shared bus.
    pub touch: Cst3530<Device>,
    /// The touch interrupt line: the controller pulls it low for each
    /// report it has ready.
    pub touch_int: PinDriver<'static, Input>,
    /// The expander, for the backlight and the amplifier.
    pub expander: Expander<Device>,
    /// The I2C bus the expander and touch controller are on, for more.
    pub bus: Bus,
}

/// The peripherals a bring-up consumes, each moved in by name so the
/// compiler knows it is used once. The modem goes to the network and the
/// SPI pins to [`spi_bus`] before this.
pub struct Pins {
    /// `I2C0`.
    pub i2c0: I2C0<'static>,
    /// SDA.
    pub sda: Gpio0<'static>,
    /// SCL.
    pub scl: Gpio1<'static>,
    /// Panel DC.
    pub dc: Gpio9<'static>,
    /// Panel CS.
    pub cs: Gpio10<'static>,
    /// Touch interrupt.
    pub touch_int: Gpio5<'static>,
}

/// The SPI bus with DMA, before anything is on it.
pub fn spi_bus(
    spi2: SPI2<'static>,
    sck: Gpio6<'static>,
    mosi: Gpio7<'static>,
    miso: Gpio8<'static>,
) -> Result<Spi, EspError> {
    Ok(Rc::new(SpiDriver::new(
        spi2,
        sck,
        mosi,
        Some(miso),
        &DriverConfig::new().dma(Dma::Auto(4096)),
    )?))
}

/// Bring the board up in the BSP's order and return its parts.
pub fn bring_up(
    pins: Pins,
    spi: Spi,
    rotation: u16,
) -> Result<Board, BringUpError> {
    // The bus, leaked: it lives as long as the firmware, and both the
    // expander and the touch controller borrow it. 100 kHz, not the
    // BSP's 400: the bus also leaves the board for the HAT header over
    // wires, and a WM8960 there wants SCL low for 1.3 µs (its datasheet),
    // which 400 kHz does not give — one write in thirty went
    // unacknowledged on the bench. Every part here is within spec at 100.
    let bus: Bus = Box::leak(Box::new(Mutex::new(I2cDriver::new(
        pins.i2c0,
        pins.sda,
        pins.scl,
        &I2cConfig::new().baudrate(100.kHz().into()),
    )?)));

    let mut expander = Expander::new(MutexDevice::new(bus));
    expander.init(TOUCH_RST.mask() | PANEL_RST.mask() | AMP_EN.mask())?;
    expander
        .write_outputs(TOUCH_RST.mask() | PANEL_RST.mask() | AMP_EN.mask())?;
    log::info!("expander: up, outputs high");

    expander.set(PANEL_RST, false)?;
    FreeRtos::delay_ms(200);
    expander.set(PANEL_RST, true)?;
    FreeRtos::delay_ms(200);

    let device = SpiDeviceDriver::new(
        spi,
        Some(pins.cs),
        &SpiConfig::new().baudrate(PANEL_HZ.Hz()).write_only(true),
    )?;
    let dc = PinDriver::output(pins.dc)?;
    let buffer: &'static mut [u8] =
        Box::leak(vec![0u8; 4096].into_boxed_slice());
    let di = SpiInterface::new(device, dc, buffer);
    let panel = Builder::new(ST7789, di)
        .display_size(240, 320)
        .color_order(ColorOrder::Rgb)
        .invert_colors(ColorInversion::Inverted)
        .orientation(Orientation::new().rotate(match rotation {
            90 => Rotation::Deg90,
            180 => Rotation::Deg180,
            270 => Rotation::Deg270,
            _ => Rotation::Deg0,
        }))
        .init(&mut FreeRtos)
        .map_err(|e| BringUpError::Panel(format!("{e:?}")))?;
    log::info!("panel: initialised at {} Hz, rotation {rotation}", PANEL_HZ);

    expander.set(TOUCH_RST, false)?;
    FreeRtos::delay_ms(10);
    expander.set(TOUCH_RST, true)?;
    let mut touch = Cst3530::new(MutexDevice::new(bus));
    // The controller boots after reset and is not to be spoken to, nor
    // the bus used, until it answers; a codec written to in that window
    // got a bus timeout on the bench.
    let mut waited = 0u32;
    loop {
        FreeRtos::delay_ms(10);
        waited += 10;
        if touch.report().is_ok() {
            break;
        }
        if waited >= TOUCH_BOOT_MS {
            return Err(BringUpError::Touch(waited));
        }
    }
    let touch_int = PinDriver::input(pins.touch_int, Pull::Up)?;
    log::info!("touch: reset, answering after {waited} ms, interrupt on GPIO5");

    Ok(Board {
        panel,
        touch,
        touch_int,
        expander,
        bus,
    })
}

impl Board {
    /// Light the backlight, 0 to 100.
    pub fn backlight(&mut self, percent: u8) -> Result<(), I2cError> {
        self.expander.set_pwm_percent(percent)
    }
}

/// Why the board did not come up.
#[derive(Debug)]
pub enum BringUpError {
    /// A driver refused.
    Esp(EspError),
    /// The expander or the touch controller did not answer on the bus.
    I2c(I2cError),
    /// The panel did not initialise.
    Panel(String),
    /// The touch controller did not answer within this many milliseconds
    /// of its reset.
    Touch(u32),
}

impl std::fmt::Display for BringUpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Esp(e) => write!(f, "{e}"),
            Self::I2c(e) => write!(f, "i2c: {e}"),
            Self::Panel(e) => write!(f, "panel: {e}"),
            Self::Touch(ms) => {
                write!(f, "touch: no answer {ms} ms after reset")
            }
        }
    }
}

impl std::error::Error for BringUpError {}

impl From<EspError> for BringUpError {
    fn from(e: EspError) -> Self {
        Self::Esp(e)
    }
}

impl From<I2cError> for BringUpError {
    fn from(e: I2cError) -> Self {
        Self::I2c(e)
    }
}

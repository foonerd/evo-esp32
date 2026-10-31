# ch32v003-expander

The CH32V003 I2C IO expander Waveshare fits to its ESP32 touch panels, running their firmware: eight GPIO lines, one PWM output and one ADC input behind address `0x24`.

On the ESP32-C5-Touch-LCD-2.8 it holds what the main chip has no pins for:

| Line | Function |
| --- | --- |
| EXIO0 | touch reset |
| EXIO1 | panel reset |
| EXIO2 | RTC interrupt |
| EXIO3 | audio amplifier enable |
| PWM | backlight |

Nothing lights before this device is spoken to, so a firmware brings it up first: `init` with the pins it drives, then the resets raised, then the panel and the touch controller reset through it, then the backlight.

```rust
let mut exp = ch32v003_expander::Expander::new(i2c);
exp.init(Pin::P0.mask() | Pin::P1.mask() | Pin::P3.mask())?;
exp.set(Pin::P1, false)?; delay.delay_ms(200); exp.set(Pin::P1, true)?; delay.delay_ms(200);
exp.set_pwm_percent(100)?;
```

The register map and the sequences are Waveshare's, from the board repository: `examples/arduino/01_exio/io_extension.cpp` for the registers, the BSP for the directions, the default levels and the reset pulses (200 ms each side for the panel, 10 ms for the touch). The device offers no read-back of its direction or output registers, so the driver keeps a shadow of both; a board that shares the expander between drivers shares one instance.

Verified: against the vendor's sources and embedded-hal-mock on the host. Not yet on a board; the first bench run records what the device answers.

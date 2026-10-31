# cst3530

The Hynitron CST3530 capacitive touch controller, as Waveshare's ESP32 touch panels carry it: address `0x58`, up to five fingers, reports in the panel's native 240x320 frame with no axis swap or mirror.

The crate implements `companion_hal::TouchSource`: one `read` returns the first finger as a `TouchSample`, or `None` on a lift or an empty report. The raw `Report` (count, first finger, strength, track id, event) is available for a device that wants more.

```rust
let mut touch = cst3530::Cst3530::new(i2c);
// when the interrupt line falls:
match touch.read()? {
    Some(sample) => app.touch(Some(sample), now, &mut http),
    None => app.touch(None, now, &mut http),
}
```

The protocol is Waveshare's `esp_lcd_touch_cst3530` component from the board repository: 32-bit registers sent big-endian, a four-byte write then a separate read, and a write to the end-read register after every report, without which the controller stops updating. The reset line is the board's business; on the ESP32-C5-Touch-LCD-2.8 it is the IO expander's EXIO0, held low for 100 ms by the BSP.

Verified: against the vendor's source and embedded-hal-mock on the host. Not yet on a board; the first bench run records the report shape as the controller sends it.

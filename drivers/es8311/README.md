# es8311

The Everest ES8311 audio codec, as Waveshare's ESP32 touch panels carry it: address `0x18`, mono, fed over I2S, with no MCLK wired, so the codec makes its master clock from the bit clock. The DAC path only; the ADC and master mode are not here.

```rust
let mut codec = es8311::Es8311::new(i2c);
codec.open()?;                       // once after power
codec.set_format(44_100, 16)?;       // the stream's rate and width
codec.start()?;
expander.set(Pin::P3, true)?;        // the amplifier, on this board
codec.set_mute(false)?;
codec.set_volume(es8311::percent_to_half_db(40, -7))?;
```

The I2S side is the device's: two 16-bit slots, so the bit clock is 32 times the sample rate (`bit_clock_for`); at 8 kHz the codec wants 32-bit slots. Rates in the clock table: 8, 11.025, 12, 16, 22.05, 24, 32, 44.1, 48, 64 and 96 kHz; anything else is refused before a register is touched.

Volume is set in half-decibels at the DAC (`0xBF` is 0 dB); `percent_to_half_db` is the reference's 0 to 100 curve with the board's hardware gain taken off, `-7` half-decibels for a 3.3 V DAC into a 5 V amplifier.

The register sequences are Espressif's `esp_codec_dev` (the driver the vendor's BSP uses for this board), in the configuration the BSP gives it: slave, no MCLK, DAC mode, nothing inverted. Transcribed, not copied.

Verified: against the reference's source and embedded-hal-mock on the host. Not yet on a board; the first bench run reads the chip ID and hears the tone.

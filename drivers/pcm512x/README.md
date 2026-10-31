# pcm512x

The TI PCM5121/PCM5122 stereo DAC as Raspberry Pi HATs carry it (IQaudIO DAC+, HiFiBerry DAC+, Waveshare's PCM5122 Audio Board (A) and their kin): I2C at `0x4C` (`0x4D` on HiFiBerry's), I2S from the header, no master clock. The chip's PLL runs from BCK and its clock dividers set themselves from BCK and LRCK, so the host says nothing about the sample rate.

```rust
let mut dac = pcm512x::Pcm512x::new(i2c);          // 0x4C; ::at(i2c, 0x4D) for a DAC+
match dac.detect()? {
    Some(id) => log::info!("PCM512x, register 12 {:#04x}", id.master_mode),
    None => return Err(NotThisChip),
}
dac.init(pcm512x::WordLength::Sixteen)?;   // once after power; ends unmuted at 0 dB
dac.set_attenuation(12)?;                   // half-decibels below 0 dB: -6 dB
dac.set_mute(true)?;                        // register 3; the chip ramps
let c = dac.clocks()?;                      // registers 4, 91, 95, raw, for a log line
```

The register sequence is the Linux `pcm512x` codec driver's for a clock-consumer DAC with no SCLK — what every Raspberry Pi runs for these boards: reset (register 1), standby (2), a missing SCK is not an error (37, IDCH), PLL reference BCK (13), BCK and LRCK as inputs (9), I2S and the word length (40, 41), both channels at 0 dB (61, 62), unmuted (3), out of standby (2). The clock dividers are left to the chip's auto-set (37, DCAS stays 0). The data sheet (SLAS763C, 8.3.6.3, Table 33) takes BCK at 32 or 64 times the sample rate, 32 kHz to 384 kHz.

Detection reads two registers the driver never changes — 12 (`0x7C` in its upper six bits) and 42 (`0x11`), the chip's reset values — because the PCM512x has no identity register; they hold through this driver's configuration, so a warm restart finds the chip too.

`no_std`, no `unsafe`, `embedded_hal::i2c::I2c` only; six tests against embedded-hal-mock on exact transactions; built for bare-metal RISC-V in `check.sh`. First board: Waveshare's PCM5122 Audio Board (A) on the C5 panel's header, 16-bit I2S at 32 fs.

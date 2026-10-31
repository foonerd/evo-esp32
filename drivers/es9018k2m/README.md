# es9018k2m

The ESS ES9018K2M stereo DAC as Raspberry Pi HATs carry it (LightSABRE, Audiophonics I-Sabre K2M and their kin): I2C at `0x48`, I2S from the header, the HAT's own oscillator. The host asks for nothing clever: the chip auto-detects the serial format and runs its ASRC and jitter eliminator; the driver detects it, runs the bring-up, and sets volume and mute.

```rust
let mut dac = es9018k2m::Es9018k2m::new(i2c);
match dac.detect()? {
    Some(id) => log::info!("ES9018K2M revision {}", id.revision),
    None => return Err(NotThisChip),
}
dac.init()?;             // once after power; ends unmuted at 0 dB
dac.set_volume(60)?;     // 0 to 100, the plugin's curve; 0 mutes
dac.set_mute(true)?;     // register 7's bit, the filter bits untouched
```

The register sequence is the one [es9018k2m-plugin](https://github.com/foonerd/es9018k2m-plugin) (MIT, Just a Nerd) runs on Volumio players: detection by the status register (bits 4..2 read `100`), the init writes in its order, 0.5 dB attenuation registers for volume (`0x00` is 0 dB, `0x63` the -49.5 dB floor, `0xFF` mute). The plugin's filter choices and graceful ramps are not here yet.

`no_std`, no `unsafe`, `embedded_hal::i2c::I2c` only; four tests against embedded-hal-mock on exact transactions; built for bare-metal RISC-V in `check.sh`. Not yet on a board: the first bench run is a LightSABRE on the C5's header.

# Waveshare ESP32-C5-Touch-LCD-2.8

The player's firmware for its first board. Today it makes a sound and nothing else: on boot it brings the IO expander up with the amplifier enabled, reads the ES8311's chip ID, opens the codec for 44.1 kHz 16-bit, starts I2S0 as master on the vendor BSP's pins, and plays a 1 kHz tone for two seconds every ten through the board's speaker, logging each with the time the write took. Paced by the I2S clock, two seconds of audio take two seconds to write; that figure is the check that the clock is right before a decoder is asked for frames.

| Part | Where |
| --- | --- |
| I2C (expander, codec) | SDA GPIO0, SCL GPIO1, 400 kHz, one bus shared through `embedded-hal-bus` |
| CH32V003 expander | I2C `0x24`; EXIO3 enables the NS4150B amplifier |
| ES8311 codec | I2C `0x18`; no MCLK, clocked from the I2S bit clock; the companion's `es8311` driver |
| I2S0 | master, Philips, 16-bit stereo slots; BCLK GPIO24, WS GPIO25, data to the codec GPIO26 |
| I2S0, mirrored for a Pi-HAT DAC | the same three signals through the GPIO matrix: BCLK GPIO2 (J8 pin 7), WS GPIO3 (J8 pin 8), data GPIO11 (J6 TXD); the console is on USB so J6 is free. The speaker and the HAT play the same frames. |

Wiring a HAT, with pin 1 as the silkscreen marks it: J8.7 → HAT pin 12 (BCLK), J8.8 → HAT pin 35 (LRCLK), J6 TXD → HAT pin 40 (DIN), J8.1 or J6 GND → HAT pin 6, J8.2 (USB 5 V) → HAT pin 2. A bare PCM5102A module takes the same three signals plus VIN 5 V and GND, with its SCK pin tied to GND.

The build is the companion's: ESP-IDF v5.5.5 through esp-idf-svc 0.53, nightly-2026-10-17 with `build-std`, `ldproxy`, `partitions.csv` with a 3 MB app partition, `scripts/image.sh` for the flashable image. The companion's drivers come by its tag.

```sh
cd firmware/waveshare-esp32-c5-touch-lcd-28
cargo build --release
cd ../.. && scripts/image.sh firmware/waveshare-esp32-c5-touch-lcd-28
esptool --chip esp32c5 --port PORT write-flash 0x0 dist/evo-esp32-waveshare-esp32-c5-touch-lcd-28-0.1.0.bin
```

Known of the chip: no FPU. Software `sin` costs about 95 µs a call, so the tone is one exact cycle (441 samples) tiled, not 88 200 sines; the first build did the latter and the idle task's watchdog said so.

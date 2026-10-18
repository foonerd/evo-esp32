# Waveshare ESP32-C5-Touch-LCD-2.8

The player's firmware for its first board, and the fabric's first run on a chip. On boot it brings the IO expander up with the amplifier enabled, opens the ES8311, starts I2S0 as master on the vendor BSP's pins, mirrors the three signals onto header pins a Pi-HAT DAC can reach, brings up an ES9018K2M if one answers on the HAT's I2C, and plays half a second of 1 kHz as the self-test. Then it mounts the card, stands the fabric up as the host distribution does (the catalogue compiled in and checked against the plugins, the files source over the card, the Symphonia decoder, the null output) and plays every track on the card once, shuffled, through the I2S sink, with the face on the panel (`distributions/panel`, the consumer the host draws as a PNG) refreshed every 40 ms; every happening goes to the log, and a heartbeat every ten seconds says what the heap and the main stack have left. A card with no music gets a two-second `tone.wav` written to it first, so the whole path is proven on any card.

The sink (`src/sink.rs`) is I2S0 with its DMA as the buffer: the decoder offers frames, the sink hands the DMA what fits with a zero timeout and says how many frames that was, so the I2S clock paces playback and a tick never waits. The ring is sized in time, a quarter second at the stream's rate within 64 KB of internal SRAM (11 buffers at 44.1 kHz, 16 at 96 kHz); a stream at another rate has the channel rebuilt and the codec told. The heartbeat says how busy the core is, what SRAM is left, where the playback position stands against the clock (1.000 is keeping up) and whether the DMA ran dry. Measured: 96 kHz/24-bit FLAC decodes at 37-58% of the core and plays at 1.000; the face costs 1-6%; MP3 through Espressif's fixed-point decoder (`platform/esp-idf`, preferred in the codec registry because the C5 has no FPU and Symphonia's floating-point MP3 reached 0.555 of real time) plays at 1.000 with the core 23-25% busy; 192 kHz FLAC decodes at 0.38 of real time and is beyond this chip.

| Part | Where |
| --- | --- |
| The board | the companion's `boards/waveshare-esp32-c5-touch-lcd-28` crate: I2C (SDA GPIO0, SCL GPIO1, 400 kHz) with the CH32V003 expander at `0x24` (EXIO3 enables the NS4150B amplifier), the ST7789 panel over SPI2 at 40 MHz, the CST3530 touch, the backlight |
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

Known of ESP-IDF: below one scheduler tick, `usleep()` spins instead of yielding, so at the default 100 Hz every sleep under 10 ms starves the idle task and its watchdog bites five seconds later. `sdkconfig.defaults` sets `CONFIG_FREERTOS_HZ=1000`; the player's loop is paced in milliseconds. The main task's stack is 48 KB; the heartbeat reports its low-water mark (45 KB free with the decoder running: Symphonia's state lives on the heap).

# Changelog

All notable changes to evo-esp32 are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versions
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

- `firmware/waveshare-esp32-c5-touch-lcd-28`: the first firmware, and the
  first sound. std Rust on ESP-IDF v5.5.5 (the companion's pin and build
  files); the IO expander and the ES8311 through the companion's drivers
  by its tag v0.1.0; I2S0 as master in Philips format, 16-bit stereo, on
  the vendor BSP's pins (BCLK GPIO24, WS GPIO25, data GPIO26, no MCLK);
  a 1 kHz tone for two seconds every ten through the board's speaker,
  each logged with the time the write took. Bench: chip ID `83 11 01`,
  352 800 bytes in 1 972 ms against 2 000 expected. Found on the way: the
  C5 has no FPU, and 88 200 software sines took eight seconds and tripped
  the idle task's watchdog; the tone is one exact cycle, tiled.
- `scripts/image.sh` from the companion, naming the image by the crate;
  `scripts/check.sh` builds every firmware crate with clippy and drops an
  esp-idf-sys CMake cache made against another ESP-IDF version; CI gains
  the companion's toolchain steps and uploads the image.

- `evo-esp32-sdk`: `contract::audio::playback::FrameSink`, where decoded
  frames go: the distribution owns the sink and hands it to the decoder;
  `start`, non-blocking `write` that says how much it took, `flush`,
  `stop`. Frames do not travel through the fabric's values.
- `org.evoframework.playback.symphonia` (Apache-2.0, std): the real
  decoder behind `playback/decode`, FLAC, MP3, WAV and PCM through
  Symphonia 0.6; a bounded slice per tick, the sink pacing; position from
  frames the sink accepted; seek to the frame, trimming after the
  reader's packet boundary; duration from the container. 6 tests through
  the steward.
- `evo-esp32-host`: the Symphonia warden replaces the simulated one; a
  paced sink (`sink.rs`) that holds 200 ms, drains at the sample rate and
  records to `--wav`; `--tone` writes a test tone. The check decodes the
  tone end to end and requires the recorded WAV to be the tone's size to
  the byte; FLAC and WAV come out bit-exact, MP3 as lossy allows. CI
  uploads the heard WAV.
- `evo-esp32-sdk`: `contract::audio`, the player's slot contracts from
  evo-device-audio's vocabulary: shelves `source/files`, `source/radio`,
  `playback/decode`, `delivery/output`, `metadata/tags`,
  `artwork/embedded`; `Track`; the verbs and their argument types; the
  playback warden's `Play` instruction, `Correction` and `State`;
  `to_value` / `from_value`. Presentation is a consumer, not a shelf.
- Plugins (Apache-2.0): `org.evoframework.source.files` (a directory on a
  host, std), `org.evoframework.playback.sim` (plays against the clock,
  `no_std`), `org.evoframework.delivery.null` (level and mute, no sink,
  `no_std`). 11 tests through the steward.
- `evo-esp32-host` (`distributions/host/`): the catalogue compiled in by
  `build.rs` and checked against the admitted plugins at start; a track
  listed, played to its end and released, projections and happenings
  printed; the panel as a consumer, drawing the companion's resting face
  from the playback and delivery projections through `companion-core`
  (by commit) and writing the last playing frame as a PNG. `check.sh`
  runs it once, keeps the PNG under `target/` (CI uploads it) and builds
  the two `no_std` plugins for bare-metal RISC-V.
- `evo-esp32-sdk` (Apache-2.0): the surface a plugin implements against.
  The catalogue as data (racks, shelves, shapes; validated), the plugin
  contract (describe, load, unload, health), respondents and wardens,
  subjects announced under an external addressing, happenings on a
  numbered stream, projections keyed to a rack or a subject, the context
  that is the only handle a plugin holds. `no_std` with `alloc`.
- `evo-esp32-steward`: admission against the catalogue with shape
  equality, subject tables keeping each plugin's contribution apart and
  composing them in admission order, a bounded happenings ring whose
  replay reports a gap, projections, custody of wardens with a tick the
  caller drives. `no_std` with `alloc`.
- `evo-esp32-catalogue`: parse and validate `catalogue.toml`, check a
  plugin set stocks it, emit the catalogue as Rust; a binary for a look by
  hand.
- `org-evoframework-example-echo` (Apache-2.0): the pedagogical plugin,
  driven through the steward and the catalogue tool in its tests.
- `scripts/check.sh`: format, clippy, tests, docs, the three `no_std`
  crates built for bare-metal RISC-V, shellcheck; refuses without the ESP
  toolchain once a firmware crate exists.
- The repository: what it is, how it fits with evo-core,
  evo-device-audio and volumio-esp-companion, the licence set (Business
  Source License 1.1 for the fabric, Apache 2.0 for the surface others
  implement), the trademark policy and the contribution flow.

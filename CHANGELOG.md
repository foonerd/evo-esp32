# Changelog

All notable changes to evo-esp32 are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versions
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

- `plugins/org.evoframework.playback.symphonia`: MPEG audio (MP1, MP2,
  MP3) is seeked coarsely. Symphonia's accurate seek walks every frame
  from the start (or the current position, forward) to the target, which
  off the card ran at about 25 times real time: 3-13 s for a tap on the
  ring. Coarse has the reader estimate the byte position from the time
  and the stream's length, resync on the next frame and walk the last
  stretch. Exact on a constant bit rate; on a variable one the landing is
  off by the stream's drift from its mean rate. Other formats stay
  accurate (FLAC seeks by its table in 0.3 s). On the host: forward,
  back and near the end of a 4-minute MP3, each landing on the packet
  before the target with up to 76 ms to drop. On the board: twenty seeks
  in a minute, forward and back across whole tracks, none long enough to
  run the ring dry (under 231 ms each); the position is moving again
  within 70 ms of the tap.
- `distributions/face`: a full frame is painted in stages, one per
  `refresh` (`Repaint::Partial` until the last returns `Repaint::Full`;
  `refresh_whole` for a host that wants the frame in one call), so the
  caller's loop runs between them. The placeholder disc and the progress
  ring are composed per pixel and sent one window a row instead of as a
  stack of shapes each with its own edge windows; the frames are pixel
  for pixel what they were (the gallery compared). On the board a window
  costs about 70 µs: the disc as twelve rings took 433 ms, composed 70 ms;
  the ring with its accent 149 ms, composed 73 ms. The first frame is
  421 ms end to end, the longest stage 73 ms.
- `firmware/waveshare-esp32-c5-touch-lcd-28`: the face's stages run on
  consecutive passes of the loop with a decoder tick between, and the
  panel lights on the first complete frame. A track start no longer runs
  the DMA dry (it did by 500 ms, the face's first frame on top of a ring
  of 159-231 ms); five track starts in a capture, none dry. The first
  frame's stage times are logged with "panel lit".
- `firmware/waveshare-esp32-c5-touch-lcd-28`: the empty-card tone is
  retired. The firmware no longer writes `tone.wav` to a card with no
  music (it stayed on the card once music arrived and came round in the
  shuffle as a honk); such a card runs the face, which says "Nothing
  playing", and answers touch. The half-second self-test at boot stays.
  A `tone.wav` an earlier build left on a card is the card owner's to
  remove; the firmware does not delete files from a card.
- `distributions/face`: touch. `touch::Tracker` turns the panel's reports
  into intents against the layout: a tap (under 800 ms, under 12 px of
  travel) on a button is `Intent::Tap`; a finger on the volume strip or
  the seek ring is `Intent::Slide` with the fraction as it moves and
  `Intent::Commit` on lift, sticky to the slider it started on.
  `Layout::map_touch` takes the CST3530's native portrait frame to the
  layout's frame at each rotation. A status change alone (pause, resume)
  repaints only the transport (`Repaint::Transport`), not the frame.
- `firmware/waveshare-esp32-c5-touch-lcd-28`: the face is live. Taps on
  play pause and resume (or start the next track when nothing holds
  custody), next and previous skip, the volume follows the finger on the
  strip, and a lift on the ring seeks. The sink applies the level as a
  Q15 gain on the house curve (1..=100 as -49.5..0 dB in half-decibel
  steps), so the speaker and a HAT hear the same level; it starts at 40%
  instead of full. Measured on the board: pause and resume, skip, a
  slide from 29% to 57%, and a seek on 96 kHz FLAC back playing 0.3 s
  later; a seek on MP3 takes 3-13 s in proportion to the distance, since
  Symphonia's demuxer walks the frames to the target at about 25 times
  real time off the card (open).
- `firmware/waveshare-esp32-c5-touch-lcd-28`: the sink names each dry
  spell as it sees it (`sink: the DMA ran dry for N ms`, at the write
  that ends it), so a capture places it in time; the ten-second report
  stays as the tally. A gap the firmware announces (`sink::expect_gap`,
  before a pause and before a start from idle) is not a dry spell.
- `firmware/waveshare-esp32-c5-touch-lcd-28`: the build id. `build.rs`
  stamps `git describe` into the binary, the boot log's first line names
  it (`build:<describe>`), and `scripts/image.sh` refuses to call an image
  good unless it carries the id of the tree it is run in. Two bench runs
  on 19 October flashed a stale image; this closes that class.
- `distributions/face` (`evo-esp32-face`) replaces `distributions/panel`:
  the player's own face instead of the companion's remote-control pages.
  Now Playing from the playback and delivery projections, in the evo
  family's themes (evo-ui's `styles.css` tokens, OKLCH converted once to
  RGB565: Evo Default, Night Sky, Sunrise, Air, Liquid), in two styles (the disc,
  with the progress on a ring round the art; the card, with the text on a
  band over the art's foot) and at rotations 0, 90, 180 and 270 (portrait
  and landscape layouts; the display driver turns the pixels). Inter
  (OFL 1.1, `assets/fonts`) is rasterised by `build.rs` into a 4-bit glyph
  atlas at the face's five sizes; text and every shape (discs, rings, the
  arc, rounded corners, the triangles of the transport glyphs) are drawn
  as rows with anti-aliased edges blended against the colour under them,
  so no frame buffer is read back. A layout test proves no region overlaps
  another at any rotation or style; `Layout::hit` maps a point to the
  play, previous and next buttons, the seek strip, the navigation and the
  art, for touch. `no_std`. The host gains `--style`, `--theme` and
  `--gallery DIR` (every rotation, style and theme as PNGs); the firmware
  draws the disc in Evo Default. On the board: full frame 1, then 5% of
  the time drawing, internal SRAM 49 KB free.
- `firmware/waveshare-esp32-c5-touch-lcd-28`: a track whose stream rate
  is above the chip's ceiling (96 kHz; 192 kHz FLAC decoded at 0.4 of
  real time) is passed over as soon as the decoder reports its format.
- `scripts/image.sh`: removes the crate's previous image before writing
  and finds esptool as a module, on PATH or in ESP-IDF's python_env.

- `platform/esp-idf` (`evo-esp32-platform-esp-idf`): Espressif's fixed-point
  MP3 decoder (`esp_audio_codec` 2.6.2, fetched by the ESP-IDF component
  manager; Espressif Modified MIT, for Espressif chips only) as a Symphonia
  `AudioDecoder`, registered at the preferred tier so the demuxer, seeking
  and the position stay Symphonia's. The ESP32-C5 has no FPU: Symphonia's
  floating-point MP3 reached 0.555 of real time with the core saturated;
  Espressif's plays at 1.000 with the core 23-25% busy and the face on.
  Ear: "perfect".
- `org.evoframework.playback.symphonia`: `default_codecs()` and
  `PlaybackSymphonia::with_codecs`, so a distribution can prefer a platform
  decoder for a codec; `new` keeps Symphonia's own.
- `firmware/waveshare-esp32-c5-touch-lcd-28`: the Espressif decoder in the
  registry; a track the decoder refuses (a codec nobody stocks, an
  unreadable file) is logged and passed over instead of ending the run;
  the companion dependencies move to `ccc5d41`, whose board crate waits
  for the touch controller to answer after its reset: the codec written
  to in that window got an I2C timeout (20 ms on the bench, 10 ms was
  waited). Measured: 192 kHz/24-bit FLAC decodes at 0.38 of real time;
  above 96 kHz is beyond this chip.
- `firmware/waveshare-esp32-c5-touch-lcd-28`: the fabric on the board.
  The card mounted through the companion's platform crate; the catalogue
  compiled in by `build.rs` and checked against the plugins at start; the
  steward with the files source, the Symphonia decoder and the null
  output; the tracks at the card's root played once each through
  `sink::I2sSink`, I2S0 with its DMA as the buffer: the decoder offers
  frames, the sink takes what fits with a zero timeout and says how many,
  a frame cut by the DMA boundary carried into the next call, mono
  doubled into both slots, the channel rebuilt and the ES8311 told when a
  stream's rate differs. A card with no music gets a two-second `tone.wav`
  first. Half a second of 1 kHz at boot as the self-test; an ES9018K2M on
  the HAT's I2C brought up when one answers. Every happening logged; a
  heartbeat every ten seconds with the heap and the main stack's
  low-water mark. Bench: 2 s of WAV from the card decoded and played in
  2.07 s, custody taken and released, no watchdog, 45 KB of the main
  task's 48 KB stack never touched. Found on the way: ESP-IDF's `usleep()`
  spins below one scheduler tick, so at 100 Hz the idle task starved once
  the fabric went quiet; `CONFIG_FREERTOS_HZ=1000`. A full DMA is the
  HAL's `ESP_ERR_TIMEOUT` with nothing taken, which is the sink
  contract's "ask again", not a warning.
- `firmware/waveshare-esp32-c5-touch-lcd-28`: the whole card, and real
  FLAC. The listing walks folders depth first, one request to the files
  source per folder; FatFs hands names to Rust as UTF-8. The I2S ring is
  sized in time, a quarter second at the stream's rate within 64 KB of
  SRAM (11 buffers at 44.1 kHz, 16 at 96 kHz), after a 96 kHz/24-bit
  album came out choppy with the core 60% idle: six buffers were 53 ms
  of cover and the decoder's cycle (20 ms idle plus one packet) was
  longer. `opt-level = 3`. The heartbeat reports the core's busy share,
  internal SRAM free, the playback position against the clock, and dry
  spells the sink certainly had. Bench: 96 kHz FLAC at 1.000 of real
  time, 37-58% busy, no dry spell, 54 KB internal free.
- `distributions/panel` (`evo-esp32-panel`): the panel as a consumer,
  shared by the host and the firmware: the playback and delivery
  projections mapped to the companion's `PlayerState`, the resting face
  drawn on any RGB565 target, repainting the whole face only on a scene
  change, the text block when the ticker moves, the seek strip when the
  position has moved a second. The host's `panel.rs` is the PNG wrapper
  over it. 2 tests.
- `firmware/waveshare-esp32-c5-touch-lcd-28`: the panel lit. The board
  comes up through the companion's new `boards/` crate (expander, ST7789,
  CST3530, backlight; the I2C bus handed out for the codec and a HAT's
  DAC), the card read before the panel as the companion does; the shared
  face drawn every 40 ms from the projections, the backlight after the
  first frame. The tracks are shuffled, so a bench run meets every format
  on the card early. The heartbeat adds the drawing share and the count
  of full frames. Every companion dependency at one commit. Measured: the
  face costs 1-2% of the core; MP3 decodes at 0.555 of real time with the
  core 98% busy (Symphonia's MP3 is floating point and the C5 has no
  FPU), so MP3 is not playable on this chip as it stands.
- `scripts/image.sh` takes the newest esp-idf-sys build under `target/`
  when an sdkconfig change has left two.

- `firmware/waveshare-esp32-c5-touch-lcd-28`: the first firmware, and the
  first sound. std Rust on ESP-IDF v5.5.5 (the companion's pin and build
  files); the IO expander and the ES8311 through the companion's drivers
  by its tag v0.1.0; I2S0 as master in Philips format, 16-bit stereo, on
  the vendor BSP's pins (BCLK GPIO24, WS GPIO25, data GPIO26, no MCLK);
  a 1 kHz tone for two seconds every ten through the board's speaker,
  each logged with the time the write took. Bench: chip ID `83 11 01`,
  352 800 bytes in 1 972 ms against 2 000 expected. Found on the way: the
  C5 has no FPU, and 88 200 software sines took eight seconds and tripped
  the idle task's watchdog; the tone is one exact cycle, tiled. The three
  I2S signals are also mirrored through the GPIO matrix onto header pins
  a Pi-HAT DAC can reach (BCLK GPIO2, WS GPIO3, data GPIO11), with the
  console moved to the USB Serial/JTAG port so J6 is free; the speaker
  and the HAT play the same frames.
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

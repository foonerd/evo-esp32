# evo-esp32

> The evo fabric on a microcontroller. A standalone music player on an
> ESP32 touch panel, built the way an evo device is built: a declared
> catalogue, slots stocked by plugins, one steward that owns the state.

evo™ and evoframework™ are trademarks of Just a Nerd. The fabric in
this repository is licensed under the Business Source License 1.1
(see `LICENSE`); the surface others implement against is licensed
under the Apache License, Version 2.0 (see the `LICENSE` file in each
such crate's directory). See `TRADEMARK.md` for the trademark policy
and `CONTRIBUTING.md` for the contribution flow (DCO sign-off
required).

**Where the project stands.** The fabric exists and runs on the host:
the SDK a plugin implements against, with the audio slot contracts typed
in; the steward that admits plugins, owns subjects, composes, emits
happenings and serves projections; the catalogue tool that validates a
`catalogue.toml` and checks a plugin set stocks it; four audio plugins
(a file source, a real decoder, a simulated one, a null output) and the
example; and a host distribution that compiles its catalogue in, decodes
FLAC, MP3 and WAV through the steward into a paced sink, bit-exact for
the lossless two, and draws the companion's face from the projections.
Every device-shaped crate builds for bare-metal RISC-V. The fabric runs
on the first board: `firmware/waveshare-esp32-c5-touch-lcd-28` brings up
the Waveshare ESP32-C5-Touch-LCD-2.8's codec and amplifier through the
companion's drivers, stands the steward and the three plugins up as the
host distribution does, and plays the music on the card through an I2S
sink paced by the I2S clock, to the board's speaker and to a Pi-HAT DAC
on its header at once. The hardware layers come from
[volumio-esp-companion](https://github.com/foonerd/volumio-esp-companion)
by tag. The [wiki](https://github.com/foonerd/evo-esp32/wiki) has the
[hardware](https://github.com/foonerd/evo-esp32/wiki/Hardware) the
first device needs.

## How it fits together

```mermaid
flowchart LR
    core["<b>evo-core</b><br/><i>the fabric on Linux</i><br/>steward · catalogue · slots<br/>happenings · projections"]
    audio["<b>evo-device-audio</b><br/><i>reference audio device</i><br/>the slot shapes of<br/>an audio player"]
    esp["<b>evo-esp32</b><br/><i>this repo</i><br/>the fabric on an MCU<br/>a player in a panel"]
    comp["<b>volumio-esp-companion</b><br/><i>hardware layers, MIT</i><br/>hal · drivers · platform<br/>board firmware crates"]

    core -.->|concepts, not code| esp
    audio -.->|slot shapes, not code| esp
    comp ==>|crates pinned by tag| esp
```

evo-core is a fabric for Linux appliances: one long-running steward
process administers a declared catalogue, admits plugin processes that
stock its slots, and emits projections and happenings over a socket.
Nothing of that runs on a microcontroller as it stands. What does carry
is the shape: the catalogue as the description of the device, plugins
that stock slots and never address each other, the steward as the sole
authority over state, projections as the only thing a consumer reads.
This repository takes that shape onto an ESP32 and finds out which
parts of the fabric survive the move and which have to be reinvented.

The hardware is not written here. The pins, the bring-up, the display,
the touch controller, the IO expander, Wi-Fi, the configuration store
and the HTTP client are the companion project's `hal`, `drivers`,
`platform` and `firmware` crates, consumed by tag. This repository
writes the application: the catalogue, the steward, the plugins that
make a panel a player.

## What this repository contains

| Part | Crate | Holds | Licence |
| --- | --- | --- | --- |
| `sdk/` | `evo-esp32-sdk` | The surface a plugin implements against: the catalogue types, the plugin contract, respondents and wardens, subjects, happenings, projections, the context; and the slot contracts, `contract::audio` first: the player's shelves, verbs, instructions and subject states as typed values. `no_std`. | Apache-2.0 |
| `steward/` | `evo-esp32-steward` | The sole authority: admission against the catalogue, the subject tables with per-plugin contributions, composition, a bounded happenings ring with gap-aware replay, projections by rack or subject, custody and ticking of wardens. `no_std`. | BSL 1.1 |
| `catalogue/` | `evo-esp32-catalogue` | The host tool: parse and validate `catalogue.toml`, check a plugin set stocks it, emit the catalogue as Rust for a distribution's build. | BSL 1.1 |
| `plugins/` | `org-evoframework-example-echo` | The pedagogical plugin: one shelf, one verb, one subject. | Apache-2.0 |
| | `org-evoframework-source-files` | `source/files` over a directory on a host: lists folders and audio files, announces each track as a subject. std. | Apache-2.0 |
| | `org-evoframework-playback-sim` | `playback/decode` against the clock: custody of a track, position, pause, resume, seek, release at the end; decodes nothing. `no_std`. | Apache-2.0 |
| | `org-evoframework-playback-symphonia` | `playback/decode` for real: FLAC, MP3, WAV and PCM through [Symphonia](https://github.com/pdeljanov/Symphonia) into the distribution's `FrameSink`, a bounded slice per tick, the sink pacing; position from frames delivered; seek to the frame. std. | Apache-2.0 |
| | `org-evoframework-delivery-null` | `delivery/output` with no sink: volume and mute on an `output` subject. `no_std`. | Apache-2.0 |
| `distributions/host/` | `evo-esp32-host` | What a firmware distribution does, on a desktop: the catalogue compiled in by `build.rs`, checked against the plugins at start, the three admitted, a track listed and decoded through the steward into a sink made of a clock and a file (`--wav` keeps what was heard), projections and happenings printed, and the panel as a consumer: the companion's pages drawn from the playback and delivery projections, written as a PNG. | BSL 1.1 |
| `firmware/` | `evo-esp32-waveshare-esp32-c5-touch-lcd-28` | One crate per board, each a distribution: the catalogue it ships, the plugins it stocks, the board it stands on. The first: the fabric on the Waveshare ESP32-C5-Touch-LCD-2.8, the card as the files source, Symphonia into an I2S sink with the DMA as the buffer, the ES8311 following the stream's rate, a Pi-HAT DAC on the header playing the same frames. | BSL 1.1 |

Presentation is not a shelf: the panel is a consumer that reads projections, as the network is, and lives in the distribution's loop; `distributions/host/src/panel.rs` is that consumer, mapping the fabric's state to the companion's `PlayerState` and drawing with its `core`. Frames are not values either: the decoder writes to a `FrameSink` the distribution owns (`sdk::contract::audio::playback::FrameSink`), as evo-device-audio's decoder writes to the operating system's audio stack and not to the fabric; on a board that sink is the I2S peripheral, on the host a clock and a file. The companion is a dependency by commit until its first tag, and the dependency runs that way only. Next: the output plugin owning the codec's volume and mute instead of the null one; the panel as a consumer on the board's display; a DAC driver per HAT the fleet has.

## Build

The host workspace builds on stable Rust, pinned by `rust-toolchain.toml`.

```sh
scripts/check.sh   # format, clippy, tests, docs, no_std on bare metal, the host distribution end to end, scripts, then every firmware built
cargo run -p evo-esp32-catalogue -- check distributions/host/catalogue.toml
cargo run -p evo-esp32-host -- --dir ~/Music --seconds 5 --panel face.png --wav heard.wav
```

The `no_std` claim is proved, not asserted: the check builds the SDK, the
steward and every device-shaped plugin for `riscv32imac-unknown-none-elf`, a
target with no std at all. The host distribution is run once against a
directory with one file, a tone it writes itself, and must decode it to the
end with every frame reaching the sink: the recorded WAV is the tone's size
to the byte. With a firmware crate present and no ESP toolchain on the
machine, the check fails and says so.

## Related

- [evo-core](https://github.com/foonerd/evo-core): the framework this
  takes its shape from.
- [evo-device-audio](https://github.com/foonerd/evo-device-audio): the
  reference audio device on Linux, whose slot shapes this reuses.
- [volumio-esp-companion](https://github.com/foonerd/volumio-esp-companion):
  the hardware layers this stands on, and the Volumio companion that
  shares the same boards.

## License

Business Source License 1.1 for the fabric; Apache License 2.0 for the
crates whose directories carry it. See [LICENSE](LICENSE). The licence
converts to Apache 2.0 four years after each release. For other
arrangements, open an issue titled `[licensing] ...`.

The Symphonia decoder the playback plugin uses is MPL-2.0, a file-level
licence that leaves the crates linking it under their own.

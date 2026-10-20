// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! evo-esp32 on ESP-IDF: what every Espressif firmware distribution
//! shares.
//!
//! [`aac`]: Espressif's AAC decoder, AAC-Plus included, behind the same
//! registry; Symphonia's own is built on the stack and overflows the
//! board's decoding task, and decodes the core alone.
//!
//! [`mp3`]: Espressif's fixed-point MP3 decoder behind Symphonia's codec
//! registry. On a chip without an FPU (the ESP32-C5) Symphonia's own,
//! floating-point, decoder reaches about half of real time; Espressif's
//! runs in a fraction of the core. The container, the demuxer, seeking
//! and the position stay Symphonia's; only the frame decode changes
//! hands.
//!
//! [`jpeg`]: Espressif's JPEG decoder (TJpgDec) for album art, decoding
//! at a scale of 1/2, 1/4 or 1/8 so a cover of hundreds of pixels comes
//! out near the face's art square in a few kilobytes of work area.
//!
//! [`png`]: the `png` crate for a station's logo, with its transparency
//! composited onto the colour it will sit on.
//!
//! [`http`]: `http://` and `https://` streams over ESP-IDF's HTTP client
//! and certificate bundle, as the playback plugin's [`StreamOpener`]
//! (`evo_esp32_sdk::contract::audio::playback::StreamOpener`).
//!
//! [`mdns`]: the device's `.local` name and the services it advertises
//! (the Evo audio plane's `_evo._tcp`), on Espressif's mDNS responder.

pub mod aac;
pub mod http;
pub mod jpeg;
pub mod mdns;
pub mod mp3;
pub mod png;

pub use http::HttpOpener;

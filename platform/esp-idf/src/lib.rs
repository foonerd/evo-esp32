// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! evo-esp32 on ESP-IDF: what every Espressif firmware distribution
//! shares.
//!
//! [`mp3`]: Espressif's fixed-point MP3 decoder behind Symphonia's codec
//! registry. On a chip without an FPU (the ESP32-C5) Symphonia's own,
//! floating-point, decoder reaches about half of real time; Espressif's
//! runs in a fraction of the core. The container, the demuxer, seeking
//! and the position stay Symphonia's; only the frame decode changes
//! hands.

pub mod mp3;

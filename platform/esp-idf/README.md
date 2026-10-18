# evo-esp32-platform-esp-idf

What every Espressif firmware distribution of evo-esp32 shares, over ESP-IDF. Built only inside a firmware crate's workspace (esp-idf-svc compiles for ESP-IDF targets alone); the host workspace excludes it.

## `mp3`: Espressif's MP3 decoder behind Symphonia

The ESP32-C5 has no FPU. Symphonia's MP3 decoder is floating point and reaches about half of real time on it; Espressif's `esp_audio_codec` MP3 decoder is fixed point and takes about a quarter of the core. `mp3::register` puts it in a Symphonia `CodecRegistry` at the preferred tier, so the container, the demuxer, seeking and the position stay Symphonia's and only the frame decode changes hands. A distribution builds its registry with `org_evoframework_playback_symphonia::default_codecs()`, registers this decoder, and hands the registry to `PlaybackSymphonia::with_codecs`.

The component is fetched by the ESP-IDF component manager at build (`espressif/esp_audio_codec` 2.6.2, declared in this crate's `Cargo.toml` for esp-idf-sys); the firmware's `components_<chip>.lock` pins what was resolved. Its bindings land in `esp_idf_svc::sys::esp_audio_codec`.

## Licence of the decoder

`esp_audio_codec` is Espressif's, under the Espressif Modified MIT licence: it may be used, copied, modified and distributed **exclusively with Espressif Systems products**, and redistribution for use with non-Espressif products is prohibited. This crate links it only into firmware for Espressif chips, which is what the licence permits. The licence text ships inside the component the build fetches, and must accompany any copy of a firmware image that carries the decoder. This crate itself is BSL 1.1 like the rest of the distribution layer.

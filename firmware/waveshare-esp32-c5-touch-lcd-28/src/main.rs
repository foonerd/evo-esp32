// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! evo-esp32 on the Waveshare ESP32-C5-Touch-LCD-2.8.
//!
//! The fabric on its first board. On boot:
//! 1. The SPI bus and the card on it; then the board through the
//!    companion's board crate: the I2C bus (SDA GPIO0, SCL GPIO1), the
//!    CH32V003 expander with its outputs high (EXIO3 is the NS4150B
//!    amplifier's enable), the ST7789 panel, the CST3530 touch.
//! 2. The codec and a HAT's DAC on the same I2C bus, handed out by the
//!    board.
//! 3. The ES8311 answers its chip ID, is opened, told the stream's
//!    rate and width, started, unmuted, set to a modest volume.
//! 4. I2S0 as master in Philips format, 16-bit stereo slots, on the
//!    BSP's pins (BCLK GPIO24, WS GPIO25, data to the codec GPIO26, no
//!    MCLK), as the board's [`sink::I2sSink`].
//! 5. The same three signals mirrored through the GPIO matrix onto the
//!    header pins a Pi-HAT DAC can reach: BCLK GPIO2 (J8.7), WS GPIO3
//!    (J8.8), data GPIO11 (J6 TXD; the console is on USB). An ES9018K2M
//!    answering on J5's I2C is brought up; a HAT without one needs
//!    nothing said to it. The speaker and the HAT play the same frames.
//! 6. The card mounted at `/sdcard` (before the panel, so it is read
//!    first). Without one there is nothing to play and the firmware says
//!    so and idles.
//! 7. The fabric, as the host distribution builds it: the catalogue
//!    compiled in and checked against the plugins, the steward, the files
//!    source over the card, the Symphonia decoder into the I2S sink, the
//!    null output. Every track on the card is played once, shuffled;
//!    a track whose stream turns out to be above the chip's rate ceiling
//!    is passed over as soon as the decoder reports its format;
//!    every happening but a position update goes to the log, and a
//!    heartbeat every ten seconds says what the heap and the main stack
//!    have left, how busy the core is and how the position stands
//!    against the clock. A card with no music runs the face all the
//!    same, saying so.
//! 8. The panel as a consumer: the player's face (`evo-esp32-face`) drawn
//!    from the playback and delivery projections every 40 ms, repainting
//!    only what moved; a full frame comes in stages with a decoder tick
//!    between each, so a track change does not starve the DMA ring; the
//!    backlight comes on after the first frame.

mod alloc;
mod art;
mod config;
mod net;
mod sink;

#[global_allocator]
static ALLOC: alloc::Placed = alloc::Placed;

use std::time::{Duration, Instant};

use companion_platform_esp_idf::Card;
use embedded_hal_bus::i2c::RefCellDevice;
use es8311::Es8311;
use es9018k2m::Es9018k2m;
use esp_idf_svc::hal::gpio::Pin;
use esp_idf_svc::hal::i2c::I2cDriver;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::log::EspLogger;
use esp_idf_svc::sys;
use evo_esp32_face::touch::{Intent, Tracker};
use evo_esp32_face::{
    theme, About, Face, Fact, Hotspot, Library, Repaint, Screen, Style, Theme,
    View,
};
use evo_esp32_sdk::catalogue::Catalogue;
use evo_esp32_sdk::contract::audio::playback::{self, Correction, Play};
use evo_esp32_sdk::contract::audio::source::{Entry, ListArgs, Listing, LIST};
use evo_esp32_sdk::contract::audio::{
    delivery, from_value, shelf, to_value, Track,
};
use evo_esp32_sdk::happening::Seq;
use evo_esp32_sdk::plugin::{Plugin, PluginDescription};
use evo_esp32_sdk::projection::ProjectionKey;
use evo_esp32_sdk::respondent::{CorrelationId, Request};
use evo_esp32_sdk::subject::SubjectId;
use evo_esp32_sdk::warden::{Assignment, CourseCorrection, CustodyHandle};
use evo_esp32_sdk::Ticks;
use evo_esp32_steward::{Config, Steward};
use log::{info, warn};
use org_evoframework_delivery_null::DeliveryNull;
use org_evoframework_playback_symphonia::{CodecRegistry, PlaybackSymphonia};
use org_evoframework_source_files::SourceFiles;
use sink::{I2sSink, RateFollower};

mod generated {
    //! `catalogue()`, from `catalogue.toml` at build time.
    extern crate alloc;
    include!(concat!(env!("OUT_DIR"), "/catalogue.rs"));
}

/// The rate the sink opens at; a stream at another rate reopens it.
const RATE: u32 = 44_100;
/// The highest stream rate this board plays. Measured: 192 kHz/24-bit
/// FLAC decodes at 0.4 of real time on the C5 with the core saturated,
/// and the ES8311 has no 192 kHz setting; 96 kHz plays at 1.0.
const RATE_CEILING: u32 = 96_000;
/// Playback volume, percent on the house curve, applied in the sink.
const VOLUME_PERCENT: u8 = 40;
/// The board's hardware gain the codec driver takes off, in half-dB
/// (the vendor's 5 V amplifier against the codec's 3.3 V output).
const HW_GAIN_HALF_DB: i16 = -7;
/// Where the card appears, and where the files source looks.
const MUSIC: &str = companion_platform_esp_idf::card::MOUNT_POINT;
/// How often the loop logs the heap and the stack.
const HEARTBEAT: Duration = Duration::from_secs(10);
/// How long the loop sleeps between ticks when the fabric is idle.
const TICK: Duration = Duration::from_millis(5);
/// The face's cadence: the companion steps its ticker every 40 ms.
const FACE_TICK: Duration = Duration::from_millis(40);
/// The panel's rotation, degrees; the companion's default.
const ROTATION: u16 = 0;
/// Backlight, 0 to 100.
const BACKLIGHT_PERCENT: u8 = 100;

/// The codec follows the sink's rate.
struct Codec(Es8311<RefCellDevice<'static, I2cDriver<'static>>>);

impl RateFollower for Codec {
    fn rate_changed(&mut self, rate: u32, bits: u8) {
        if let Err(e) = self.0.set_format(rate, bits) {
            warn!("es8311: no setting for {rate} Hz {bits}-bit: {e:?}; the speaker may be off-pitch");
        }
    }
}

fn main() -> anyhow::Result<()> {
    sys::link_patches();
    EspLogger::initialize_default();
    info!(
        "{} {} starting, {}",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
        env!("EVO_BUILD_ID")
    );

    let Peripherals {
        i2c0,
        i2s0,
        spi2,
        pins,
        modem,
        ..
    } = Peripherals::take()?;
    memory("at start");

    // The SPI bus first, so the card is read before the panel is up.
    let spi = board::spi_bus(spi2, pins.gpio6, pins.gpio7, pins.gpio8)?;
    let _card = match Card::mount(spi.clone(), pins.gpio23) {
        Ok(card) => {
            info!("card: mounted at {MUSIC}, {} MB", card.capacity >> 20);
            card
        }
        Err(e) => {
            warn!("card: none mounted ({e}); nothing to play");
            idle();
        }
    };
    let card_mb = _card.capacity >> 20;
    memory("after the card");

    // The card's evoconfig.txt: rotation is applied at the panel's
    // bring-up, theme and style when the face is made, the level at the
    // sink; evostatus.txt says what was read, once the fabric is up.
    let mut config = config::Config::read(MUSIC);
    let rotation = config.settings.rotation.unwrap_or(ROTATION);

    // The board in the vendor BSP's order: expander (amplifier enabled on
    // EXIO3), panel, touch. The backlight waits for the first frame.
    let mut board = board::bring_up(
        board::Pins {
            i2c0,
            sda: pins.gpio0,
            scl: pins.gpio1,
            dc: pins.gpio9,
            cs: pins.gpio10,
            touch_int: pins.gpio5,
        },
        spi,
        rotation,
    )?;

    let mut codec = Es8311::new(RefCellDevice::new(board.bus));
    match codec.chip_id() {
        Ok(id) => {
            info!("es8311: chip id {:02x} {:02x} {:02x}", id[0], id[1], id[2])
        }
        Err(e) => warn!("es8311: no chip id: {e:?}"),
    }
    codec
        .open()
        .map_err(|e| anyhow::anyhow!("es8311 open: {e:?}"))?;
    codec
        .set_format(RATE, 16)
        .map_err(|e| anyhow::anyhow!("es8311 format: {e:?}"))?;
    codec
        .start()
        .map_err(|e| anyhow::anyhow!("es8311 start: {e:?}"))?;
    codec
        .set_mute(false)
        .map_err(|e| anyhow::anyhow!("es8311 unmute: {e:?}"))?;
    // The codec sits at its 100%: the level is the sink's, so that the
    // HAT on the same stream hears the same volume.
    let half_db = es8311::percent_to_half_db(100, HW_GAIN_HALF_DB);
    codec
        .set_volume(half_db)
        .map_err(|e| anyhow::anyhow!("es8311 volume: {e:?}"))?;
    info!("es8311: open, {RATE} Hz 16-bit, started, at its 100% ({half_db} half-dB); the level is the sink's");

    memory("after the panel and the codec");
    let sink = I2sSink::new(
        i2s0,
        pins.gpio24,
        pins.gpio26,
        pins.gpio25,
        RATE,
        Codec(codec),
    )?;
    info!("i2s0: master, Philips 16-bit stereo, BCLK GPIO24, WS GPIO25, DOUT GPIO26");
    memory("after the DMA ring");

    // The network, on its own thread, after the ring has its memory: the
    // music does not wait for the search, and the ring is not short for
    // the driver. Its line in the status and in Settings follows the link.
    let mut net = match config.settings.wifi_ssid.clone() {
        Some(ssid) => {
            let password =
                config.settings.wifi_password.clone().unwrap_or_default();
            let hostname = config
                .settings
                .name
                .clone()
                .unwrap_or_else(|| DEFAULT_NAME.to_string());
            match net::Net::start(modem, ssid.clone(), password, Some(hostname))
            {
                Ok(net) => {
                    config.set_network(format!("wifi: joining '{ssid}'"));
                    Some(net)
                }
                Err(e) => {
                    warn!("net: no thread: {e}");
                    config.set_network(format!("wifi: could not start: {e}"));
                    None
                }
            }
        }
        None => {
            config.set_network("wifi: no wifi_ssid set; no network");
            None
        }
    };

    // The HAT's copy of the bus. The Pi header's I2S is BCLK pin 12, LRCLK
    // pin 35, DIN pin 40, GND pin 6; the HAT's 5 V is its own.
    mirror(pins.gpio2.pin(), sys::I2SO_BCK_OUT_IDX);
    mirror(pins.gpio3.pin(), sys::I2SO_WS_OUT_IDX);
    mirror(pins.gpio11.pin(), sys::I2SO_SD_OUT_IDX);
    info!("i2s0: mirrored for a HAT: BCLK GPIO2 (J8.7), WS GPIO3 (J8.8), DATA GPIO11 (J6 TXD)");

    let mut sabre = Es9018k2m::new(RefCellDevice::new(board.bus));
    match sabre.detect() {
        Ok(Some(id)) => {
            info!(
                "es9018k2m: found on the bus, status {:#04x}, revision {}",
                id.status, id.revision
            );
            match sabre.init().and_then(|()| sabre.set_volume(100)) {
                Ok(()) => {
                    info!("es9018k2m: initialised, at its 100%; the level is the sink's")
                }
                Err(e) => warn!("es9018k2m: init failed: {e:?}"),
            }
        }
        Ok(None) => {
            info!("es9018k2m: something answers at 0x48 but is not one")
        }
        Err(_) => info!("es9018k2m: none on the bus"),
    }

    let volume = sink.volume();
    let lead = sink.lead();
    let start_level = config.settings.volume.unwrap_or(VOLUME_PERCENT);
    volume.set(start_level, false);
    let started = Instant::now();

    let now = || Ticks(started.elapsed().as_millis() as u64);
    let mut steward = bring_up(sink, now()).map_err(|e| anyhow::anyhow!(e))?;
    memory("after the steward");
    info!(
        "steward up with {} plugins: {}",
        steward.plugins().count(),
        steward
            .plugins()
            .map(|p| p.name)
            .collect::<Vec<_>>()
            .join(", ")
    );

    let mut tracks =
        list_tracks(&mut steward, now()).map_err(|e| anyhow::anyhow!(e))?;
    memory("after the listing");
    if tracks.is_empty() {
        warn!("no audio file on the card; the face says so");
    }
    // Each track once, shuffled so a bench run meets every format and
    // rate on the card early; then the fabric idles.
    shuffle(&mut tracks);
    info!("{} tracks, shuffled", tracks.len());
    let mut next = 0usize;
    // The level the listener wants; every track starts at it.
    let mut wanted = start_level;
    let mut current =
        play_next(&mut steward, &tracks, &mut next, now(), wanted);
    let mut track_started = Instant::now();

    // The output subject's level and mute reach the sink from here.
    let mut level: Option<(u8, bool)> = None;

    // One finger on the panel, into taps and slides.
    let mut tracker = Tracker::default();

    // The library page: the folder in view, the way back to the root, and
    // the folder's tracks in the source's order, which a tap plays from.
    let mut browse = Browse::default();

    // The panel: the player's face, lit after its first frame. A full
    // frame comes in stages, one per pass of the loop, so the decoder
    // ticks between them and the DMA ring is fed; `painting` keeps the
    // passes coming without waiting for the face's cadence.
    let theme = config.theme(theme::EVO_DEFAULT);
    let style = config.style(Style::Disc);
    let mut face = Face::new(rotation, style, theme);
    memory("after the face");
    if config.settings.dac.is_some() || config.settings.output.is_some() {
        config.note("dac, output: read; this firmware plays the speaker and a HAT both and has no register of boards yet");
    }
    memory("before the first frame");
    let status_head = [
        format!(
            "evo-esp32 {} {} on Waveshare ESP32-C5-Touch-LCD-2.8",
            env!("CARGO_PKG_VERSION"),
            env!("EVO_BUILD_ID")
        ),
        format!(
            "running: theme {}, style {}, rotation {rotation}, volume {start_level}%, {} tracks on the card",
            face.theme().name,
            face.layout().style.name(),
            tracks.len()
        ),
    ];
    config.write_status(&status_head);
    // Album art, fetched beneath this task; the face shows it when it lands.
    let mut art = art::Fetcher::new(face.layout().art.size.width)?;
    memory("after the art fetcher");
    let mut lit = false;
    let mut last_face = Instant::now() - FACE_TICK;
    let mut painting = false;
    let mut frame_started: Option<Instant> = None;
    let mut longest_stage = Duration::ZERO;
    let mut stage_ms: Vec<u32> = Vec::new();
    let mut drawing = Duration::ZERO;
    let mut full_frames = 0u32;

    let mut seen = Seq(0);
    let mut last_beat = Instant::now();
    let mut busy = Duration::ZERO;
    loop {
        let t = now();
        let tick_started = Instant::now();
        let due = steward.tick(t);
        busy += tick_started.elapsed();
        // A report waits while INT is low; a finger that was down is read
        // until the controller says it lifted.
        if board.touch_int.is_low() || tracker.down() {
            match board.touch.report() {
                Ok(r) => {
                    let p = face.layout().map_touch(r.x, r.y);
                    if let Some(intent) =
                        tracker.sample(|p| face.hit(p), r.touching(), p, t.0)
                    {
                        match intent {
                            Intent::Tap(Hotspot::Play | Hotspot::BarPlay) => {
                                let status = playback_state(&steward)
                                    .map(|pb| pb.status)
                                    .unwrap_or_default();
                                match (current, status) {
                                    (Some(h), playback::Status::Playing) => {
                                        correct(
                                            &mut steward,
                                            h,
                                            Correction::Pause,
                                            t,
                                        )
                                    }
                                    (Some(h), playback::Status::Paused) => {
                                        correct(
                                            &mut steward,
                                            h,
                                            Correction::Resume,
                                            t,
                                        )
                                    }
                                    _ => {
                                        // Nothing in custody: start again.
                                        if next >= tracks.len() {
                                            next = 0;
                                        }
                                        current = play_next(
                                            &mut steward,
                                            &tracks,
                                            &mut next,
                                            t,
                                            wanted,
                                        );
                                        track_started = Instant::now();
                                    }
                                }
                            }
                            Intent::Tap(Hotspot::Next) => {
                                if next >= tracks.len() {
                                    next = 0;
                                }
                                skip(&mut steward, &mut current, t);
                            }
                            Intent::Tap(Hotspot::Previous) => {
                                // `next` is one past the playing track.
                                next = next.saturating_sub(2);
                                skip(&mut steward, &mut current, t);
                            }
                            Intent::Slide(Hotspot::Volume(f)) => {
                                let want = (f * 100.0).round() as u8;
                                if want != wanted {
                                    wanted = want;
                                    set_volume(&mut steward, wanted, t);
                                }
                            }
                            Intent::Commit(Hotspot::Seek(f)) => {
                                let duration = playback_state(&steward)
                                    .and_then(|pb| pb.track)
                                    .and_then(|tr| tr.duration_ms);
                                match (current, duration) {
                                    (Some(h), Some(d)) => correct(
                                        &mut steward,
                                        h,
                                        Correction::Seek {
                                            position_ms: (f as f64 * d as f64)
                                                as u64,
                                        },
                                        t,
                                    ),
                                    _ => info!("seek: nothing to seek in"),
                                }
                            }
                            Intent::Tap(Hotspot::Nav(i)) => {
                                let screen = Screen::from_nav(i);
                                match screen {
                                    Screen::Library if !browse.loaded => {
                                        browse.open(
                                            &mut steward,
                                            &mut face,
                                            "",
                                            t,
                                        );
                                    }
                                    Screen::Settings => face.set_about(facts(
                                        &face,
                                        card_mb,
                                        tracks.len(),
                                        &network_line(&net),
                                    )),
                                    _ => {}
                                }
                                face.set_screen(screen);
                            }
                            Intent::Tap(Hotspot::Row(i)) => {
                                if face.screen() == Screen::Settings {
                                    let fact =
                                        face.about().first + usize::from(i);
                                    match settings_row(fact) {
                                        Some(SettingsRow::Theme) => {
                                            let next = next_theme(face.theme());
                                            face.set_theme(next);
                                            config.save("theme", next.name);
                                        }
                                        Some(SettingsRow::Style) => {
                                            let next = match face.layout().style
                                            {
                                                Style::Disc => Style::Card,
                                                Style::Card => Style::Disc,
                                            };
                                            face.set_style(next);
                                            config.save("style", next.name());
                                            // The art square may be another size now.
                                            if art.resize(
                                                face.layout().art.size.width,
                                            ) {
                                                face.set_art(None);
                                            }
                                        }
                                        Some(SettingsRow::Rotation) => {
                                            // The panel is oriented at bring-up:
                                            // write the setting and start again.
                                            let next = (face.layout().rotation
                                                + 90)
                                                % 360;
                                            config.save(
                                                "rotation",
                                                &next.to_string(),
                                            );
                                            info!("rotation {next} written; restarting");
                                            esp_idf_svc::hal::reset::restart();
                                        }
                                        None => {}
                                    }
                                    face.set_about(facts(
                                        &face,
                                        card_mb,
                                        tracks.len(),
                                        &network_line(&net),
                                    ));
                                    continue;
                                }
                                if face.screen() != Screen::Library {
                                    continue;
                                }
                                let shown = face.layout().page.rows();
                                let Some(entry) = face
                                    .library()
                                    .visible(shown)
                                    .get(usize::from(i))
                                    .cloned()
                                else {
                                    continue;
                                };
                                if !entry.playable {
                                    info!(
                                        "library: {:?} is above the ceiling",
                                        entry.name
                                    );
                                    continue;
                                }
                                if entry.folder {
                                    let path = browse
                                        .folder_path(&entry.name)
                                        .unwrap_or_default();
                                    browse
                                        .trail
                                        .push(face.library().path.clone());
                                    browse.open(
                                        &mut steward,
                                        &mut face,
                                        &path,
                                        t,
                                    );
                                } else if let Some(at) =
                                    browse.tracks.iter().position(|tr| {
                                        Some(&tr.uri) == entry.uri.as_ref()
                                    })
                                {
                                    // The folder becomes the queue, from
                                    // the tapped track on.
                                    tracks = browse.tracks.clone();
                                    next = at;
                                    if current.is_some() {
                                        skip(&mut steward, &mut current, t);
                                    } else {
                                        current = play_next(
                                            &mut steward,
                                            &tracks,
                                            &mut next,
                                            t,
                                            wanted,
                                        );
                                        track_started = Instant::now();
                                    }
                                }
                            }
                            Intent::Tap(Hotspot::Back) => {
                                if let Some(parent) = browse.trail.pop() {
                                    browse.open(
                                        &mut steward,
                                        &mut face,
                                        &parent,
                                        t,
                                    );
                                }
                            }
                            Intent::Scroll(dy) => face.scroll(dy),
                            Intent::Tap(_)
                            | Intent::Slide(_)
                            | Intent::Commit(_) => {}
                        }
                    }
                }
                Err(e) => log::debug!("touch: {e:?}"),
            }
        }
        // Audio first: a paint stage can take 90 ms, and while the decoder
        // is short of lead the loop decodes instead of painting.
        let audio_short = lead.ms().is_some_and(|ms| ms < LOW_LEAD_MS);
        if !audio_short && (painting || last_face.elapsed() >= FACE_TICK) {
            last_face = Instant::now();
            let output = output_state(&steward);
            if let Some(out) = &output {
                let now_level = (out.volume, out.muted);
                if level != Some(now_level) {
                    volume.set(out.volume, out.muted);
                    info!(
                        "volume: {}%{}",
                        out.volume,
                        if out.muted { ", muted" } else { "" }
                    );
                    level = Some(now_level);
                }
            }
            let pb = playback_state(&steward).unwrap_or_default();
            // The chip's ceiling is the distribution's to enforce: the
            // decoder would play it, badly.
            if let (Some(handle), Some(f)) = (current, pb.format.as_ref()) {
                if f.sample_rate > RATE_CEILING {
                    warn!(
                        "{:?} is {} Hz, above this board's ceiling of {RATE_CEILING} Hz; passed over",
                        pb.track.as_ref().map(|t| t.title.as_str()).unwrap_or("?"),
                        f.sample_rate
                    );
                    if let Err(e) = steward.release(handle, t) {
                        warn!("release {}: {e:?}", handle.0);
                    }
                    current = None;
                }
            }
            let state = View::new(&pb, output.as_ref(), "Card");
            if let Some(track) = pb.track.as_ref() {
                if art.track(&track.uri) {
                    face.set_art(None);
                }
            }
            if let Some(found) = art.poll() {
                face.set_art(found.art);
            }
            if let Some(event) = net.as_mut().and_then(|n| n.poll()) {
                let line = match &event {
                    net::Event::Up {
                        ssid,
                        address,
                        hostname,
                        channel,
                    } => format!(
                        "wifi: joined '{ssid}' on channel {channel}; hostname {hostname}, address {address}"
                    ),
                    net::Event::Down => "wifi: link down; rejoining".to_string(),
                    net::Event::Failed(why) => format!("wifi: off: {why}"),
                };
                config.set_network(line);
                config.write_status(&status_head);
                if face.screen() == Screen::Settings {
                    face.set_about(facts(
                        &face,
                        card_mb,
                        tracks.len(),
                        &network_line(&net),
                    ));
                }
            }
            let painted = face.refresh(&mut board.panel, &state);
            let took = last_face.elapsed();
            drawing += took;
            match painted {
                Ok(Repaint::Partial) => {
                    if !painting {
                        frame_started = Some(last_face);
                        longest_stage = Duration::ZERO;
                        stage_ms.clear();
                    }
                    painting = true;
                    longest_stage = longest_stage.max(took);
                    stage_ms.push(took.as_millis() as u32);
                }
                Ok(Repaint::Full) => {
                    painting = false;
                    full_frames += 1;
                    longest_stage = longest_stage.max(took);
                    stage_ms.push(took.as_millis() as u32);
                    let whole =
                        frame_started.take().map_or(took, |s| s.elapsed());
                    log::debug!(
                        "panel: full frame in {} ms, the longest stage {} ms",
                        whole.as_millis(),
                        longest_stage.as_millis()
                    );
                    if !lit {
                        board.backlight(BACKLIGHT_PERCENT)?;
                        lit = true;
                        info!(
                            "panel lit: first frame in {} ms, the longest stage {} ms, stages {stage_ms:?} ms",
                            whole.as_millis(),
                            longest_stage.as_millis()
                        );
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    painting = false;
                    warn!("panel: {e:?}");
                }
            }
        }
        for h in steward.happenings_since(seen).happenings {
            seen = h.seq;
            let kind = serde_json::to_string(&h.kind).unwrap_or_default();
            // Position updates come every tick; the heartbeat reports them.
            if kind != r#"{"kind":"updated"}"# {
                info!(
                    "#{:<4} +{:>7} ms  {kind}{}",
                    h.seq.0,
                    h.at.0,
                    h.subject
                        .as_ref()
                        .map(|s| format!("  {s}"))
                        .unwrap_or_default()
                );
            }
            if kind.contains("custody_released") {
                if next < tracks.len() {
                    current = play_next(
                        &mut steward,
                        &tracks,
                        &mut next,
                        now(),
                        wanted,
                    );
                    track_started = Instant::now();
                } else if next == tracks.len() {
                    info!(
                        "all {} tracks played; the fabric idles",
                        tracks.len()
                    );
                    current = None;
                    next += 1;
                }
            }
        }
        if last_beat.elapsed() >= HEARTBEAT {
            let period = last_beat.elapsed().as_secs_f64();
            let busy_share = busy.as_secs_f64() / period;
            let drawing_share = drawing.as_secs_f64() / period;
            busy = Duration::ZERO;
            drawing = Duration::ZERO;
            last_beat = Instant::now();
            // SAFETY: plain getters; a null handle means the calling task.
            let (free, min, internal, stack) = unsafe {
                (
                    sys::esp_get_free_heap_size(),
                    sys::esp_get_minimum_free_heap_size(),
                    sys::heap_caps_get_free_size(sys::MALLOC_CAP_INTERNAL),
                    sys::uxTaskGetStackHighWaterMark(std::ptr::null_mut()),
                )
            };
            info!(
                "heartbeat: free heap {free} B (internal {internal} B), low-water {min} B, main stack low-water {stack} B, ticking {:.0}% and drawing {:.0}% of the time, {full_frames} full frames",
                busy_share * 100.0,
                drawing_share * 100.0
            );
            full_frames = 0;
            report_playback(&steward, track_started.elapsed());
            if let Some(line) = sink::report_dry() {
                warn!("sink: {line}");
            }
        }
        // The idle task must run or its watchdog bites: a short sleep
        // when the fabric wants ticking now or a frame is mid-paint, the
        // tick length otherwise.
        std::thread::sleep(if painting || due.is_some_and(|d| d.0 <= t.0) {
            Duration::from_millis(1)
        } else {
            TICK
        });
    }
}

/// Symphonia's codecs with Espressif's MP3 decoder preferred: the chip
/// has no FPU, and Symphonia's floating-point MP3 reaches half of real
/// time on it.
fn codecs() -> CodecRegistry {
    let mut registry = org_evoframework_playback_symphonia::default_codecs();
    evo_esp32_platform_esp_idf::mp3::register(&mut registry);
    registry
}

/// The catalogue this distribution was built with.
fn catalogue() -> Catalogue {
    generated::catalogue()
}

/// The plugins this distribution admits, in admission order, described
/// so the catalogue can be checked against them before any is loaded.
fn descriptions() -> Vec<PluginDescription> {
    vec![
        SourceFiles::new(MUSIC).describe(),
        PlaybackSymphonia::new(Box::new(Silent)).describe(),
        DeliveryNull::new().describe(),
    ]
}

/// A sink that takes nothing, for describing the decoder before the
/// real one is handed over.
struct Silent;

impl evo_esp32_sdk::contract::audio::playback::FrameSink for Silent {
    fn start(&mut self, _: &evo_esp32_sdk::contract::audio::playback::Format) {}
    fn write(&mut self, _: &[i16]) -> usize {
        0
    }
    fn flush(&mut self) {}
    fn stop(&mut self) {}
}

/// Build the fabric: steward, plugins, checked against the catalogue.
fn bring_up(sink: I2sSink<Codec>, now: Ticks) -> Result<Steward, String> {
    let catalogue = catalogue();
    let report = evo_esp32_catalogue::check(&catalogue, &descriptions());
    if !report.is_clean() {
        return Err(format!("the catalogue is not stocked:\n{report}"));
    }
    let mut steward = Steward::new(catalogue, Config::default())
        .map_err(|e| e.to_string())?;
    steward
        .admit_respondent(Box::new(SourceFiles::new(MUSIC)), now)
        .map_err(|e| e.to_string())?;
    steward
        .admit_warden(
            Box::new(PlaybackSymphonia::with_codecs(Box::new(sink), codecs())),
            now,
        )
        .map_err(|e| e.to_string())?;
    steward
        .admit_respondent(Box::new(DeliveryNull::new()), now)
        .map_err(|e| e.to_string())?;
    Ok(steward)
}

/// Ask the source for the root and keep the tracks it lists, in order.
/// Every track on the card, folders first and depth first, each folder
/// in the files source's order: one listing request per folder.
fn list_tracks(
    steward: &mut Steward,
    now: Ticks,
) -> Result<Vec<Track>, String> {
    let mut tracks = Vec::new();
    let mut pending = vec![String::new()];
    while let Some(path) = pending.pop() {
        let listing = list_folder(steward, &path, now)?;
        let depth = path.split('/').filter(|s| !s.is_empty()).count();
        let indent = "  ".repeat(depth);
        let mut folders = Vec::new();
        let mut here = 0usize;
        for e in listing.entries {
            match e {
                Entry::Folder { name, path } => {
                    info!("{indent}{name}/");
                    folders.push(path);
                }
                Entry::Track(t) => {
                    info!("{indent}  {}", t.title);
                    tracks.push(t);
                    here += 1;
                }
            }
        }
        if depth == 0 {
            info!(
                "source/files: {here} tracks at the root, {} folders",
                folders.len()
            );
        }
        // A stack: push in reverse so the first folder is listed next.
        pending.extend(folders.into_iter().rev());
    }
    Ok(tracks)
}

fn list_folder(
    steward: &mut Steward,
    path: &str,
    now: Ticks,
) -> Result<Listing, String> {
    let response = steward
        .request(
            &Request {
                cid: CorrelationId(1),
                shelf: shelf::source_files(),
                verb: LIST.into(),
                subject: None,
                args: to_value(&ListArgs {
                    path: path.to_string(),
                }),
            },
            now,
        )
        .map_err(|e| format!("{e:?}"))?;
    match response.outcome {
        Ok(v) => from_value(&v),
        Err(f) => Err(format!("list {path:?}: {f:?}")),
    }
}

/// The playback projection's state, if a warden has published one.
fn playback_state(steward: &Steward) -> Option<playback::State> {
    steward
        .projection(ProjectionKey::Rack {
            name: shelf::PLAYBACK.into(),
        })
        .subjects
        .first()
        .and_then(|s| from_value(&s.state).ok())
}

/// The output's state, if the delivery plugin has published one.
fn output_state(steward: &Steward) -> Option<delivery::State> {
    steward
        .projection(ProjectionKey::Rack {
            name: shelf::DELIVERY.into(),
        })
        .subjects
        .first()
        .and_then(|s| from_value(&s.state).ok())
}

/// A Fisher-Yates shuffle on the chip's hardware random numbers.
fn shuffle<T>(items: &mut [T]) {
    for i in (1..items.len()).rev() {
        // SAFETY: esp_random reads the hardware RNG; no preconditions.
        let r = unsafe { sys::esp_random() } as usize;
        items.swap(i, r % (i + 1));
    }
}

/// The playback projection against the wall clock since the track was
/// assigned: position over elapsed is 1.0 when the decoder keeps up.
fn report_playback(steward: &Steward, elapsed: Duration) {
    let Some(st) = playback_state(steward) else {
        return;
    };
    let elapsed_ms = elapsed.as_millis().max(1) as u64;
    info!(
        "playback: {:?} {} ms of {} ms elapsed ({:.3}){}{}",
        st.status,
        st.position_ms,
        elapsed_ms,
        st.position_ms as f64 / elapsed_ms as f64,
        st.format
            .as_ref()
            .map(|f| format!("  {} Hz {} ch", f.sample_rate, f.channels))
            .unwrap_or_default(),
        st.track
            .as_ref()
            .map(|t| format!("  {:?}", t.title))
            .unwrap_or_default()
    );
}

/// Play the track at `*next`, or the first after it the decoder takes;
/// a refused track (a codec nobody stocks, an unreadable file) is logged
/// and passed over, so one bad file does not end the run. The custody
/// taken, if any.
fn play_next(
    steward: &mut Steward,
    tracks: &[Track],
    next: &mut usize,
    now: Ticks,
    level: u8,
) -> Option<CustodyHandle> {
    while let Some(track) = tracks.get(*next) {
        *next += 1;
        if !within_ceiling(track) {
            info!(
                "{:?} is {} Hz, above this board's ceiling of {RATE_CEILING} Hz; passed over unopened",
                track.title,
                track.sample_rate.unwrap_or(0)
            );
            continue;
        }
        if let Some(handle) = play(steward, track, now, level) {
            return Some(handle);
        }
    }
    None
}

/// Whether the source's listing says the track's rate is one this board
/// plays; a track whose rate the listing does not know is tried.
fn within_ceiling(track: &Track) -> bool {
    track.sample_rate.is_none_or(|rate| rate <= RATE_CEILING)
}

/// Pause, resume or seek the track in custody.
fn correct(
    steward: &mut Steward,
    handle: CustodyHandle,
    correction: Correction,
    now: Ticks,
) {
    info!("touch: {correction:?}");
    if let Err(e) = steward.correct(
        handle,
        CourseCorrection {
            instruction: to_value(&correction),
        },
        now,
    ) {
        warn!("correct {}: {e:?}", handle.0);
    }
}

/// Let go of the track in custody; `custody_released` then plays the
/// one `next` names. With nothing in custody the loop's handler does not
/// run, so the caller has already pointed `next` where it wants to go.
fn skip(
    steward: &mut Steward,
    current: &mut Option<CustodyHandle>,
    now: Ticks,
) {
    if let Some(h) = current.take() {
        info!("touch: skip, releasing custody {}", h.0);
        if let Err(e) = steward.release(h, now) {
            warn!("release {}: {e:?}", h.0);
        }
    }
}

/// Ask the output for `level`, 0 to 100.
fn set_volume(steward: &mut Steward, level: u8, now: Ticks) {
    let _ = steward.request(
        &Request {
            cid: CorrelationId(2),
            shelf: shelf::delivery_output(),
            verb: delivery::VOLUME.into(),
            subject: None,
            args: to_value(&delivery::Volume { level }),
        },
        now,
    );
}

/// Give the decoder custody of `track` from its start, at the volume;
/// the custody if it took it.
fn play(
    steward: &mut Steward,
    track: &Track,
    now: Ticks,
    level: u8,
) -> Option<CustodyHandle> {
    let taken = match steward.assign(
        &shelf::playback_decode(),
        Assignment {
            subject: SubjectId::new("playback/now"),
            instruction: to_value(&Play {
                track: track.clone(),
                start_ms: 0,
            }),
        },
        now,
    ) {
        Ok(handle) => {
            info!("playing {:?} under custody {}", track.title, handle.0);
            Some(handle)
        }
        Err(e) => {
            warn!("assign {:?}: {e:?}", track.title);
            None
        }
    };
    set_volume(steward, level, now);
    taken
}

/// Nothing to do; stay up so the log stays readable.
fn idle() -> ! {
    loop {
        std::thread::sleep(Duration::from_secs(60));
        info!("idle");
    }
}

/// Drive `gpio` from the I2S output `signal` as well as the pin the
/// driver was given; the GPIO matrix fans one signal out to any number
/// of pins.
fn mirror(gpio: u8, signal: u32) {
    // SAFETY: plain configuration calls on a pin this firmware owns and
    // nothing else drives.
    unsafe {
        sys::gpio_reset_pin(i32::from(gpio));
        sys::gpio_set_direction(
            i32::from(gpio),
            sys::gpio_mode_t_GPIO_MODE_OUTPUT,
        );
        sys::esp_rom_gpio_connect_out_signal(
            u32::from(gpio),
            signal,
            false,
            false,
        );
    }
}
/// The library page's state: the folder in view, the folders above it,
/// and the folder's tracks in the source's order.
#[derive(Default)]
struct Browse {
    loaded: bool,
    /// The paths of the folders opened to reach the one in view.
    trail: Vec<String>,
    /// The folder in view's subfolders, name to path.
    folders: Vec<(String, String)>,
    /// The folder in view's tracks, in order.
    tracks: Vec<Track>,
}

impl Browse {
    /// List `path` from the source and show it on the face.
    fn open(
        &mut self,
        steward: &mut Steward,
        face: &mut Face,
        path: &str,
        now: Ticks,
    ) {
        let listing = match list_folder(steward, path, now) {
            Ok(l) => l,
            Err(e) => {
                warn!("library: {e}");
                return;
            }
        };
        self.folders.clear();
        self.tracks.clear();
        let mut entries = Vec::with_capacity(listing.entries.len());
        for e in listing.entries {
            match e {
                Entry::Folder { name, path } => {
                    entries.push(evo_esp32_face::Entry {
                        name: name.clone(),
                        folder: true,
                        uri: None,
                        playable: true,
                    });
                    self.folders.push((name, path));
                }
                Entry::Track(t) => {
                    entries.push(evo_esp32_face::Entry {
                        name: t.title.clone(),
                        folder: false,
                        uri: Some(t.uri.clone()),
                        playable: within_ceiling(&t),
                    });
                    self.tracks.push(t);
                }
            }
        }
        let name = path
            .rsplit('/')
            .find(|s| !s.is_empty())
            .unwrap_or("Card")
            .to_string();
        face.set_library(Library {
            path: path.to_string(),
            name,
            entries,
            first: 0,
        });
        self.loaded = true;
    }

    /// The source's path of the subfolder called `name`.
    fn folder_path(&self, name: &str) -> Option<String> {
        self.folders
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, p)| p.clone())
    }
}

/// What the settings page says about this player.
/// The hostname when `name` is not set: the template's.
const DEFAULT_NAME: &str = "evo-player";

/// Below this much decoded audio in hand, the loop paints nothing and
/// decodes: a track's start, a slow passage at 96 kHz. A paint stage is
/// up to 90 ms, and the DMA ring stands behind the reservoir.
const LOW_LEAD_MS: u32 = 120;

/// A line of internal SRAM left at a point of the boot: what the DMA
/// rings, the stacks and the Wi-Fi driver compete for.
fn memory(when: &str) {
    // SAFETY: plain getters with no preconditions.
    let (free, largest) = unsafe {
        (
            sys::heap_caps_get_free_size(sys::MALLOC_CAP_INTERNAL),
            sys::heap_caps_get_largest_free_block(sys::MALLOC_CAP_INTERNAL),
        )
    };
    info!("memory: internal {free} B free, largest block {largest} B, {when}");
}

/// What the Wi-Fi row says.
fn network_line(net: &Option<net::Net>) -> String {
    match net {
        Some(net) => net.state.line(),
        None => "not set".to_string(),
    }
}

fn facts(face: &Face, card_mb: u64, tracks: usize, network: &str) -> About {
    let fact = |name: &str, value: String| Fact {
        name: name.to_string(),
        value,
    };
    About {
        facts: vec![
            fact("Player", format!("evo-esp32 {}", env!("CARGO_PKG_VERSION"))),
            fact("Wi-Fi", network.to_string()),
            fact(
                "Build",
                env!("EVO_BUILD_ID")
                    .trim_start_matches("build:")
                    .to_string(),
            ),
            fact("Board", "Waveshare ESP32-C5 2.8".to_string()),
            fact("Theme \u{203a}", face.theme().name.to_string()),
            fact("Style \u{203a}", face.layout().style.name().to_string()),
            fact("Rotation \u{203a}", format!("{}", face.layout().rotation)),
            fact(
                "Card",
                format!("{:.0} GB, {tracks} tracks", card_mb as f64 / 1024.0),
            ),
        ],
        first: 0,
    }
}

/// The settings rows a tap acts on, by their place in [`facts`].
enum SettingsRow {
    Theme,
    Style,
    Rotation,
}

fn settings_row(i: usize) -> Option<SettingsRow> {
    match i {
        4 => Some(SettingsRow::Theme),
        5 => Some(SettingsRow::Style),
        6 => Some(SettingsRow::Rotation),
        _ => None,
    }
}

/// The theme after `current` in the family's order, round the end.
fn next_theme(current: &Theme) -> Theme {
    let at = theme::ALL
        .iter()
        .position(|t| t.name == current.name)
        .unwrap_or(0);
    theme::ALL[(at + 1) % theme::ALL.len()]
}

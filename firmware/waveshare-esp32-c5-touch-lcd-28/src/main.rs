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
use esp_idf_svc::hal::task::thread::{MallocCap, ThreadSpawnConfiguration};
use esp_idf_svc::log::EspLogger;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sys;
use evo_esp32_face::touch::{Intent, Tracker};
use evo_esp32_face::{
    theme, About, Face, Fact, Hotspot, Library, Repaint, Screen, Style, Theme,
    View,
};
use evo_esp32_platform_esp_idf::mdns::{Advertiser, Search};
use evo_esp32_platform_esp_idf::HttpOpener;
use evo_esp32_queue::{Queue, Repeat};
use evo_esp32_sdk::catalogue::Catalogue;
use evo_esp32_sdk::contract::audio::playback::{
    self, Correction, Play, Receive,
};
use evo_esp32_sdk::contract::audio::source::{Entry, ListArgs, Listing, LIST};
use evo_esp32_sdk::contract::audio::{
    delivery, from_value, shelf, to_value, Track,
};
use evo_esp32_sdk::happening::{HappeningKind, Seq};
use evo_esp32_sdk::plugin::{Plugin, PluginDescription};
use evo_esp32_sdk::projection::ProjectionKey;
use evo_esp32_sdk::respondent::{CorrelationId, Request};
use evo_esp32_sdk::subject::SubjectId;
use evo_esp32_sdk::warden::{Assignment, CourseCorrection, CustodyHandle};
use evo_esp32_sdk::Ticks;
use evo_esp32_steward::{Config, Steward};
use evo_esp32_volumio::{
    self as volumio, Art as VolumioArt, Command as GroupCommand, Identity,
    Playing, Presence,
};
use log::{info, warn};
use org_evoframework_delivery_null::DeliveryNull;
use org_evoframework_multiroom_receiver::{
    Link, MultiroomReceiver, Settings as RoomSettings,
};
use org_evoframework_playback_symphonia::{CodecRegistry, PlaybackSymphonia};
use org_evoframework_snapcast_receiver::{
    Link as SnapLink, Settings as SnapSettings, SnapcastReceiver,
};
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
/// and the ES8311 has no 192 kHz setting; 96 kHz plays at real time
/// with the network up, 14% of the core to spare (the card read ahead
/// on its own thread).
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
/// How often the loop sleeps a millisecond for the idle task while the
/// decoder is behind and would otherwise never let go of the core.
const IDLE_EVERY: Duration = Duration::from_millis(250);
/// The face's cadence: the companion steps its ticker every 40 ms.
const FACE_TICK: Duration = Duration::from_millis(40);
/// The panel's rotation, degrees; the companion's default.
const ROTATION: u16 = 0;
/// Backlight, 0 to 100.
const BACKLIGHT_PERCENT: u8 = 100;
/// How long one search of the network for a Snapcast server listens.
const SEARCH_FOR: Duration = Duration::from_secs(3);
/// How long after a search that found nothing, or a found server that
/// went away, the next search starts.
const SEARCH_AGAIN: Duration = Duration::from_secs(30);

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
    // Threads the plugins spawn from this task (a stream's fetcher) get
    // their stacks in PSRAM: they write no flash, and internal SRAM is
    // what the Wi-Fi driver and the DMA rings live on. A thread that
    // does write flash (`net.rs`) asks for internal itself.
    // Threads the plugins spawn get PSRAM stacks, and so do the threads
    // those spawn (the open thread's fetcher, the listener's connection):
    // ESP-IDF applies a thread's configuration to its children only when
    // told to inherit.
    ThreadSpawnConfiguration {
        stack_alloc_caps: MallocCap::Spiram | MallocCap::Cap8bit,
        inherit: true,
        ..Default::default()
    }
    .set()?;

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

    // The TCP/IP stack before anything binds a socket: the multiroom
    // receiver listens from the moment it is loaded, and the network
    // thread brings the driver up at its own pace.
    esp_idf_svc::netif::NetifStack::initialize()?;
    // The device's identity on the Evo network, from the chip itself.
    let nvs = EspDefaultNvsPartition::take()?;
    let device_id = identity();
    info!("evo: device id {device_id}");
    let hostname = config
        .settings
        .name
        .clone()
        .unwrap_or_else(|| DEFAULT_NAME.to_string());

    // The network, on its own thread, after the ring has its memory: the
    // music does not wait for the search, and the ring is not short for
    // the driver. Its line in the status and in Settings follows the link.
    let mut net = match config.settings.wifi_ssid.clone() {
        Some(ssid) => {
            let password =
                config.settings.wifi_password.clone().unwrap_or_default();
            match net::Net::start(
                modem,
                nvs,
                ssid.clone(),
                password,
                Some(hostname.clone()),
            ) {
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
    let link = Link::new();
    let room_settings =
        RoomSettings::new(device_id.clone(), env!("CARGO_PKG_VERSION"));
    let snap_link = SnapLink::new();
    let snap_settings = SnapSettings::new(
        mac_text(),
        hostname.clone(),
        env!("CARGO_PKG_VERSION"),
    );
    let mut steward = bring_up(
        sink::Shared::new(sink),
        &link,
        room_settings,
        &snap_link,
        snap_settings,
        now(),
    )
    .map_err(|e| anyhow::anyhow!(e))?;
    // The network sources that take the sink from the queue while they
    // send; whether the queue plays again after.
    let mut rooms = Rooms::new(link, snap_link);
    let mut resume_after_room = false;
    let mut room_took_the_sink = false;
    let mut mdns: Option<Advertiser> = None;
    // The search for a Snapcast server when the card names none: one at
    // a time, the next one due at `search_at`.
    let snapserver = config.settings.snapserver.clone();
    let mut search: Option<Search> = None;
    let mut search_at: Option<Instant> = None;
    // The device among Volumio's: listed in their Multiroom, grouped
    // from their side; the peers are the ones that connect.
    let identity = Identity {
        name: hostname.clone(),
        uuid: device_id.clone(),
    };
    let presence = Presence::new(identity.clone());
    match presence.start(volumio::PORT) {
        Ok(()) => info!(
            "volumio: {} on port {} as {}",
            identity.name,
            volumio::PORT,
            identity.uuid
        ),
        Err(e) => warn!("volumio: not started: {e}"),
    }
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
        list_tracks(&mut steward, "", now()).map_err(|e| anyhow::anyhow!(e))?;
    memory("after the listing");
    if tracks.is_empty() {
        warn!("no audio file on the card; the face says so");
    }
    // The card's files are the queue unless `start` names something
    // else; stations have no end, so the card's run leaves them out and
    // a tap in the library plays one.
    let stations = tracks.iter().filter(|t| is_stream(t)).count();
    tracks.retain(|t| !is_stream(t));
    let card_tracks = tracks.len();
    // SAFETY: esp_random reads the hardware RNG; no preconditions.
    let seed = unsafe {
        (u64::from(sys::esp_random()) << 32) | u64::from(sys::esp_random())
    };
    let mut queue: Queue<Track> = Queue::new(seed);
    queue.set_shuffle(config.settings.shuffle.unwrap_or(true));
    queue.set_repeat(match config.settings.repeat {
        Some(evo_esp32_config::Repeat::Off) => Repeat::Off,
        Some(evo_esp32_config::Repeat::One) => Repeat::One,
        Some(evo_esp32_config::Repeat::All) | None => Repeat::All,
    });
    // Why the last thing asked for did not play, for the status strip.
    let mut notice: Option<String> = None;
    // A station named by `start` waits for the network before it is
    // asked for; a file does not.
    let mut start_when_up = false;
    match config.settings.start.clone() {
        Some(start) => match starting_point(&mut steward, &start, now()) {
            Ok((items, at)) => {
                info!(
                    "start: {start:?}: {} items, from {}",
                    items.len(),
                    at.map_or("the first".to_string(), |i| format!("#{i}"))
                );
                let station = at
                    .and_then(|i| items.get(i))
                    .or(items.first())
                    .is_some_and(is_stream);
                queue.set(items, at);
                if station && net.is_some() {
                    start_when_up = true;
                }
            }
            Err(why) => {
                warn!("start: {start:?}: {why}; the card plays");
                notice = Some(format!("start: {why}"));
                config.note(format!("start: {start} -> {why}"));
                queue.set(tracks, None);
            }
        },
        None => queue.set(tracks, None),
    }
    info!(
        "queue: {} items, shuffle {}, repeat {}; {card_tracks} tracks and {stations} stations on the card; seed {seed:#x}",
        queue.len(),
        if queue.shuffle() { "on" } else { "off" },
        repeat_name(queue.repeat())
    );
    // The level the listener wants; every track starts at it.
    let mut wanted = start_level;
    let mut current = if start_when_up {
        if let Some(t) = queue.items().first() {
            notice = Some(format!("{}: waiting for the network", t.title));
        }
        None
    } else {
        advance(
            &mut steward,
            &mut queue,
            Step::Next,
            now(),
            wanted,
            &mut notice,
        )
    };
    let mut track_started = Instant::now();
    // A station being reached; memory is logged when it is up.
    let mut opening = false;
    // What the release of custody should play: set before a skip that is
    // not "the next one".
    let mut then: Option<Step> = None;

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
            "running: theme {}, style {}, rotation {rotation}, volume {start_level}%, {card_tracks} tracks on the card",
            face.theme().name,
            face.layout().style.name(),
        ),
    ];
    config.write_status(&status_head);
    // Album art, fetched beneath this task; the face shows it when it lands.
    let mut art =
        art::Fetcher::new(face.layout().art.size.width, background(&face))?;
    memory("after the art fetcher");
    let mut lit = false;
    let mut last_face = Instant::now() - FACE_TICK;
    let mut last_net = Instant::now() - FACE_TICK;
    let mut painting = false;
    let mut frame_started: Option<Instant> = None;
    let mut longest_stage = Duration::ZERO;
    let mut stage_ms: Vec<u32> = Vec::new();
    let mut drawing = Duration::ZERO;
    let mut full_frames = 0u32;

    let mut seen = Seq(0);
    let mut last_beat = Instant::now();
    let mut last_idle = Instant::now();
    let mut shares = CoreShares::new();
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
                                match (current.or(rooms.handle()), status) {
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
                                    // Still being reached: nothing to
                                    // hold yet; next and previous move on.
                                    (Some(_), playback::Status::Opening) => {}
                                    _ => {
                                        // Nothing in custody: the item in
                                        // hand again (a held station), or
                                        // the next; after the end, the
                                        // start.
                                        start_when_up = false;
                                        current = advance(
                                            &mut steward,
                                            &mut queue,
                                            Step::Current,
                                            t,
                                            wanted,
                                            &mut notice,
                                        );
                                        track_started = Instant::now();
                                    }
                                }
                            }
                            Intent::Tap(Hotspot::Next) => {
                                if rooms.take_back(&mut steward, t) {
                                    resume_after_room = false;
                                }
                                if current.is_some() {
                                    then = Some(Step::Skip);
                                    skip(&mut steward, &mut current, t);
                                } else {
                                    current = advance(
                                        &mut steward,
                                        &mut queue,
                                        Step::Skip,
                                        t,
                                        wanted,
                                        &mut notice,
                                    );
                                    track_started = Instant::now();
                                }
                            }
                            Intent::Tap(Hotspot::Previous) => {
                                if rooms.take_back(&mut steward, t) {
                                    resume_after_room = false;
                                }
                                if current.is_some() {
                                    then = Some(Step::Previous);
                                    skip(&mut steward, &mut current, t);
                                } else {
                                    current = advance(
                                        &mut steward,
                                        &mut queue,
                                        Step::Previous,
                                        t,
                                        wanted,
                                        &mut notice,
                                    );
                                    track_started = Instant::now();
                                }
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
                                        card_tracks,
                                        &network_line(&net),
                                        &queue,
                                        &device_id,
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
                                            // A logo sits on the background.
                                            if art.refit(
                                                face.layout().art.size.width,
                                                background(&face),
                                            ) {
                                                face.set_art(None);
                                            }
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
                                            if art.refit(
                                                face.layout().art.size.width,
                                                background(&face),
                                            ) {
                                                face.set_art(None);
                                            }
                                        }
                                        Some(SettingsRow::Shuffle) => {
                                            let on = !queue.shuffle();
                                            queue.set_shuffle(on);
                                            config.save(
                                                "shuffle",
                                                if on { "on" } else { "off" },
                                            );
                                        }
                                        Some(SettingsRow::Repeat) => {
                                            let next = match queue.repeat() {
                                                Repeat::Off => Repeat::All,
                                                Repeat::All => Repeat::One,
                                                Repeat::One => Repeat::Off,
                                            };
                                            queue.set_repeat(next);
                                            config.save(
                                                "repeat",
                                                repeat_name(next),
                                            );
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
                                        card_tracks,
                                        &network_line(&net),
                                        &queue,
                                        &device_id,
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
                                    queue.set(browse.tracks.clone(), Some(at));
                                    start_when_up = false;
                                    if current.is_some() {
                                        then = Some(Step::Skip);
                                        skip(&mut steward, &mut current, t);
                                    } else {
                                        current = advance(
                                            &mut steward,
                                            &mut queue,
                                            Step::Skip,
                                            t,
                                            wanted,
                                            &mut notice,
                                        );
                                        track_started = Instant::now();
                                    }
                                }
                            }
                            Intent::Tap(Hotspot::Shuffle) => {
                                let on = !queue.shuffle();
                                queue.set_shuffle(on);
                                config.save(
                                    "shuffle",
                                    if on { "on" } else { "off" },
                                );
                            }
                            Intent::Tap(Hotspot::Repeat) => {
                                let next = match queue.repeat() {
                                    Repeat::Off => Repeat::All,
                                    Repeat::All => Repeat::One,
                                    Repeat::One => Repeat::Off,
                                };
                                queue.set_repeat(next);
                                config.save("repeat", repeat_name(next));
                            }
                            Intent::Tap(Hotspot::Text) => {
                                face.show_sheet(true);
                            }
                            Intent::Tap(Hotspot::Sheet) => {
                                face.show_sheet(false);
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
        // The network and the hand-over of the sink keep their cadence
        // whatever the decoder is doing: a stream from the network must
        // reach the sink while the card's track is short of lead too.
        if last_net.elapsed() >= FACE_TICK {
            last_net = Instant::now();
            // A network source sending to this device takes the sink from
            // the queue; it comes back when the source stops.
            if rooms.custody.is_none() {
                if let Some((source, r)) = rooms.offer() {
                    memory("before the room");
                    if let Some(h) = current.take() {
                        room_took_the_sink = true;
                        resume_after_room = true;
                        if let Err(e) = steward.release(h, t) {
                            warn!("release {}: {e:?}", h.0);
                        }
                    }
                    match steward.assign(
                        &source.shelf(),
                        Assignment {
                            subject: SubjectId::new(source.subject()),
                            instruction: to_value(&r),
                        },
                        t,
                    ) {
                        Ok(h) => {
                            info!(
                                "{}: {} from {} has the sink under custody {}",
                                source.label(),
                                r.group,
                                r.leader,
                                h.0
                            );
                            notice = None;
                            rooms.custody = Some((source, h));
                        }
                        Err(e) => {
                            warn!("{}: assign: {e:?}", source.label());
                            room_took_the_sink = false;
                            if resume_after_room {
                                resume_after_room = false;
                                current = advance(
                                    &mut steward,
                                    &mut queue,
                                    Step::Current,
                                    t,
                                    wanted,
                                    &mut notice,
                                );
                            }
                        }
                    }
                }
            }
            if let Some(event) = net.as_mut().and_then(|n| n.poll()) {
                let line = match &event {
                    net::Event::Up {
                        ssid,
                        address,
                        hostname,
                        channel,
                        rssi,
                    } => format!(
                        "wifi: joined '{ssid}' on channel {channel} at {rssi} dBm; hostname {hostname}, address {address}"
                    ),
                    net::Event::Down => "wifi: link down; rejoining".to_string(),
                    net::Event::Failed(why) => format!("wifi: off: {why}"),
                };
                config.set_network(line);
                config.write_status(&status_head);
                if let net::Event::Up { address, .. } = &event {
                    presence.set_address(address.parse().ok());
                    // Power save as the driver has it now (R37): the net
                    // thread turned it off at start, read back here.
                    // SAFETY: a plain getter on the started driver.
                    unsafe {
                        let mut ps = sys::wifi_ps_type_t_WIFI_PS_MIN_MODEM;
                        let got = sys::esp_wifi_get_ps(&mut ps);
                        if got != sys::ESP_OK
                            || ps != sys::wifi_ps_type_t_WIFI_PS_NONE
                        {
                            warn!("wifi: power save is on (mode {ps}, {got})");
                        }
                    }
                    if mdns.is_none() {
                        mdns = advertise(&hostname, &device_id, &identity);
                    }
                    // The server the card names is dialled once the
                    // link is up; with none named, the network is asked.
                    match &snapserver {
                        Some(s) => {
                            info!("snapcast: the card names {s}");
                            rooms.snap.set_server(Some(s.clone()));
                        }
                        None if search.is_none() => {
                            search_at = Some(Instant::now());
                        }
                        None => {}
                    }
                }
                if start_when_up && matches!(event, net::Event::Up { .. }) {
                    start_when_up = false;
                    current = advance(
                        &mut steward,
                        &mut queue,
                        Step::Next,
                        now(),
                        wanted,
                        &mut notice,
                    );
                    track_started = Instant::now();
                }
                if face.screen() == Screen::Settings {
                    face.set_about(facts(
                        &face,
                        card_mb,
                        card_tracks,
                        &network_line(&net),
                        &queue,
                        &device_id,
                    ));
                }
            }
            // The search for a Snapcast server: polled, never waited on.
            if let Some(s) = search.as_mut() {
                if let Some(found) = s.poll() {
                    search = None;
                    let first = found
                        .iter()
                        .find_map(|f| f.socket_v4().map(|a| (f, a)));
                    match first {
                        Some((f, addr)) => {
                            info!(
                                "snapcast: found {:?} on {}.local at {addr}{}",
                                f.instance,
                                f.host,
                                if found.len() > 1 {
                                    format!(" ({} answered)", found.len())
                                } else {
                                    String::new()
                                }
                            );
                            rooms.snap.set_server(Some(addr));
                        }
                        None => {
                            info!(
                                "snapcast: no server on the network; asking again in {} s",
                                SEARCH_AGAIN.as_secs()
                            );
                            search_at = Some(Instant::now() + SEARCH_AGAIN);
                        }
                    }
                }
            } else if let (Some(m), Some(at)) = (mdns.as_ref(), search_at) {
                if Instant::now() >= at {
                    search_at = None;
                    match m.search("_snapcast", "_tcp", SEARCH_FOR, 4) {
                        Ok(s) => search = Some(s),
                        Err(e) => {
                            warn!("{e}");
                            search_at = Some(Instant::now() + SEARCH_AGAIN);
                        }
                    }
                }
            }
            // The Volumio devices are not searched for: one that finds
            // this device connects and introduces itself, and the
            // presence keeps a session back to it (the crate says why a
            // search of our own would get us dropped from their lists).
            // A Volumio leader's word outranks the card's server and the
            // search: grouped, the device plays the leader's stream.
            while let Some(command) = presence.take_command() {
                match command {
                    GroupCommand::Join { host, id, name } => {
                        info!(
                            "volumio: grouped by {name:?} ({id}); its Snapcast server is at {host}"
                        );
                        search = None;
                        search_at = None;
                        rooms.snap.set_server(Some(host));
                    }
                    GroupCommand::Leave => {
                        info!("volumio: ungrouped; back to the card's server or the search");
                        match &snapserver {
                            Some(s) => rooms.snap.set_server(Some(s.clone())),
                            None => {
                                rooms.snap.set_server(None);
                                search_at = Some(Instant::now());
                            }
                        }
                    }
                }
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
            match pb.status {
                playback::Status::Opening => opening = true,
                playback::Status::Playing if opening => {
                    opening = false;
                    memory("with the station open");
                }
                _ => opening = false,
            }
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
            let source = rooms.source().map_or("Card", Source::label);
            let mut state = View::new(&pb, output.as_ref(), source);
            state.caution = net.as_ref().map_or("", |n| n.state.caution());
            presence.set_playing(playing(&pb));
            state.shuffle = queue.shuffle();
            state.repeat = queue.repeat();
            if current.is_none() {
                state.notice = notice.clone().unwrap_or_default();
            }
            if let Some(track) = pb.track.as_ref() {
                if art.track(track) {
                    face.set_art(None);
                    presence.set_art(None);
                }
            }
            if let Some(found) = art.poll() {
                face.set_art(found.art);
                presence.set_art(found.bytes.and_then(VolumioArt::sniff));
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
            if let HappeningKind::CustodyReleased { warden, reason } = &h.kind {
                if let Some(source) = Source::of_warden(warden) {
                    // The source is over: it left or went quiet. The
                    // queue plays again where it was. A release from the
                    // source that does not have the sink is stale.
                    match rooms.source() {
                        Some(s) if s == source => rooms.custody = None,
                        Some(_) => continue,
                        None => {}
                    }
                    memory("after the room");
                    notice = reason
                        .as_ref()
                        .map(|why| format!("{}: {why}", source.ended()));
                    if resume_after_room {
                        resume_after_room = false;
                        current = advance(
                            &mut steward,
                            &mut queue,
                            Step::Current,
                            now(),
                            wanted,
                            &mut notice,
                        );
                        track_started = Instant::now();
                    }
                    continue;
                }
                if room_took_the_sink {
                    // The decoder let go for the room; nothing moves on.
                    room_took_the_sink = false;
                    continue;
                }
                // A station that ends on its own (the fetcher gave up on
                // the connection) or could not be reached is held with
                // the reason, not followed by the next station unasked;
                // Play tries it again. A file's end, a file that failed,
                // and anything the listener let go of, moves on.
                let on_its_own = current.is_some();
                let ended_station =
                    on_its_own && queue.current().is_some_and(is_stream);
                if ended_station {
                    let title = queue.current().map(|t| t.title.clone());
                    let title = title.unwrap_or_default();
                    let why =
                        reason.as_deref().map_or("the stream ended", plain);
                    warn!("{title:?}: {why} ({reason:?})");
                    notice = Some(format!("{title}: {why}"));
                    current = None;
                } else {
                    if let Some(why) = reason {
                        warn!("the track failed: {why}; moving on");
                    }
                    let step = then.take().unwrap_or(if on_its_own {
                        Step::Next
                    } else {
                        Step::Skip
                    });
                    current = advance(
                        &mut steward,
                        &mut queue,
                        step,
                        now(),
                        wanted,
                        &mut notice,
                    );
                    if current.is_none() && queue.current().is_none() {
                        info!("the queue is at its end; the fabric idles");
                    }
                    track_started = Instant::now();
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
            info!("core: {}", shares.line());
            if let Some(line) = sink::report_dry() {
                warn!("sink: {line}");
            }
            let snap = rooms.snap.session();
            if snap.connected {
                info!(
                    "snapcast: {} {:?} {} {} Hz {} ch, buffer {} ms latency {} ms, clock {} us, volume {}{}, {} chunks, {} dropped, {} bad frames",
                    snap.server.as_deref().unwrap_or("?"),
                    snap.stream.as_deref().unwrap_or(""),
                    snap.codec.as_deref().unwrap_or("no codec yet"),
                    snap.rate,
                    snap.channels,
                    snap.buffer_ms,
                    snap.latency_ms,
                    snap.diff_us.map_or("?".to_string(), |d| d.to_string()),
                    snap.volume,
                    if snap.muted { " muted" } else { "" },
                    snap.chunks,
                    snap.dropped,
                    snap.bad_frames
                );
            } else if snapserver.is_none()
                && !presence.grouped()
                && rooms.snap.server().is_some()
                && search.is_none()
                && search_at.is_none()
            {
                // A server found on the network has gone: it may be back
                // at another address.
                search_at = Some(Instant::now() + SEARCH_AGAIN);
            }
            let (served, hearing) = presence.connections();
            if served + hearing > 0 || presence.grouped() {
                info!(
                    "volumio: {served} connected to this device, hearing {hearing} of {} known{}",
                    presence.peers().len(),
                    if presence.grouped() { ", grouped" } else { "" }
                );
            }
            if net.is_some() {
                info!("wifi: {}", net::link_line());
            }
        }
        // The idle task must run or its watchdog bites (5 s): a short
        // sleep when the fabric wants ticking now or a frame is
        // mid-paint, the tick length otherwise. While the decoder is
        // short of lead every millisecond is its, so the sleep then
        // comes once in [`IDLE_EVERY`]: a 96 kHz FLAC at the edge of the
        // core lost 7% to these sleeps (measured by the kernel's run-time
        // counters) and ran dry for it.
        let due_now = painting || due.is_some_and(|d| d.0 <= t.0);
        if due_now && audio_short && last_idle.elapsed() < IDLE_EVERY {
            std::thread::yield_now();
        } else {
            if due_now {
                last_idle = Instant::now();
            }
            std::thread::sleep(if due_now {
                Duration::from_millis(1)
            } else {
                TICK
            });
        }
    }
}

/// Symphonia's codecs with Espressif's MP3 decoder preferred: the chip
/// has no FPU, and Symphonia's floating-point MP3 reaches half of real
/// time on it.
fn codecs() -> CodecRegistry {
    let mut registry = org_evoframework_playback_symphonia::default_codecs();
    evo_esp32_platform_esp_idf::mp3::register(&mut registry);
    evo_esp32_platform_esp_idf::aac::register(&mut registry);
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
        MultiroomReceiver::new(
            Box::new(Silent),
            RoomSettings::new("", ""),
            Link::new(),
        )
        .describe(),
        SnapcastReceiver::new(
            Box::new(Silent),
            SnapSettings::new("", "", ""),
            SnapLink::new(),
        )
        .describe(),
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
/// The sink is shared between the decoder and the two network
/// receivers; the loop grants one of them custody at a time.
fn bring_up(
    sink: sink::Shared<Codec>,
    link: &Link,
    room: RoomSettings,
    snap_link: &SnapLink,
    snap: SnapSettings,
    now: Ticks,
) -> Result<Steward, String> {
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
            Box::new(
                PlaybackSymphonia::with_codecs(
                    Box::new(sink.clone()),
                    codecs(),
                )
                .with_opener(Box::new(HttpOpener)),
            ),
            now,
        )
        .map_err(|e| e.to_string())?;
    steward
        .admit_warden(
            Box::new(MultiroomReceiver::new(
                Box::new(sink.clone()),
                room,
                link.clone(),
            )),
            now,
        )
        .map_err(|e| e.to_string())?;
    steward
        .admit_warden(
            Box::new(SnapcastReceiver::new(
                Box::new(sink),
                snap,
                snap_link.clone(),
            )),
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
    root: &str,
    now: Ticks,
) -> Result<Vec<Track>, String> {
    let mut tracks = Vec::new();
    let mut pending = vec![root.to_string()];
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

/// The playback projection's state: the decoder's (`playback/now`) or
/// the room's (`playback/room`), whichever is not stopped; the
/// decoder's when both are.
fn playback_state(steward: &Steward) -> Option<playback::State> {
    let states: Vec<playback::State> = steward
        .projection(ProjectionKey::Rack {
            name: shelf::PLAYBACK.into(),
        })
        .subjects
        .iter()
        .filter_map(|s| from_value(&s.state).ok())
        .collect();
    states
        .iter()
        .find(|s| s.status != playback::Status::Stopped)
        .or(states.first())
        .cloned()
}

/// What plays, as Volumio devices are told it: opening counts as
/// playing, since Volumio has no word for it.
fn playing(pb: &playback::State) -> Playing {
    let status = match pb.status {
        playback::Status::Playing | playback::Status::Opening => {
            volumio::Status::Play
        }
        playback::Status::Paused => volumio::Status::Pause,
        _ => volumio::Status::Stop,
    };
    let track = pb.track.as_ref();
    Playing {
        status,
        title: track.map(|t| t.title.clone()).unwrap_or_default(),
        artist: track.and_then(|t| t.artist.clone()).unwrap_or_default(),
        album: track.and_then(|t| t.album.clone()).unwrap_or_default(),
    }
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
/// Who had the core since the last call: each task's share of the run
/// time, from the kernel's counters. The counters are 32-bit
/// microseconds and wrap in 71 minutes; the deltas are taken wrapping.
struct CoreShares {
    /// By the kernel's task number: two tasks may share a name.
    last: std::collections::HashMap<u32, u32>,
    last_total: u32,
}

impl CoreShares {
    fn new() -> Self {
        Self {
            last: std::collections::HashMap::new(),
            last_total: 0,
        }
    }

    /// One line, the largest share first: `main 71%  wifi 9%  IDLE 8%`;
    /// tasks under one percent left out.
    fn line(&mut self) -> String {
        // SAFETY: the array is sized by the kernel's task count with room
        // for tasks created meanwhile; the kernel fills what fits and
        // says how many.
        let (statuses, total) = unsafe {
            let n = sys::uxTaskGetNumberOfTasks() as usize + 4;
            let mut buf: Vec<sys::TaskStatus_t> = vec![std::mem::zeroed(); n];
            let mut total: u32 = 0;
            let got = sys::uxTaskGetSystemState(
                buf.as_mut_ptr(),
                n as u32,
                &mut total,
            );
            buf.truncate(got as usize);
            (buf, total)
        };
        let period = total.wrapping_sub(self.last_total);
        self.last_total = total;
        let mut shares: Vec<(String, u32)> = Vec::new();
        let mut seen = std::collections::HashMap::new();
        for s in &statuses {
            // SAFETY: the kernel's task name, NUL-terminated, alive while
            // the task is.
            let name = unsafe { std::ffi::CStr::from_ptr(s.pcTaskName) }
                .to_string_lossy()
                .into_owned();
            let was = self.last.get(&s.xTaskNumber).copied().unwrap_or(0);
            let ran = s.ulRunTimeCounter.wrapping_sub(was);
            seen.insert(s.xTaskNumber, s.ulRunTimeCounter);
            let name = format!("{name}/{}", s.uxCurrentPriority);
            if period > 0 {
                let pct = (u64::from(ran) * 100 / u64::from(period)) as u32;
                if pct >= 1 {
                    shares.push((name, pct));
                }
            }
        }
        self.last = seen;
        shares.sort_by_key(|a| std::cmp::Reverse(a.1));
        shares
            .iter()
            .map(|(n, p)| format!("{n} {p}%"))
            .collect::<Vec<_>>()
            .join("  ")
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
/// How the queue moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// The item in hand again (a held station, a pause's end), else the
    /// next.
    Current,
    /// The next, a track having ended on its own (repeat-one repeats).
    Next,
    /// The next, asked for (repeat-one moves on).
    Skip,
    /// The previous.
    Previous,
}

/// Move the queue by `step` and play what it lands on, or the first
/// after it that opens: a file that will not play is passed over. A
/// station that will not play is not: the one asked for stays on the
/// face with the reason in `notice`, rather than another station
/// starting unasked.
fn advance(
    steward: &mut Steward,
    queue: &mut Queue<Track>,
    step: Step,
    now: Ticks,
    level: u8,
    notice: &mut Option<String>,
) -> Option<CustodyHandle> {
    let mut step = step;
    // A queue of nothing but files above the ceiling would loop on
    // repeat; one pass over it is enough to know.
    for _ in 0..queue.len().max(1) {
        let track = match step {
            Step::Current => queue.current(),
            Step::Next => queue.next(true),
            Step::Skip => queue.next(false),
            Step::Previous => queue.previous(),
        };
        let track = match track {
            Some(t) => t.clone(),
            None if step == Step::Current => {
                step = Step::Skip;
                continue;
            }
            None => return None,
        };
        step = Step::Skip;
        if !within_ceiling(&track) {
            info!(
                "{:?} is {} Hz, above this board's ceiling of {RATE_CEILING} Hz; passed over unopened",
                track.title,
                track.sample_rate.unwrap_or(0)
            );
            continue;
        }
        if is_stream(&track) {
            memory("before the station");
        }
        match play(steward, &track, now, level) {
            Ok(handle) => {
                *notice = None;
                return Some(handle);
            }
            Err(why) if is_stream(&track) => {
                *notice = Some(format!("{}: {}", track.title, plain(&why)));
                return None;
            }
            Err(_) => continue,
        }
    }
    None
}

/// The name a repeat mode is written as.
fn repeat_name(repeat: Repeat) -> &'static str {
    match repeat {
        Repeat::Off => "off",
        Repeat::All => "all",
        Repeat::One => "one",
    }
}

/// What `start` names: `(items, the index to start at)`. A playlist's
/// stations, with `#Name` picking one; a folder's files, all of them,
/// deep; a file, with its folder round it.
fn starting_point(
    steward: &mut Steward,
    start: &str,
    now: Ticks,
) -> Result<(Vec<Track>, Option<usize>), String> {
    let (path, pick) = match start.split_once('#') {
        Some((p, n)) => (p.trim_end_matches('/'), Some(n.trim())),
        None => (start.trim_end_matches('/'), None),
    };
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".m3u")
        || lower.ends_with(".m3u8")
        || lower.ends_with(".pls")
    {
        let listing = list_folder(steward, path, now)?;
        let items: Vec<Track> = listing
            .entries
            .into_iter()
            .filter_map(|e| match e {
                Entry::Track(t) => Some(t),
                _ => None,
            })
            .collect();
        if items.is_empty() {
            return Err(format!("{path}: no station in it"));
        }
        let at = match pick {
            Some(name) if !name.is_empty() => Some(
                items
                    .iter()
                    .position(|t| t.title.eq_ignore_ascii_case(name))
                    .ok_or_else(|| {
                        format!("{path}: no station called {name:?}")
                    })?,
            ),
            _ => None,
        };
        return Ok((items, at));
    }
    // A folder, or a file in one.
    let (folder, file) = match list_folder(steward, path, now) {
        Ok(_) => (path.to_string(), None),
        Err(_) => match path.rsplit_once('/') {
            Some((f, name)) => (f.to_string(), Some(name)),
            None => (String::new(), Some(path)),
        },
    };
    let mut items = list_tracks(steward, &folder, now)?;
    items.retain(|t| !is_stream(t));
    if items.is_empty() {
        return Err(format!("{path}: nothing to play in it"));
    }
    let at = match file {
        Some(name) => Some(
            items
                .iter()
                .position(|t| t.uri.ends_with(&format!("/{name}")))
                .ok_or_else(|| format!("{path}: not found"))?,
        ),
        None => None,
    };
    Ok((items, at))
}

/// A refusal's reason in the listener's words, for the status strip.
fn plain(why: &str) -> &'static str {
    if why.contains("stalled") {
        "the stream stalled"
    } else if why.contains("end of stream") || why.contains("Eof") {
        "the stream ended"
    } else if why.contains("HTTP_CONNECT") || why.contains("getaddrinfo") {
        "could not connect"
    } else if why.contains("HTTP 404") {
        "not found (404)"
    } else if why.contains("HTTP ") {
        "the server refused"
    } else if why.contains("os error 12") || why.contains("NO_MEM") {
        "out of memory"
    } else if why.contains("unsupported")
        || why.contains("codec")
        || why.contains("not supported")
    {
        "format not supported here"
    } else {
        "could not open"
    }
}

/// Whether the source's listing says the track's rate is one this board
/// plays; a track whose rate the listing does not know is tried.
fn within_ceiling(track: &Track) -> bool {
    track.sample_rate.is_none_or(|rate| rate <= RATE_CEILING)
}

/// The face's background as the raw RGB565 a picture is composed on.
fn background(face: &Face) -> u16 {
    evo_esp32_face::art::raw(face.theme().background)
}

/// A track from the network rather than the card.
fn is_stream(track: &Track) -> bool {
    track.uri.starts_with("http://") || track.uri.starts_with("https://")
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

/// The chip's factory MAC, from eFuse; zeros if it cannot be read.
fn factory_mac() -> [u8; 6] {
    let mut mac = [0u8; 6];
    // SAFETY: esp_efuse_mac_get_default writes six bytes into the buffer
    // given; no other preconditions.
    let read = unsafe { sys::esp_efuse_mac_get_default(mac.as_mut_ptr()) };
    if read != sys::ESP_OK {
        warn!("efuse: no MAC ({read}); the id is the zero MAC's");
    }
    mac
}

/// The MAC as `aa:bb:cc:dd:ee:ff`: what a Snapcast server knows a
/// client by.
fn mac_text() -> String {
    factory_mac()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// The device's id on the Evo network: a UUID derived from the chip's
/// factory MAC, so it is the same across boots, cards and flashes (a
/// full flash blanks NVS; the MAC is in eFuse). Its first digit is `f`:
/// a group's source-host is elected as the lowest id among the members,
/// and a device that only receives must never win that, so it sorts
/// itself last. Version and variant bits are a UUID's.
fn identity() -> String {
    let mac = factory_mac();
    // Sixteen bytes from six: FNV-1a over the MAC under two seeds.
    let mut b = [0u8; 16];
    for (half, seed) in [
        (0usize, 0xcbf2_9ce4_8422_2325u64),
        (8, 0x8422_2325_cbf2_9ce4),
    ] {
        let mut h = seed;
        for byte in mac {
            h ^= u64::from(byte);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        b[half..half + 8].copy_from_slice(&h.to_be_bytes());
    }
    b[0] |= 0xf0;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Advertise the device on the local network: `hostname.local`, the
/// Evo audio plane's service with the id a source-host looks for, and
/// the service Volumio devices look for each other by.
fn advertise(
    hostname: &str,
    device_id: &str,
    identity: &Identity,
) -> Option<Advertiser> {
    let started = Advertiser::start(hostname, hostname).and_then(|mut m| {
        m.add_service(
            "_evo",
            "_tcp",
            org_evoframework_multiroom_receiver::CONTROL_PORT,
            &[
                ("id", device_id),
                ("name", hostname),
                ("vendor", "evo-esp32"),
                ("version", env!("CARGO_PKG_VERSION")),
                ("caps", "multi-room"),
            ],
        )?;
        let txt = volumio::txt(identity);
        let txt: Vec<(&str, &str)> =
            txt.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        m.add_service(volumio::SERVICE, "_tcp", volumio::PORT, &txt)?;
        Ok(m)
    });
    match started {
        Ok(m) => {
            info!(
                "mdns: {hostname}.local, _evo._tcp on port {} as {device_id}, {}._tcp on port {}",
                org_evoframework_multiroom_receiver::CONTROL_PORT,
                volumio::SERVICE,
                volumio::PORT
            );
            Some(m)
        }
        Err(e) => {
            warn!("mdns: {e}; the device is not discoverable");
            None
        }
    }
}

/// A network source that may take the sink from the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    /// An Evo source-host sending to this room.
    Evo,
    /// A Snapcast server streaming to this client.
    Snapcast,
}

impl Source {
    /// The face's word for where the sound comes from.
    fn label(self) -> &'static str {
        match self {
            Source::Evo => "Evo room",
            Source::Snapcast => "Snapcast",
        }
    }

    /// The notice when the source lets go.
    fn ended(self) -> &'static str {
        match self {
            Source::Evo => "the room ended",
            Source::Snapcast => "the Snapcast stream ended",
        }
    }

    fn shelf(self) -> evo_esp32_sdk::catalogue::ShelfRef {
        match self {
            Source::Evo => shelf::playback_receive(),
            Source::Snapcast => shelf::playback_snapcast(),
        }
    }

    fn subject(self) -> &'static str {
        match self {
            Source::Evo => "playback/room",
            Source::Snapcast => "playback/snapcast",
        }
    }

    /// The source whose warden `name` is.
    fn of_warden(name: &str) -> Option<Self> {
        if name == org_evoframework_multiroom_receiver::NAME {
            Some(Source::Evo)
        } else if name == org_evoframework_snapcast_receiver::NAME {
            Some(Source::Snapcast)
        } else {
            None
        }
    }
}

/// The network sources, the custody while one of them has the sink,
/// and the connection of each the listener declined.
struct Rooms {
    evo: Link,
    snap: SnapLink,
    custody: Option<(Source, CustodyHandle)>,
    declined: [Option<u32>; 2],
}

impl Rooms {
    fn new(evo: Link, snap: SnapLink) -> Self {
        Self {
            evo,
            snap,
            custody: None,
            declined: [None, None],
        }
    }

    fn handle(&self) -> Option<CustodyHandle> {
        self.custody.map(|(_, h)| h)
    }

    fn source(&self) -> Option<Source> {
        self.custody.map(|(s, _)| s)
    }

    fn generation(&self, source: Source) -> u32 {
        match source {
            Source::Evo => self.evo.session().generation,
            Source::Snapcast => self.snap.session().generation,
        }
    }

    /// A source that is sending on a connection the listener did not
    /// decline, with the instruction to assign; the Evo room first.
    fn offer(&self) -> Option<(Source, Receive)> {
        let offers = [
            (Source::Evo, self.evo.sending()),
            (Source::Snapcast, self.snap.sending()),
        ];
        offers.into_iter().find_map(|(source, r)| {
            let r = r?;
            (self.declined[source as usize] != Some(self.generation(source)))
                .then_some((source, r))
        })
    }

    /// The listener takes the sink back: the source's custody ends and
    /// this connection of it is not granted the sink again (a new one
    /// is). True when there was a custody to end; the caller decides
    /// whether the queue resumes.
    fn take_back(&mut self, steward: &mut Steward, now: Ticks) -> bool {
        let Some((source, h)) = self.custody.take() else {
            return false;
        };
        info!("touch: the {} gives the sink back", source.label());
        self.declined[source as usize] = Some(self.generation(source));
        if let Err(e) = steward.release(h, now) {
            warn!("release {}: {e:?}", h.0);
        }
        true
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
) -> Result<CustodyHandle, String> {
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
            Ok(handle)
        }
        Err(e) => {
            warn!("assign {:?}: {e:?}", track.title);
            Err(format!("{e:?}"))
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
    let (free, largest, psram, psram_largest) = unsafe {
        (
            sys::heap_caps_get_free_size(sys::MALLOC_CAP_INTERNAL),
            sys::heap_caps_get_largest_free_block(sys::MALLOC_CAP_INTERNAL),
            sys::heap_caps_get_free_size(sys::MALLOC_CAP_SPIRAM),
            sys::heap_caps_get_largest_free_block(sys::MALLOC_CAP_SPIRAM),
        )
    };
    info!(
        "memory: internal {free} B free, largest block {largest} B; PSRAM {} KB free, largest {} KB, {when}",
        psram / 1024,
        psram_largest / 1024
    );
}

/// What the Wi-Fi row says.
fn network_line(net: &Option<net::Net>) -> String {
    match net {
        Some(net) => net.state.line(),
        None => "not set".to_string(),
    }
}

fn facts(
    face: &Face,
    card_mb: u64,
    tracks: usize,
    network: &str,
    queue: &Queue<Track>,
    device_id: &str,
) -> About {
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
                "Shuffle \u{203a}",
                if queue.shuffle() { "on" } else { "off" }.to_string(),
            ),
            fact("Repeat \u{203a}", repeat_name(queue.repeat()).to_string()),
            fact(
                "Card",
                format!("{:.0} GB, {tracks} tracks", card_mb as f64 / 1024.0),
            ),
            fact("Evo id", device_id.to_string()),
        ],
        first: 0,
    }
}

/// The settings rows a tap acts on, by their place in [`facts`].
enum SettingsRow {
    Theme,
    Style,
    Rotation,
    Shuffle,
    Repeat,
}

fn settings_row(i: usize) -> Option<SettingsRow> {
    match i {
        4 => Some(SettingsRow::Theme),
        5 => Some(SettingsRow::Style),
        6 => Some(SettingsRow::Rotation),
        7 => Some(SettingsRow::Shuffle),
        8 => Some(SettingsRow::Repeat),
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

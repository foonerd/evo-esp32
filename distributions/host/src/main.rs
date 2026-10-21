// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The host distribution: the fabric playing a directory of music on a
//! desktop, decoded for real into a sink made of a clock and a file.
//!
//! What a firmware distribution does, done here without a board: the
//! catalogue compiled in, the plugins admitted, a consumer loop that
//! reads projections and the happenings stream. The consumer is the
//! panel: it draws the player's face from the projections and, on the
//! host, writes the last frame as a PNG. The sink paces the decoder at
//! the sample rate as a DAC would and, if asked, records what it let out
//! as a WAV, so the run leaves something to listen to.
//!
//! ```text
//! evo-esp32-host --dir ~/Music --seconds 5 --panel face.png --wav heard.wav
//! evo-esp32-host --gallery target/faces      # every rotation, style and theme
//! ```

mod http;
mod panel;
mod sink;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use evo_esp32_face::{
    art, theme, About, Art, Entry as Row, Fact, Library, Screen, Style, Theme,
};
use evo_esp32_sdk::catalogue::{Catalogue, ShelfRef};
use evo_esp32_sdk::contract::audio::playback::{self, Play};
use evo_esp32_sdk::contract::audio::source::{Entry, ListArgs, Listing, LIST};
use evo_esp32_sdk::contract::audio::{
    delivery, from_value, shelf, to_value, Track,
};
use evo_esp32_sdk::happening::Seq;
use evo_esp32_sdk::plugin::{Plugin, PluginDescription};
use evo_esp32_sdk::projection::ProjectionKey;
use evo_esp32_sdk::respondent::{CorrelationId, Request};
use evo_esp32_sdk::subject::SubjectId;
use evo_esp32_sdk::warden::Assignment;
use evo_esp32_sdk::Ticks;
use evo_esp32_steward::{Config, Steward};
use org_evoframework_delivery_null::DeliveryNull;
use org_evoframework_playback_symphonia::PlaybackSymphonia;
use org_evoframework_source_dlna::SourceDlna;
use org_evoframework_source_files::SourceFiles;
use org_evoframework_source_smb::{Share, SmbOpener, SourceSmb};
use sink::PacedWav;

mod generated {
    //! `catalogue()`, from `catalogue.toml` at build time.
    extern crate alloc;
    include!(concat!(env!("OUT_DIR"), "/catalogue.rs"));
}

/// The plugins' `log` lines, to stderr, at the level `EVO_LOG` names.
mod logger {
    use log::{Level, LevelFilter, Log, Metadata, Record};

    struct Stderr;

    impl Log for Stderr {
        fn enabled(&self, m: &Metadata) -> bool {
            m.level() <= log::max_level()
        }

        fn log(&self, r: &Record) {
            if self.enabled(r.metadata()) {
                let tag = match r.level() {
                    Level::Error => "E",
                    Level::Warn => "W",
                    Level::Info => "I",
                    Level::Debug => "D",
                    Level::Trace => "T",
                };
                eprintln!("{tag} {}: {}", r.target(), r.args());
            }
        }

        fn flush(&self) {}
    }

    static LOGGER: Stderr = Stderr;

    pub fn install() {
        let level = match std::env::var("EVO_LOG").as_deref() {
            Ok("trace") => LevelFilter::Trace,
            Ok("debug") => LevelFilter::Debug,
            Ok("warn") => LevelFilter::Warn,
            Ok("error") => LevelFilter::Error,
            Ok("off") => LevelFilter::Off,
            _ => LevelFilter::Info,
        };
        if log::set_logger(&LOGGER).is_ok() {
            log::set_max_level(level);
        }
    }
}

const USAGE: &str = "\
evo-esp32-host: the evo-esp32 fabric playing a directory on a desktop

  --dir PATH        the music directory (default .)
  --dlna NAME       play from the DLNA server of that name (any part of it)
                    instead: the first track found walking its folders,
                    those called Music first
  --iface ADDR      the address of the interface the search goes out of,
                    on a host with several
  --share URL       a network share, smb://host/share[/folder]; with
                    --share-user and --share-password where it needs them.
                    --play-share plays its first track found walking it
  --play-share      play from the share rather than the directory
  --seconds N       how long to run (default 5)
  --start MS        start the track this many milliseconds in (a seek at open)
  --panel PATH      write the panel's last frame as a PNG
  --sheet           with the details sheet raised over it
  --rotation DEG    the panel's rotation, 0, 90, 180 or 270 (default 0)
  --style NAME      the face's style, disc or card (default disc)
  --theme NAME      the face's theme: evo-default, night-sky, sunrise, air, liquid
  --gallery DIR     write a frame for every rotation, style and theme there and exit
  --wav PATH        record what the sink let out as a WAV
  --tone PATH       write a 440 Hz test tone (2 s, 44.1 kHz) there and exit
  --help

The plugins' log goes to stderr; EVO_LOG=debug (or trace, warn, error)
sets the level, info by default.";

struct Cli {
    dir: PathBuf,
    dlna: Option<String>,
    iface: Option<std::net::Ipv4Addr>,
    share: Option<String>,
    share_user: Option<String>,
    share_password: Option<String>,
    play_share: bool,
    seconds: u64,
    start_ms: u64,
    panel: Option<PathBuf>,
    sheet: bool,
    rotation: u16,
    style: Style,
    theme: Theme,
    gallery: Option<PathBuf>,
    wav: Option<PathBuf>,
    tone: Option<PathBuf>,
}

fn parse_args() -> Result<Cli, String> {
    let mut cli = Cli {
        dir: PathBuf::from("."),
        dlna: None,
        iface: None,
        share: None,
        share_user: None,
        share_password: None,
        play_share: false,
        seconds: 5,
        start_ms: 0,
        panel: None,
        sheet: false,
        rotation: 0,
        style: Style::Disc,
        theme: theme::EVO_DEFAULT,
        gallery: None,
        wav: None,
        tone: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        if flag == "--help" || flag == "-h" {
            println!("{USAGE}");
            std::process::exit(0);
        }
        if flag == "--sheet" {
            cli.sheet = true;
            continue;
        }
        if flag == "--play-share" {
            cli.play_share = true;
            continue;
        }
        let value = args
            .next()
            .ok_or_else(|| format!("{flag} needs a value\n{USAGE}"))?;
        match flag.as_str() {
            "--dir" => cli.dir = PathBuf::from(value),
            "--dlna" => cli.dlna = Some(value),
            "--share" => cli.share = Some(value),
            "--share-user" => cli.share_user = Some(value),
            "--share-password" => cli.share_password = Some(value),
            "--iface" => {
                cli.iface = Some(value.parse().map_err(|_| {
                    format!("--iface must be an IPv4 address, got {value:?}")
                })?);
            }
            "--seconds" => {
                cli.seconds = value.parse().map_err(|_| {
                    format!("--seconds must be a number, got {value:?}")
                })?;
            }
            "--start" => {
                cli.start_ms = value.parse().map_err(|_| {
                    format!("--start must be milliseconds, got {value:?}")
                })?;
            }
            "--panel" => cli.panel = Some(PathBuf::from(value)),
            "--gallery" => cli.gallery = Some(PathBuf::from(value)),
            "--wav" => cli.wav = Some(PathBuf::from(value)),
            "--tone" => cli.tone = Some(PathBuf::from(value)),
            "--style" => {
                cli.style = Style::named(&value).ok_or_else(|| {
                    format!("--style must be disc or card, got {value:?}")
                })?;
            }
            "--theme" => {
                cli.theme = Theme::named(&value).ok_or_else(|| {
                    format!(
                        "--theme must be one of {}, got {value:?}",
                        theme::ALL
                            .iter()
                            .map(|t| t.name)
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })?;
            }
            "--rotation" => {
                cli.rotation = match value.parse() {
                    Ok(r @ (0 | 90 | 180 | 270)) => r,
                    _ => {
                        return Err(format!(
                        "--rotation must be 0, 90, 180 or 270, got {value:?}"
                    ))
                    }
                };
            }
            other => return Err(format!("unknown option {other}\n{USAGE}")),
        }
    }
    Ok(cli)
}

/// The catalogue this distribution was built with.
pub fn catalogue() -> Catalogue {
    generated::catalogue()
}

/// The plugins this distribution admits, in admission order, described
/// so the catalogue can be checked against them before any is loaded.
fn descriptions(dir: &std::path::Path) -> Vec<PluginDescription> {
    vec![
        SourceFiles::new(dir).describe(),
        SourceDlna::new().describe(),
        SourceSmb::new(Vec::new()).describe(),
        PlaybackSymphonia::new(Box::new(PacedWav::new(None))).describe(),
        DeliveryNull::new().describe(),
    ]
}

/// Build the fabric: steward, plugins, checked against the catalogue.
/// The decoder writes to a paced sink that records to `wav` if given.
fn bring_up(
    dir: &std::path::Path,
    iface: Option<std::net::Ipv4Addr>,
    shares: Vec<Share>,
    wav: Option<PathBuf>,
    now: Ticks,
) -> Result<Steward, String> {
    let catalogue = catalogue();
    let report = evo_esp32_catalogue::check(&catalogue, &descriptions(dir));
    if !report.is_clean() {
        return Err(format!("the catalogue is not stocked:\n{report}"));
    }
    let mut steward = Steward::new(catalogue, Config::default())
        .map_err(|e| e.to_string())?;
    steward
        .admit_respondent(Box::new(SourceFiles::new(dir)), now)
        .map_err(|e| e.to_string())?;
    steward
        .admit_respondent(
            Box::new(match iface {
                Some(a) => SourceDlna::from_interface(a),
                None => SourceDlna::new(),
            }),
            now,
        )
        .map_err(|e| e.to_string())?;
    steward
        .admit_respondent(Box::new(SourceSmb::new(shares.clone())), now)
        .map_err(|e| e.to_string())?;
    steward
        .admit_warden(
            Box::new(
                PlaybackSymphonia::new(Box::new(PacedWav::new(wav)))
                    .with_opener(Box::new(http::PlainHttp))
                    .with_file_opener(Box::new(SmbOpener::new(shares.clone())))
                    .with_file_opener(Box::new(SmbOpener::smb1(shares))),
            ),
            now,
        )
        .map_err(|e| e.to_string())?;
    steward
        .admit_respondent(Box::new(DeliveryNull::new()), now)
        .map_err(|e| e.to_string())?;
    Ok(steward)
}

/// Lists one path of a source, printing what came back.
fn list(
    steward: &mut Steward,
    source: &ShelfRef,
    cid: u32,
    path: &str,
    now: Ticks,
) -> Result<Listing, String> {
    let response = steward
        .request(
            &Request {
                cid: CorrelationId(cid),
                shelf: source.clone(),
                verb: LIST.into(),
                subject: None,
                args: to_value(&ListArgs { path: path.into() }),
            },
            now,
        )
        .map_err(|e| format!("{e:?}"))?;
    let listing: Listing = match response.outcome {
        Ok(v) => from_value(&v)?,
        Err(f) => return Err(format!("list: {f:?}")),
    };
    println!(
        "{}/{} lists {} entries in {:?}:",
        source.rack,
        source.shelf,
        listing.entries.len(),
        listing.path
    );
    for e in &listing.entries {
        match e {
            Entry::Folder { name, .. } => println!("  {name}/"),
            Entry::Track(t) => println!("  {}", t.title),
        }
    }
    Ok(listing)
}

/// The first track found walking a source's folders depth first from
/// `root`, folders called Music ahead of the others at every level
/// (a directory holding one playlist plays that playlist's first
/// station); the walk is bounded so a wide library does not take the
/// run with it.
fn first_track(
    steward: &mut Steward,
    source: ShelfRef,
    root: String,
    now: Ticks,
) -> Result<Option<Track>, String> {
    const LISTS: u32 = 40;
    let mut pending = vec![root];
    for cid in 1..=LISTS {
        let Some(path) = pending.pop() else { break };
        let listing = list(steward, &source, cid, &path, now)?;
        let mut folders = Vec::new();
        for e in listing.entries {
            match e {
                Entry::Track(t) => return Ok(Some(t)),
                Entry::Folder { path, name } => {
                    folders.push((
                        name.to_ascii_lowercase().contains("music"),
                        path,
                    ));
                }
            }
        }
        // Popped from the end: the last pushed is walked first.
        folders.sort_by_key(|(music, _)| *music);
        pending.extend(folders.into_iter().map(|(_, p)| p));
    }
    Ok(None)
}

/// The path of the DLNA server whose name has `name` in it, once the
/// search has found it; the source is polled since the search runs on
/// its own thread.
fn dlna_server(
    steward: &mut Steward,
    name: &str,
    now: Ticks,
) -> Result<Option<String>, String> {
    let wanted = name.to_ascii_lowercase();
    for cid in 1..=20u32 {
        let listing =
            list(steward, &shelf::source_dlna(), 1000 + cid, "", now)?;
        for e in listing.entries {
            if let Entry::Folder { path, name } = e {
                if name.to_ascii_lowercase().contains(&wanted) {
                    return Ok(Some(path));
                }
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Ok(None)
}

/// A frame of the face for every rotation, style and theme, from one
/// made-up state, written to `dir` as `face-<rotation>-<style>-<theme>.png`:
/// the review surface for the face, the same code the board draws with.
fn gallery(dir: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    let pb = playback::State {
        status: playback::Status::Playing,
        track: Some(Track {
            uri: "file:///Music/Dire Straits/Brothers In Arms/02. Money For Nothing.flac".into(),
            title: "Money For Nothing".into(),
            artist: Some("Dire Straits".into()),
            album: Some("Brothers In Arms".into()),
            duration_ms: Some(506_000),
            sample_rate: None,
            art: None,
            live: false,
        }),
        position_ms: 119_000,
        format: Some(playback::Format {
            codec: "flac".into(),
            sample_rate: 96_000,
            bits: 16,
            channels: 2,
        }),
    };
    let out = delivery::State {
        sink: "i2s".into(),
        volume: 40,
        muted: false,
    };
    let view = panel::View::new(&pb, Some(&out), "Card");
    for rotation in [0u16, 90, 180, 270] {
        for style in [Style::Disc, Style::Card] {
            for t in theme::ALL {
                let mut p = panel::Panel::new(rotation, style, *t);
                let side = p.face_mut().layout().art.size.width;
                p.face_mut().set_art(sample_art(side));
                p.draw(&view);
                let path = dir.join(format!(
                    "face-{rotation}-{}-{}.png",
                    style.name(),
                    t.name
                ));
                p.save(&path)?;
                println!("{}", path.display());
            }
        }
        // The list pages, in the default theme: an album open in the
        // library with the track in hand among its rows, and the facts.
        let mut p =
            panel::Panel::new(rotation, Style::Disc, theme::EVO_DEFAULT);
        p.face_mut().set_screen(Screen::Library);
        p.face_mut().set_library(sample_library());
        p.draw(&view);
        let path = dir.join(format!("library-{rotation}.png"));
        p.save(&path)?;
        println!("{}", path.display());
        p.face_mut().set_screen(Screen::Settings);
        p.face_mut().set_about(sample_about());
        p.draw(&view);
        let path = dir.join(format!("settings-{rotation}.png"));
        p.save(&path)?;
        println!("{}", path.display());
    }
    Ok(())
}

/// A made-up cover at `side` pixels: a warm-to-dark sweep with a pale
/// disc, enough texture to judge the disc, the card and its scrim by.
fn sample_art(side: u32) -> Option<Art> {
    let n = side as i32;
    let pixels: Vec<u16> = (0..n * n)
        .map(|i| {
            let (x, y) = (i % n, i / n);
            let (dx, dy) = (x - n * 2 / 3, y - n / 3);
            let (r, g, b) = if dx * dx + dy * dy < (n / 5).pow(2) {
                (29u16, 54u16, 24u16)
            } else {
                let t = (x + y) as u16 * 31 / (2 * n as u16);
                (31 - t / 2, 20 + t, 4 + t / 2)
            };
            (r << 11) | (g << 5) | b
        })
        .collect();
    art::fit(side, side, &pixels, side)
}

fn sample_library() -> Library {
    let album = "file:///Music/Dire Straits/Brothers In Arms";
    let tracks = [
        "01. So Far Away",
        "02. Money For Nothing",
        "03. Walk Of Life",
        "04. Your Latest Trick",
        "05. Why Worry",
        "06. Ride Across The River",
        "07. The Man's Too Strong",
        "08. One World",
        "09. Brothers In Arms",
    ];
    Library {
        path: "Dire Straits/Brothers In Arms".into(),
        name: "Brothers In Arms".into(),
        entries: tracks
            .iter()
            .map(|t| Row {
                name: t.to_string(),
                folder: false,
                uri: Some(format!("{album}/{t}.flac")),
                playable: !t.starts_with("07"),
            })
            .collect(),
        first: 0,
    }
}

fn sample_about() -> About {
    let fact = |name: &str, value: &str| Fact {
        name: name.into(),
        value: value.into(),
    };
    About {
        facts: vec![
            fact("Player", "evo-esp32 0.1.0"),
            fact("Wi-Fi", "192.168.1.23 (5 GHz)"),
            fact("Build", "a7fd9d9"),
            fact("Board", "Waveshare ESP32-C5 2.8"),
            fact("Theme", "Evo Default"),
            fact("Style", "disc"),
            fact("Rotation", "0"),
            fact("Card", "29 GB, 54 tracks"),
        ],
        first: 0,
    }
}

fn main() {
    logger::install();
    let cli = match parse_args() {
        Ok(cli) => cli,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let start = Instant::now();
    let now = || Ticks(start.elapsed().as_millis() as u64);

    if let Some(path) = &cli.tone {
        if let Err(e) = sink::write_tone(path, 44_100, 2000) {
            eprintln!("tone: {}: {e}", path.display());
            std::process::exit(1);
        }
        println!("tone written to {}", path.display());
        return;
    }

    if let Some(dir) = &cli.gallery {
        if let Err(e) = gallery(dir) {
            eprintln!("gallery: {e}");
            std::process::exit(1);
        }
        return;
    }

    let shares = match &cli.share {
        Some(url) => match Share::parse(
            url,
            cli.share_user.as_deref(),
            cli.share_password.as_deref(),
            None,
        ) {
            Ok(s) => vec![s],
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(2);
            }
        },
        None => Vec::new(),
    };
    let mut steward =
        match bring_up(&cli.dir, cli.iface, shares, cli.wav.clone(), now()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        };
    println!(
        "steward up with {} plugins: {}",
        steward.plugins().count(),
        steward
            .plugins()
            .map(|p| p.name)
            .collect::<Vec<_>>()
            .join(", ")
    );

    let track = if cli.play_share {
        match first_track(
            &mut steward,
            shelf::source_smb(),
            String::new(),
            now(),
        ) {
            Ok(Some(t)) => t,
            Ok(None) => {
                eprintln!("no track found on the share");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
    } else if let Some(name) = &cli.dlna {
        // The search runs on its thread; a server takes a couple of
        // seconds to answer.
        let server = match dlna_server(&mut steward, name, now()) {
            Ok(Some(s)) => s,
            Ok(None) => {
                eprintln!(
                    "no DLNA server called {name:?} answered in ten seconds"
                );
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        };
        match first_track(&mut steward, shelf::source_dlna(), server, now()) {
            Ok(Some(t)) => t,
            Ok(None) => {
                eprintln!("no track found on {name:?}");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
    } else {
        match first_track(
            &mut steward,
            shelf::source_files(),
            String::new(),
            now(),
        ) {
            Ok(Some(t)) => t,
            Ok(None) => {
                eprintln!("no audio file in {}", cli.dir.display());
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
    };

    let handle = steward
        .assign(
            &shelf::playback_decode(),
            Assignment {
                subject: SubjectId::new("playback/now"),
                instruction: to_value(&Play {
                    track: track.clone(),
                    start_ms: cli.start_ms,
                }),
            },
            now(),
        )
        .unwrap_or_else(|e| {
            eprintln!("assign: {e:?}");
            std::process::exit(1);
        });
    println!(
        "playing {:?} under custody {}{}",
        track.title,
        handle.0,
        if cli.start_ms > 0 {
            format!(", from {} ms", cli.start_ms)
        } else {
            String::new()
        }
    );
    let _ = steward.request(
        &Request {
            cid: CorrelationId(10),
            shelf: shelf::delivery_output(),
            verb: delivery::VOLUME.into(),
            subject: None,
            args: to_value(&delivery::Volume { level: 40 }),
        },
        now(),
    );

    let mut seen = Seq(0);
    let mut last_line = String::new();
    let mut panel = panel::Panel::new(cli.rotation, cli.style, cli.theme);
    // The frame worth keeping is the last one drawn while playing.
    let mut was_playing = false;
    let mut saved = false;
    let save_panel = |panel: &panel::Panel| {
        if let Some(path) = &cli.panel {
            match panel.save(path) {
                Ok(()) => println!("panel written to {}", path.display()),
                Err(e) => {
                    eprintln!("panel: {e}");
                    std::process::exit(1);
                }
            }
        }
    };
    while start.elapsed() < Duration::from_secs(cli.seconds) {
        let t = now();
        steward.tick(t);
        for h in steward.happenings_since(seen).happenings {
            seen = h.seq;
            println!(
                "  #{:<4} +{:>6} ms  {}{}",
                h.seq.0,
                h.at.0,
                serde_json::to_string(&h.kind).unwrap_or_default(),
                h.subject
                    .as_ref()
                    .map(|s| format!("  {s}"))
                    .unwrap_or_default()
            );
        }
        let p = steward.projection(ProjectionKey::Rack {
            name: shelf::PLAYBACK.into(),
        });
        let output = steward
            .projection(ProjectionKey::Rack {
                name: shelf::DELIVERY.into(),
            })
            .subjects
            .first()
            .and_then(|s| from_value::<delivery::State>(&s.state).ok());
        if let Some(st) = p
            .subjects
            .first()
            .and_then(|s| from_value::<playback::State>(&s.state).ok())
        {
            let playing = st.status == playback::Status::Playing;
            if was_playing && !playing && !saved {
                save_panel(&panel);
                saved = true;
            }
            was_playing = playing;
            if cli.sheet && playing {
                panel.face_mut().show_sheet(true);
            }
            panel.draw(&panel::View::new(&st, output.as_ref(), "Folder"));
            let line = format!(
                "playback: {:?} {} ms{}",
                st.status,
                st.position_ms,
                st.track
                    .as_ref()
                    .map(|t| format!("  {}", t.title))
                    .unwrap_or_default()
            );
            if line != last_line {
                println!("{line}");
                last_line = line;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let out = steward.projection(ProjectionKey::Rack {
        name: shelf::DELIVERY.into(),
    });
    println!(
        "delivery/output as of #{}: {}",
        out.as_of.0,
        serde_json::to_string(&out.subjects[0].state).unwrap_or_default()
    );
    if !saved {
        save_panel(&panel);
    }
    steward.shutdown(now());
    println!("shut down after {} ms", now().0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalogue_is_stocked_by_what_we_admit() {
        let report = evo_esp32_catalogue::check(
            &catalogue(),
            &descriptions(std::path::Path::new(".")),
        );
        assert!(report.is_clean(), "{report}");
        assert_eq!(catalogue().racks.len(), 3);
    }

    #[test]
    fn a_track_plays_through_the_fabric_and_is_heard_by_the_sink() {
        let dir = std::env::temp_dir()
            .join(format!("evo-host-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // 300 ms at 8 kHz: short enough to play out within the test.
        sink::write_tone(&dir.join("song.wav"), 8000, 300).unwrap();
        let heard = dir.join("heard.wav");
        let mut s =
            bring_up(&dir, None, Vec::new(), Some(heard.clone()), Ticks(0))
                .unwrap();
        let track =
            first_track(&mut s, shelf::source_files(), String::new(), Ticks(0))
                .unwrap()
                .unwrap();
        assert_eq!(track.title, "song");
        s.assign(
            &shelf::playback_decode(),
            Assignment {
                subject: SubjectId::new("playback/now"),
                instruction: to_value(&Play { track, start_ms: 0 }),
            },
            Ticks(0),
        )
        .unwrap();
        let p = s.projection(ProjectionKey::Rack {
            name: shelf::PLAYBACK.into(),
        });
        let st: playback::State = from_value(&p.subjects[0].state).unwrap();
        assert_eq!(st.status, playback::Status::Playing);
        assert_eq!(st.track.unwrap().duration_ms, Some(300));
        assert_eq!(st.format.unwrap().sample_rate, 8000);

        // The sink paces by the wall clock; tick until released.
        let start = Instant::now();
        let mut released = false;
        while start.elapsed() < Duration::from_secs(5) {
            let t = Ticks(start.elapsed().as_millis() as u64);
            if s.tick(t).is_none() {
                released = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(released, "custody still open after 5 s");
        assert!(start.elapsed() >= Duration::from_millis(100), "paced");
        let replay = s.happenings_since(Seq(0));
        assert!(replay.happenings.iter().any(|h| matches!(
            h.kind,
            evo_esp32_sdk::happening::HappeningKind::CustodyReleased { .. }
        )));
        let bytes = std::fs::read(&heard).unwrap();
        assert_eq!(bytes.len(), 44 + 2400 * 4, "every frame was heard");
        s.shutdown(Ticks(500));
        std::fs::remove_dir_all(dir).unwrap();
    }
}

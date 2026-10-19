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

mod panel;
mod sink;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use evo_esp32_face::{theme, Style, Theme};
use evo_esp32_sdk::catalogue::Catalogue;
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
use org_evoframework_source_files::SourceFiles;
use sink::PacedWav;

mod generated {
    //! `catalogue()`, from `catalogue.toml` at build time.
    extern crate alloc;
    include!(concat!(env!("OUT_DIR"), "/catalogue.rs"));
}

const USAGE: &str = "\
evo-esp32-host: the evo-esp32 fabric playing a directory on a desktop

  --dir PATH        the music directory (default .)
  --seconds N       how long to run (default 5)
  --panel PATH      write the panel's last frame as a PNG
  --rotation DEG    the panel's rotation, 0, 90, 180 or 270 (default 0)
  --style NAME      the face's style, disc or card (default disc)
  --theme NAME      the face's theme: evo-default, night-sky, sunrise, air, liquid
  --gallery DIR     write a frame for every rotation, style and theme there and exit
  --wav PATH        record what the sink let out as a WAV
  --tone PATH       write a 440 Hz test tone (2 s, 44.1 kHz) there and exit
  --help";

struct Cli {
    dir: PathBuf,
    seconds: u64,
    panel: Option<PathBuf>,
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
        seconds: 5,
        panel: None,
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
        let value = args
            .next()
            .ok_or_else(|| format!("{flag} needs a value\n{USAGE}"))?;
        match flag.as_str() {
            "--dir" => cli.dir = PathBuf::from(value),
            "--seconds" => {
                cli.seconds = value.parse().map_err(|_| {
                    format!("--seconds must be a number, got {value:?}")
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
        PlaybackSymphonia::new(Box::new(PacedWav::new(None))).describe(),
        DeliveryNull::new().describe(),
    ]
}

/// Build the fabric: steward, plugins, checked against the catalogue.
/// The decoder writes to a paced sink that records to `wav` if given.
fn bring_up(
    dir: &std::path::Path,
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
        .admit_warden(
            Box::new(PlaybackSymphonia::new(Box::new(PacedWav::new(wav)))),
            now,
        )
        .map_err(|e| e.to_string())?;
    steward
        .admit_respondent(Box::new(DeliveryNull::new()), now)
        .map_err(|e| e.to_string())?;
    Ok(steward)
}

/// Ask the source for the root and pick the first track it lists.
fn first_track(
    steward: &mut Steward,
    now: Ticks,
) -> Result<Option<Track>, String> {
    let response = steward
        .request(
            &Request {
                cid: CorrelationId(1),
                shelf: shelf::source_files(),
                verb: LIST.into(),
                subject: None,
                args: to_value(&ListArgs::default()),
            },
            now,
        )
        .map_err(|e| format!("{e:?}"))?;
    let listing: Listing = match response.outcome {
        Ok(v) => from_value(&v)?,
        Err(f) => return Err(format!("list: {f:?}")),
    };
    println!("source/files lists {} entries:", listing.entries.len());
    for e in &listing.entries {
        match e {
            Entry::Folder { name, .. } => println!("  {name}/"),
            Entry::Track(t) => println!("  {}", t.title),
        }
    }
    Ok(listing.entries.into_iter().find_map(|e| match e {
        Entry::Track(t) => Some(t),
        Entry::Folder { .. } => None,
    }))
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
    }
    Ok(())
}

fn main() {
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

    let mut steward = match bring_up(&cli.dir, cli.wav.clone(), now()) {
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

    let track = match first_track(&mut steward, now()) {
        Ok(Some(t)) => t,
        Ok(None) => {
            eprintln!("no audio file in {}", cli.dir.display());
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };

    let handle = steward
        .assign(
            &shelf::playback_decode(),
            Assignment {
                subject: SubjectId::new("playback/now"),
                instruction: to_value(&Play {
                    track: track.clone(),
                    start_ms: 0,
                }),
            },
            now(),
        )
        .unwrap_or_else(|e| {
            eprintln!("assign: {e:?}");
            std::process::exit(1);
        });
    println!("playing {:?} under custody {}", track.title, handle.0);
    let _ = steward.request(
        &Request {
            cid: CorrelationId(2),
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
        let mut s = bring_up(&dir, Some(heard.clone()), Ticks(0)).unwrap();
        let track = first_track(&mut s, Ticks(0)).unwrap().unwrap();
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

// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The host distribution: the fabric playing a directory of music on a
//! desktop, with nothing decoded and nothing heard.
//!
//! What a firmware distribution does, done here without a board: the
//! catalogue compiled in, the plugins admitted, a consumer loop that
//! reads projections and the happenings stream. The consumer is where the
//! panel's pages will draw from; here it prints.
//!
//! ```text
//! evo-esp32-host --dir ~/Music --seconds 5
//! ```

use std::path::PathBuf;
use std::time::{Duration, Instant};

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
use org_evoframework_playback_sim::PlaybackSim;
use org_evoframework_source_files::SourceFiles;

mod generated {
    //! `catalogue()`, from `catalogue.toml` at build time.
    extern crate alloc;
    include!(concat!(env!("OUT_DIR"), "/catalogue.rs"));
}

const USAGE: &str = "\
evo-esp32-host: the evo-esp32 fabric playing a directory on a desktop

  --dir PATH        the music directory (default .)
  --seconds N       how long to run (default 5)
  --help";

struct Cli {
    dir: PathBuf,
    seconds: u64,
}

fn parse_args() -> Result<Cli, String> {
    let mut cli = Cli {
        dir: PathBuf::from("."),
        seconds: 5,
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
        PlaybackSim::new().describe(),
        DeliveryNull::new().describe(),
    ]
}

/// Build the fabric: steward, plugins, checked against the catalogue.
fn bring_up(dir: &std::path::Path, now: Ticks) -> Result<Steward, String> {
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
        .admit_warden(Box::new(PlaybackSim::new()), now)
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

    let mut steward = match bring_up(&cli.dir, now()) {
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

    // A simulated track has no decoder to tell its length; give it one so
    // the run shows a start and an end.
    let track = Track {
        duration_ms: Some(cli.seconds.saturating_mul(1000) / 2),
        ..track
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
        if let Some(st) = p
            .subjects
            .first()
            .and_then(|s| from_value::<playback::State>(&s.state).ok())
        {
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
    fn a_track_plays_through_the_fabric() {
        let dir = std::env::temp_dir()
            .join(format!("evo-host-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("song.flac"), b"").unwrap();
        let mut s = bring_up(&dir, Ticks(0)).unwrap();
        let track = first_track(&mut s, Ticks(0)).unwrap().unwrap();
        assert_eq!(track.title, "song");
        let track = Track {
            duration_ms: Some(300),
            ..track
        };
        s.assign(
            &shelf::playback_decode(),
            Assignment {
                subject: SubjectId::new("playback/now"),
                instruction: to_value(&Play { track, start_ms: 0 }),
            },
            Ticks(0),
        )
        .unwrap();
        s.tick(Ticks(200));
        let p = s.projection(ProjectionKey::Rack {
            name: shelf::PLAYBACK.into(),
        });
        let st: playback::State = from_value(&p.subjects[0].state).unwrap();
        assert_eq!(st.status, playback::Status::Playing);
        assert_eq!(st.position_ms, 200);
        s.tick(Ticks(400));
        let p = s.projection(ProjectionKey::Rack {
            name: shelf::PLAYBACK.into(),
        });
        let st: playback::State = from_value(&p.subjects[0].state).unwrap();
        assert_eq!(st.status, playback::Status::Stopped);
        let replay = s.happenings_since(Seq(0));
        assert!(replay.happenings.iter().any(|h| matches!(
            h.kind,
            evo_esp32_sdk::happening::HappeningKind::CustodyReleased { .. }
        )));
        s.shutdown(Ticks(500));
        std::fs::remove_dir_all(dir).unwrap();
    }
}

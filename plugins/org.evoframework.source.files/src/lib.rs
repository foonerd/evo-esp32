// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! A source over a directory tree on a host.
//!
//! `org.evoframework.source.files` stocks `source/files`. On `list` it
//! reads one directory under its root and answers with folders first,
//! then the files whose extension names an audio format, each as a
//! [`Track`] titled by its file name; every track listed is announced
//! as a `track` subject so a consumer can find it again by URI. Nothing
//! is read inside a file: tags are `metadata/tags`' concern, length is
//! the decoder's.
//!
//! A host plugin: it uses `std::fs`. On a device the same shelf is
//! stocked by a source over the card's filesystem with this plugin's
//! shape and none of its code.
#![forbid(unsafe_code)]

use std::path::{Component, Path, PathBuf};

use evo_esp32_sdk::context::Context;
use evo_esp32_sdk::contract::audio::source::{Entry, ListArgs, Listing, LIST};
use evo_esp32_sdk::contract::audio::{
    from_value, shelf, to_value, Track, TRACK_KIND,
};
use evo_esp32_sdk::plugin::{
    InstanceShape, InteractionShape, LoadError, Plugin, PluginDescription,
    Stocking,
};
use evo_esp32_sdk::respondent::{Fault, Request, Respondent, Response};
use evo_esp32_sdk::subject::{
    ExternalAddressing, SubjectAnnouncement, SubjectId,
};

/// The plugin's name, as the catalogue and the steward know it.
pub const NAME: &str = "org.evoframework.source.files";

/// Extensions taken to be audio, lower case.
pub const AUDIO_EXTENSIONS: &[&str] = &[
    "flac", "mp3", "wav", "ogg", "opus", "aac", "m4a", "aiff", "aif",
];

static STOCKS: [Stocking; 1] = [Stocking {
    rack: shelf::SOURCE,
    shelf: shelf::FILES,
    shape: shelf::SHAPE,
}];

/// The files source.
#[derive(Debug)]
pub struct SourceFiles {
    root: PathBuf,
    announced: Vec<SubjectId>,
}

impl SourceFiles {
    /// A source rooted at `root`. The root is not read until `list`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            announced: Vec::new(),
        }
    }

    /// The directory a listing path names, refusing to leave the root.
    fn resolve(&self, path: &str) -> Result<PathBuf, Fault> {
        let rel = Path::new(path.trim_start_matches('/'));
        if rel
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
        {
            return Err(Fault::BadArgs {
                detail: format!("path {path:?} leaves the source"),
            });
        }
        Ok(self.root.join(rel))
    }

    fn list(
        &mut self,
        args: &ListArgs,
        ctx: &mut dyn Context,
    ) -> Result<Listing, Fault> {
        let dir = self.resolve(&args.path)?;
        let read = std::fs::read_dir(&dir).map_err(|e| Fault::Failed {
            detail: format!("{}: {e}", dir.display()),
        })?;
        let mut folders = Vec::new();
        let mut tracks = Vec::new();
        for entry in read.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let rel = if args.path.trim_matches('/').is_empty() {
                name.clone()
            } else {
                format!("{}/{name}", args.path.trim_matches('/'))
            };
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                folders.push((name.clone(), rel));
            } else if is_audio(&name) {
                let (sample_rate, duration_ms) =
                    header_facts(&entry.path()).unwrap_or_default();
                tracks.push(Track {
                    uri: format!("file://{}", entry.path().display()),
                    title: title_of(&name),
                    artist: None,
                    album: None,
                    duration_ms,
                    sample_rate,
                });
            }
        }
        folders.sort();
        tracks.sort_by(|a, b| a.title.cmp(&b.title));
        for t in &tracks {
            if let Ok(id) = ctx.announce(SubjectAnnouncement {
                addressing: ExternalAddressing::new("file", t.uri.clone()),
                kind: TRACK_KIND.into(),
                state: to_value(t),
            }) {
                if !self.announced.contains(&id) {
                    self.announced.push(id);
                }
            }
        }
        let mut entries: Vec<Entry> = folders
            .into_iter()
            .map(|(name, path)| Entry::Folder { path, name })
            .collect();
        entries.extend(tracks.into_iter().map(Entry::Track));
        Ok(Listing {
            path: args.path.trim_matches('/').to_string(),
            entries,
        })
    }
}

/// The sample rate and the length a file's header states, read from its
/// first bytes: a FLAC's STREAMINFO (the first metadata block, 34 bytes
/// after the marker), a WAV's `fmt ` chunk. Other formats, or a header
/// that is not where it should be, give nothing; the decoder finds out
/// later. One small read a file, so a listing can say what a player
/// with a rate ceiling cannot play without opening anything.
pub fn header_facts(path: &Path) -> Option<(Option<u32>, Option<u64>)> {
    use std::io::Read;
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let mut head = [0u8; 64];
    let n = std::fs::File::open(path).ok()?.read(&mut head).ok()?;
    let head = &head[..n];
    match ext.as_str() {
        "flac" => flac_streaminfo(head),
        "wav" => wav_fmt(head),
        _ => None,
    }
}

/// `(sample rate, duration)` from a FLAC file's first 42 bytes.
fn flac_streaminfo(head: &[u8]) -> Option<(Option<u32>, Option<u64>)> {
    if head.len() < 42 || &head[..4] != b"fLaC" || head[4] & 0x7f != 0 {
        return None;
    }
    let info = &head[8..42];
    // 20 bits of rate, 3 of channels - 1, 5 of bits - 1, 36 of samples.
    let rate = (u32::from(info[10]) << 12)
        | (u32::from(info[11]) << 4)
        | (u32::from(info[12]) >> 4);
    let samples = (u64::from(info[13] & 0x0f) << 32)
        | (u64::from(info[14]) << 24)
        | (u64::from(info[15]) << 16)
        | (u64::from(info[16]) << 8)
        | u64::from(info[17]);
    if rate == 0 {
        return None;
    }
    let duration = (samples > 0).then(|| samples * 1000 / u64::from(rate));
    Some((Some(rate), duration))
}

/// `(sample rate, None)` from a WAV whose `fmt ` chunk is the first.
fn wav_fmt(head: &[u8]) -> Option<(Option<u32>, Option<u64>)> {
    if head.len() < 28 || &head[..4] != b"RIFF" || &head[8..16] != b"WAVEfmt " {
        return None;
    }
    let rate = u32::from_le_bytes([head[24], head[25], head[26], head[27]]);
    (rate > 0).then_some((Some(rate), None))
}

/// True for a file name whose extension is in [`AUDIO_EXTENSIONS`].
pub fn is_audio(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| {
            AUDIO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str())
        })
}

/// The file name without its extension.
pub fn title_of(name: &str) -> String {
    Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name)
        .to_string()
}

impl Plugin for SourceFiles {
    fn describe(&self) -> PluginDescription {
        PluginDescription {
            name: NAME,
            version: env!("CARGO_PKG_VERSION"),
            instance: InstanceShape::Factory,
            interaction: InteractionShape::Respondent,
            stocks: &STOCKS,
        }
    }

    fn load(&mut self, _ctx: &mut dyn Context) -> Result<(), LoadError> {
        if !self.root.is_dir() {
            return Err(LoadError(format!(
                "{} is not a directory",
                self.root.display()
            )));
        }
        Ok(())
    }

    fn unload(&mut self, ctx: &mut dyn Context) {
        for id in self.announced.drain(..) {
            let _ = ctx.retract(&id);
        }
    }
}

impl Respondent for SourceFiles {
    fn handle(&mut self, req: &Request, ctx: &mut dyn Context) -> Response {
        if req.verb != LIST {
            return Response::fault(req, Fault::UnknownVerb);
        }
        let args: ListArgs = match from_value(&req.args) {
            Ok(a) => a,
            Err(detail) => {
                return Response::fault(req, Fault::BadArgs { detail })
            }
        };
        match self.list(&args, ctx) {
            Ok(listing) => Response::ok(req, to_value(&listing)),
            Err(f) => Response::fault(req, f),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_esp32_sdk::catalogue::{Catalogue, Family, Kind, Rack, Shelf};
    use evo_esp32_sdk::projection::ProjectionKey;
    use evo_esp32_sdk::respondent::CorrelationId;
    use evo_esp32_sdk::Ticks;
    use evo_esp32_steward::{Config, Steward};

    fn catalogue() -> Catalogue {
        Catalogue {
            racks: vec![Rack {
                name: shelf::SOURCE.into(),
                family: Family::Domain,
                kinds: vec![Kind::Producer],
                charter: String::new(),
                shelves: vec![Shelf {
                    name: shelf::FILES.into(),
                    shape: shelf::SHAPE,
                    description: String::new(),
                }],
            }],
        }
    }

    fn tree() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "evo-source-files-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("Album")).unwrap();
        for f in [
            "b.flac",
            "a.MP3",
            "notes.txt",
            ".hidden.flac",
            "Album/1.wav",
        ] {
            std::fs::write(dir.join(f), b"").unwrap();
        }
        dir
    }

    fn list(s: &mut Steward, path: &str) -> Response {
        s.request(
            &Request {
                cid: CorrelationId(1),
                shelf: shelf::source_files(),
                verb: LIST.into(),
                subject: None,
                args: to_value(&ListArgs { path: path.into() }),
            },
            Ticks(0),
        )
        .unwrap()
    }

    #[test]
    fn the_header_peek_reads_flac_and_wav_rates() {
        // A FLAC marker, a STREAMINFO block header, and the 34 bytes:
        // min/max block 4096, min/max frame 0, then rate 192000 (20 bits),
        // 2 channels, 24 bits, 19 200 000 samples (100 s).
        let mut flac = b"fLaC\x00\x00\x00\x22".to_vec();
        flac.extend_from_slice(&[0x10, 0x00, 0x10, 0x00, 0, 0, 0, 0, 0, 0]);
        let rate: u32 = 192_000;
        let samples: u64 = 19_200_000;
        flac.push((rate >> 12) as u8);
        flac.push((rate >> 4) as u8);
        flac.push((((rate & 0xf) as u8) << 4) | (1 << 1) | ((24 - 1) >> 4));
        flac.push(
            ((((24 - 1) & 0xf) as u8) << 4) | ((samples >> 32) as u8 & 0xf),
        );
        flac.extend_from_slice(&(samples as u32).to_be_bytes());
        flac.extend_from_slice(&[0u8; 16]); // MD5
        assert_eq!(
            flac_streaminfo(&flac),
            Some((Some(192_000), Some(100_000)))
        );
        let mut wav =
            b"RIFF\x00\x00\x00\x00WAVEfmt \x10\x00\x00\x00\x01\x00\x02\x00"
                .to_vec();
        wav.extend_from_slice(&44_100u32.to_le_bytes());
        wav.extend_from_slice(&[0u8; 8]);
        assert_eq!(wav_fmt(&wav), Some((Some(44_100), None)));
        assert_eq!(flac_streaminfo(b"fLaC"), None, "too short");
        assert_eq!(wav_fmt(b"RIFF....WAVELIST"), None, "fmt not first");
    }

    #[test]
    fn lists_folders_then_audio_and_announces_tracks() {
        let root = tree();
        let mut s = Steward::new(catalogue(), Config::default()).unwrap();
        s.admit_respondent(Box::new(SourceFiles::new(&root)), Ticks(0))
            .unwrap();
        let r = list(&mut s, "");
        let listing: Listing = from_value(&r.outcome.unwrap()).unwrap();
        let names: Vec<String> = listing
            .entries
            .iter()
            .map(|e| match e {
                Entry::Folder { name, .. } => format!("{name}/"),
                Entry::Track(t) => t.title.clone(),
            })
            .collect();
        assert_eq!(names, ["Album/", "a", "b"]);
        let p = s.projection(ProjectionKey::Rack {
            name: shelf::SOURCE.into(),
        });
        assert_eq!(p.subjects.len(), 2);
        assert!(p.subjects.iter().all(|r| r.kind == TRACK_KIND));

        let r = list(&mut s, "Album");
        let listing: Listing = from_value(&r.outcome.unwrap()).unwrap();
        assert_eq!(listing.path, "Album");
        assert!(
            matches!(&listing.entries[0], Entry::Track(t) if t.title == "1")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn refuses_to_leave_the_root() {
        let root = tree();
        let mut s = Steward::new(catalogue(), Config::default()).unwrap();
        s.admit_respondent(Box::new(SourceFiles::new(&root)), Ticks(0))
            .unwrap();
        assert!(matches!(
            list(&mut s, "../").outcome,
            Err(Fault::BadArgs { .. })
        ));
        assert!(matches!(
            list(&mut s, "missing").outcome,
            Err(Fault::Failed { .. })
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_missing_root_fails_to_load() {
        let mut s = Steward::new(catalogue(), Config::default()).unwrap();
        assert!(s
            .admit_respondent(
                Box::new(SourceFiles::new("/nonexistent/evo")),
                Ticks(0)
            )
            .is_err());
    }

    #[test]
    fn names_and_extensions() {
        assert!(is_audio("x.FLAC"));
        assert!(!is_audio("x.txt"));
        assert_eq!(title_of("01 - Song.flac"), "01 - Song");
    }
}

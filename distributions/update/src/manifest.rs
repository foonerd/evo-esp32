// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The release manifest: what a player reads to learn whether a newer
//! firmware exists, and where. One small JSON file per board at a fixed
//! address (the project's latest release, or a server of the owner's
//! own), written by the release workflow beside the images:
//!
//! ```json
//! {"board":"waveshare-esp32-c5-touch-lcd-28","version":"0.2.0",
//!  "file":"evo-esp32-waveshare-esp32-c5-touch-lcd-28-0.2.0.ota.bin",
//!  "size":4423680,"sha256":"…","url":"https://…/…-0.2.0.ota.bin"}
//! ```
//!
//! The manifest is advice, not trust: the image it points at is signed,
//! and the running firmware verifies the signature before it accepts a
//! byte of it as firmware. A manifest that lies costs a download.

use std::cmp::Ordering;

/// The address the player reads when the card names none: the latest
/// release's manifest for the board.
pub const DEFAULT_URL: &str = "https://github.com/foonerd/evo-esp32/releases/latest/download/evo-esp32-waveshare-esp32-c5-touch-lcd-28.manifest.json";

/// The most a manifest may be; anything longer is not one.
pub const MAX_LEN: usize = 4096;

/// What a manifest says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// The board the image is for.
    pub board: String,
    /// The release's version, `MAJOR.MINOR.PATCH`.
    pub version: String,
    /// The image's address.
    pub url: String,
    /// The image's length in bytes, with its signature block.
    pub size: u64,
}

impl Manifest {
    /// Read a manifest; refused when a field is missing, not of its
    /// kind, or the version is not three numbers.
    pub fn parse(text: &str) -> Result<Self, String> {
        let v: serde_json::Value =
            serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
        let field = |name: &str| -> Result<String, String> {
            v.get(name)
                .and_then(|x| x.as_str())
                .map(str::to_string)
                .ok_or_else(|| format!("no \"{name}\""))
        };
        let board = field("board")?;
        let version = field("version")?;
        let url = field("url")?;
        let size = v
            .get("size")
            .and_then(|x| x.as_u64())
            .ok_or_else(|| "no \"size\"".to_string())?;
        parts(&version).ok_or_else(|| format!("version {version:?}"))?;
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return Err(format!("url {url:?}"));
        }
        Ok(Self {
            board,
            version,
            url,
            size,
        })
    }

    /// Whether this is a newer release than `running` for `board`.
    pub fn is_newer_for(&self, board: &str, running: &str) -> bool {
        self.board == board
            && compare(&self.version, running) == Ordering::Greater
    }
}

/// `MAJOR.MINOR.PATCH` as numbers; a `v` in front and anything after a
/// `-` or `+` are allowed and ignored.
fn parts(version: &str) -> Option<[u64; 3]> {
    let core = version
        .trim_start_matches('v')
        .split(['-', '+'])
        .next()
        .unwrap_or("");
    let mut out = [0u64; 3];
    let mut n = 0;
    for p in core.split('.') {
        if n == 3 {
            return None;
        }
        out[n] = p.parse().ok()?;
        n += 1;
    }
    (n == 3).then_some(out)
}

/// Order two versions; one that does not parse is older than one that
/// does, and equal to another that does not.
pub fn compare(a: &str, b: &str) -> Ordering {
    match (parts(a), parts(b)) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => Ordering::Greater,
        (None, Some(_)) => Ordering::Less,
        (None, None) => Ordering::Equal,
    }
}

/// Where the player stands with releases, for the Settings row and the
/// page.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Standing {
    /// The card says `update = off`.
    Off,
    /// Not looked yet: no network, or not time.
    #[default]
    NotYet,
    /// Reading the manifest now.
    Checking,
    /// The running firmware is the latest the manifest knows.
    Current,
    /// A newer release exists.
    Available(Manifest),
    /// The last look failed, and why.
    Failed(String),
    /// The release is being written now.
    Applying(String),
}

impl Standing {
    /// One line for the Settings row's value, after the running version.
    pub fn line(&self) -> String {
        match self {
            Standing::Off => "updates off".into(),
            Standing::NotYet => "not checked yet".into(),
            Standing::Checking => "checking\u{2026}".into(),
            Standing::Current => "up to date".into(),
            Standing::Available(m) => format!("{} available", m.version),
            Standing::Failed(why) => format!("check failed: {why}"),
            Standing::Applying(v) => format!("installing {v}\u{2026}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"{"board":"waveshare-esp32-c5-touch-lcd-28","version":"0.2.0","file":"x.ota.bin","size":4423680,"sha256":"ab","url":"https://example.net/x.ota.bin"}"#;

    #[test]
    fn a_manifest_is_read_and_judged_against_the_running_version() {
        let m = Manifest::parse(GOOD).unwrap();
        assert_eq!(m.version, "0.2.0");
        assert_eq!(m.size, 4423680);
        assert!(m.is_newer_for("waveshare-esp32-c5-touch-lcd-28", "0.1.6"));
        assert!(m.is_newer_for("waveshare-esp32-c5-touch-lcd-28", "0.1.16"));
        assert!(!m.is_newer_for("waveshare-esp32-c5-touch-lcd-28", "0.2.0"));
        assert!(!m.is_newer_for("waveshare-esp32-c5-touch-lcd-28", "0.10.0"));
        assert!(!m.is_newer_for("another-board", "0.1.6"));
        assert_eq!(compare("v0.2.0-3-gabc", "0.2.0"), Ordering::Equal);
        assert_eq!(compare("1.0.0", "0.99.99"), Ordering::Greater);
        assert_eq!(compare("junk", "0.1.0"), Ordering::Less);
    }

    #[test]
    fn a_manifest_that_is_not_one_is_refused() {
        assert!(Manifest::parse("not json").is_err());
        assert!(Manifest::parse(
            r#"{"board":"b","version":"0.2","url":"https://x","size":1}"#
        )
        .is_err());
        assert!(Manifest::parse(
            r#"{"board":"b","version":"0.2.0","url":"ftp://x","size":1}"#
        )
        .is_err());
        assert!(Manifest::parse(
            r#"{"board":"b","version":"0.2.0","url":"https://x"}"#
        )
        .is_err());
        assert!(Manifest::parse(
            r#"{"board":"b","version":"0.2.0","url":"https://x","size":"big"}"#
        )
        .is_err());
    }

    #[test]
    fn the_standing_has_a_line() {
        let m = Manifest::parse(GOOD).unwrap();
        assert_eq!(Standing::Available(m).line(), "0.2.0 available");
        assert_eq!(Standing::Current.line(), "up to date");
        assert_eq!(
            Standing::Failed("HTTP 404".into()).line(),
            "check failed: HTTP 404"
        );
        assert_eq!(
            Standing::Applying("0.2.0".into()).line(),
            "installing 0.2.0\u{2026}"
        );
    }
}

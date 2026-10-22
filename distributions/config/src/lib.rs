// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The player's configuration on the card.
//!
//! A player with a card slot and no keyboard is configured by a text file:
//! `evoconfig.txt` at the card's root, written on any computer, read on
//! every boot. The card stays in (it holds the music), so the file is the
//! truth and the player writes back to it when a setting changes on the
//! panel, keeping every other line as the user wrote it. After a boot the
//! player writes `evostatus.txt` next to it: what it is, what it applied,
//! what it did not understand.
//!
//! The grammar is the smallest a text editor on any OS produces: UTF-8, a
//! leading byte-order mark tolerated, CRLF or LF, one `key = value` per
//! line, blank lines and lines starting with `#` or `;` ignored. Keys are
//! case-insensitive. The value is everything after the first `=`, trimmed;
//! a value wrapped in matching double quotes keeps its inner spaces. There
//! are no comments after a value, since `#` is a legal character in a
//! password. An empty value puts the setting back to its default.
//!
//! The keys are [`KEYS`]; [`parse`] gives [`Parsed`] with the typed
//! [`Settings`] and every line that needs the user's attention; [`set`]
//! edits one key in the file's text in place.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// The file's name at the card's root.
pub const FILE_NAME: &str = "evoconfig.txt";
/// The report the player writes next to it after a boot.
pub const STATUS_NAME: &str = "evostatus.txt";

/// The keys the file may carry, lower case.
pub const KEYS: &[&str] = &[
    "theme",
    "style",
    "rotation",
    "volume",
    "name",
    "wifi_ssid",
    "wifi_password",
    "wifi_band",
    "dac",
    "output",
    "shuffle",
    "repeat",
    "start",
    "snapserver",
];

/// The schemes a `share` may name.
pub const SHARE_SCHEMES: &[&str] = &["smb://", "smb1://", "nfs://"];
/// The most shares the file may name: `share` and `share2` to `share9`.
pub const SHARES: usize = 9;

/// Values from the file, each `None` where the file did not say or said
/// something the key could not take (reported in [`Parsed::invalid`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Settings {
    /// The face's theme, by name; the face knows which names exist.
    pub theme: Option<String>,
    /// The face's style, by name: `disc` or `card`.
    pub style: Option<String>,
    /// The panel's rotation in degrees clockwise: 0, 90, 180 or 270.
    pub rotation: Option<u16>,
    /// The level at power-on, percent, 0 to 100.
    pub volume: Option<u8>,
    /// The player's name on the network: a DNS label, up to 30 of
    /// letters, digits and hyphens.
    pub name: Option<String>,
    /// The Wi-Fi network to join.
    pub wifi_ssid: Option<String>,
    /// Its password; empty for an open network.
    pub wifi_password: Option<String>,
    /// The band to join it on, when the network is on both and the
    /// player is to take one of them; not set, either.
    pub wifi_band: Option<WifiBand>,
    /// The DAC board on the header, by the register's name, for a board
    /// the player cannot recognise by itself (an I2S-only one) or to
    /// override what it found. The distribution knows the names.
    pub dac: Option<String>,
    /// Where the frames go when the board has a speaker and a DAC both.
    pub output: Option<Output>,
    /// Whether the queue plays in a shuffled order.
    pub shuffle: Option<bool>,
    /// What happens at the queue's end.
    pub repeat: Option<Repeat>,
    /// What plays at power-on: a path on the card (a folder, a file, a
    /// playlist), with `#Name` after a playlist to pick a station. Not
    /// set, the whole card.
    pub start: Option<String>,
    /// The Snapcast server to play from, `host` or `host:port`, the
    /// port 1704 when not given. Not set, the player looks for one on
    /// the network.
    pub snapserver: Option<String>,
    /// The network shares the library lists, in the file's numbering:
    /// `share` (1) and `share2` to `share9`, each with its credentials
    /// from `share_user`, `share_password`, `share_domain` (`share2_user`
    /// and so on). A numbered credential with no share of that number
    /// is dropped.
    pub shares: Vec<ShareSetting>,
}

/// One network share: where it is and as whom.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ShareSetting {
    /// Its number in the file: 1 for `share`, 2 for `share2`.
    pub index: u8,
    /// `smb://host/share/folder`, `smb1://…` or `nfs://host/export/…`.
    pub url: Option<String>,
    /// The account; none is a guest.
    pub user: Option<String>,
    /// Its password.
    pub password: Option<String>,
    /// The account's domain, for SMB.
    pub domain: Option<String>,
}

/// What happens at the end of the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Repeat {
    /// It stops.
    #[default]
    Off,
    /// It starts again.
    All,
    /// The one track plays again.
    One,
}

impl Repeat {
    /// The name [`parse`] accepts.
    pub fn name(self) -> &'static str {
        match self {
            Repeat::Off => "off",
            Repeat::All => "all",
            Repeat::One => "one",
        }
    }

    fn named(name: &str) -> Option<Self> {
        match name {
            "off" | "no" | "none" => Some(Repeat::Off),
            "all" | "on" | "yes" => Some(Repeat::All),
            "one" | "track" => Some(Repeat::One),
            _ => None,
        }
    }
}

/// Where the frames go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// The board's own speaker or codec.
    Speaker,
    /// The DAC on the header.
    Hat,
    /// Both, the same frames.
    Both,
}

impl Output {
    /// The name [`parse`] accepts.
    pub fn name(self) -> &'static str {
        match self {
            Output::Speaker => "speaker",
            Output::Hat => "hat",
            Output::Both => "both",
        }
    }

    fn named(name: &str) -> Option<Self> {
        match name {
            "speaker" => Some(Output::Speaker),
            "hat" => Some(Output::Hat),
            "both" => Some(Output::Both),
            _ => None,
        }
    }
}

/// The Wi-Fi band the player joins its network on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WifiBand {
    /// 5 GHz where the network is on it, either band where it is not;
    /// the default.
    Auto,
    /// The same as `Auto`, said explicitly.
    Five,
    /// 2.4 GHz only.
    TwoPointFour,
}

impl WifiBand {
    /// The name [`parse`] accepts.
    pub fn name(self) -> &'static str {
        match self {
            WifiBand::Auto => "auto",
            WifiBand::Five => "5",
            WifiBand::TwoPointFour => "2.4",
        }
    }

    fn named(name: &str) -> Option<Self> {
        match name {
            "auto" | "both" => Some(WifiBand::Auto),
            "5" | "5ghz" => Some(WifiBand::Five),
            "2.4" | "2.4ghz" => Some(WifiBand::TwoPointFour),
            _ => None,
        }
    }
}

/// What a file said.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Parsed {
    /// The settings, typed.
    pub settings: Settings,
    /// `(line, key)` for keys this player does not know.
    pub unknown: Vec<(usize, String)>,
    /// `(line, key, value)` for values a key could not take.
    pub invalid: Vec<(usize, String, String)>,
    /// Lines with no `=`.
    pub malformed: Vec<usize>,
}

impl Parsed {
    /// True when nothing in the file needs the user's attention.
    pub fn is_clean(&self) -> bool {
        self.unknown.is_empty()
            && self.invalid.is_empty()
            && self.malformed.is_empty()
    }
}

/// Parse the file's text. Never fails: a bad line is reported, the rest
/// is applied, since a player cannot refuse to boot over a typo. A key
/// repeated keeps its last line.
pub fn parse(text: &str) -> Parsed {
    let mut out = Parsed::default();
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            out.malformed.push(i + 1);
            continue;
        };
        let key = k.trim().to_ascii_lowercase();
        let value = unquote(v.trim());
        if let Some((index, field)) = share_key(&key) {
            if !take_share(&mut out.settings, index, field, value) {
                out.invalid.push((i + 1, key, value.to_string()));
            }
            continue;
        }
        if !KEYS.contains(&key.as_str()) {
            out.unknown.push((i + 1, key));
            continue;
        }
        if !take(&mut out.settings, &key, value) {
            out.invalid.push((i + 1, key, value.to_string()));
        }
    }
    // A credential whose share was never named has nothing to go with.
    out.settings.shares.retain(|s| s.url.is_some());
    out.settings.shares.sort_by_key(|s| s.index);
    out
}

/// `share`, `share_user`, `share7_password`: the share's number and
/// the field named.
fn share_key(key: &str) -> Option<(u8, &'static str)> {
    let rest = key.strip_prefix("share")?;
    let (digits, field) = match rest.split_once('_') {
        Some((d, f)) => (d, f),
        None => (rest, ""),
    };
    let index = match digits {
        "" => 1,
        d => d
            .parse::<u8>()
            .ok()
            .filter(|n| (2..=SHARES as u8).contains(n))?,
    };
    let field = match field {
        "" => "",
        "user" => "user",
        "password" => "password",
        "domain" => "domain",
        _ => return None,
    };
    Some((index, field))
}

fn take_share(
    settings: &mut Settings,
    index: u8,
    field: &str,
    value: &str,
) -> bool {
    let text = (!value.is_empty()).then(|| value.to_string());
    if field.is_empty()
        && !value.is_empty()
        && !SHARE_SCHEMES.iter().any(|s| {
            value.strip_prefix(s).is_some_and(|rest| {
                rest.split_once('/').is_some_and(|(h, p)| {
                    !h.is_empty() && !p.trim_matches('/').is_empty()
                })
            })
        })
    {
        return false;
    }
    let share = match settings.shares.iter_mut().find(|s| s.index == index) {
        Some(s) => s,
        None => {
            settings.shares.push(ShareSetting {
                index,
                ..ShareSetting::default()
            });
            settings.shares.last_mut().expect("just pushed")
        }
    };
    match field {
        "" => share.url = text,
        "user" => share.user = text,
        // An empty password is a password.
        "password" => share.password = Some(value.to_string()),
        _ => share.domain = text,
    }
    true
}

/// Put `value` for `key` into `settings`; false if the key cannot take it.
/// An empty value clears the key.
fn take(settings: &mut Settings, key: &str, value: &str) -> bool {
    let text = (!value.is_empty()).then(|| value.to_string());
    match key {
        "theme" => settings.theme = text,
        "style" => settings.style = text,
        "rotation" => {
            settings.rotation = match value {
                "" => None,
                "0" | "90" | "180" | "270" => value.parse().ok(),
                _ => return false,
            }
        }
        "volume" => {
            settings.volume = match value {
                "" => None,
                v => match v.parse::<u8>() {
                    Ok(n) if n <= 100 => Some(n),
                    _ => return false,
                },
            }
        }
        "name" => {
            if !value.is_empty() && !is_dns_label(value) {
                return false;
            }
            settings.name = text;
        }
        "wifi_ssid" => {
            if value.len() > 32 {
                return false;
            }
            settings.wifi_ssid = text;
        }
        "wifi_password" => {
            if value.len() > 63 {
                return false;
            }
            // An open network has a password line that is empty, which
            // is a value, not an absence: keep the empty string.
            settings.wifi_password = Some(value.to_string());
        }
        "wifi_band" => {
            settings.wifi_band = match value {
                "" => None,
                v => match WifiBand::named(&v.to_ascii_lowercase()) {
                    Some(b) => Some(b),
                    None => return false,
                },
            }
        }
        "dac" => settings.dac = text,
        "output" => {
            settings.output = match value {
                "" => None,
                v => match Output::named(&v.to_ascii_lowercase()) {
                    Some(o) => Some(o),
                    None => return false,
                },
            }
        }
        "shuffle" => {
            settings.shuffle = match value.to_ascii_lowercase().as_str() {
                "" => None,
                "on" | "yes" | "true" | "1" => Some(true),
                "off" | "no" | "false" | "0" => Some(false),
                _ => return false,
            }
        }
        "repeat" => {
            settings.repeat = match value {
                "" => None,
                v => match Repeat::named(&v.to_ascii_lowercase()) {
                    Some(r) => Some(r),
                    None => return false,
                },
            }
        }
        "start" => {
            // A path on the card, never one that leaves it.
            if value.contains("..") || value.contains('\\') {
                return false;
            }
            settings.start =
                text.map(|t| t.trim_start_matches('/').to_string());
        }
        "snapserver" => {
            if !value.is_empty() && !is_host_port(value) {
                return false;
            }
            settings.snapserver = text;
        }
        _ => return false,
    }
    true
}

/// Letters, digits and hyphens, not starting or ending with a hyphen,
/// 1 to 30 long: a DNS label a router's device list and mDNS accept.
fn is_dns_label(s: &str) -> bool {
    (1..=30).contains(&s.len())
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-')
}

/// A host name or address, with or without `:port`: the characters a
/// name or a dotted address uses, and a port that is a number.
fn is_host_port(s: &str) -> bool {
    let (host, port) = match s.rsplit_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (s, None),
    };
    let host_ok = !host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.');
    host_ok && port.is_none_or(|p| p.parse::<u16>().is_ok_and(|n| n > 0))
}

fn unquote(v: &str) -> &str {
    match v.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        Some(inner) if v.len() >= 2 => inner,
        _ => v,
    }
}

/// The file's text with `key` set to `value`: the key's line is rewritten
/// where it is (the first one, if repeated; later ones are dropped so the
/// last line no longer overrides), every other line stays as written, and
/// a key the file lacks is appended. A value with leading or trailing
/// spaces is quoted. Line endings follow the file's.
pub fn set(text: &str, key: &str, value: &str) -> String {
    let crlf = text.contains("\r\n");
    let eol = if crlf { "\r\n" } else { "\n" };
    let key = key.to_ascii_lowercase();
    let line = format!("{key} = {}", quote_if_needed(value));
    let mut out = String::with_capacity(text.len() + line.len() + 2);
    let mut written = false;
    let bom = text.starts_with('\u{feff}');
    if bom {
        out.push('\u{feff}');
    }
    let body = text.strip_prefix('\u{feff}').unwrap_or(text);
    for raw in body.split_inclusive('\n') {
        let content = raw.trim_end_matches(['\r', '\n']);
        let trimmed = content.trim();
        let this_key = if trimmed.is_empty()
            || trimmed.starts_with('#')
            || trimmed.starts_with(';')
        {
            None
        } else {
            trimmed
                .split_once('=')
                .map(|(k, _)| k.trim().to_ascii_lowercase())
        };
        if this_key.as_deref() == Some(key.as_str()) {
            if !written {
                out.push_str(&line);
                out.push_str(eol);
                written = true;
            }
            continue;
        }
        out.push_str(raw);
    }
    if !written {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push_str(eol);
        }
        out.push_str(&line);
        out.push_str(eol);
    }
    out
}

fn quote_if_needed(value: &str) -> String {
    if value != value.trim() {
        format!("\"{value}\"")
    } else {
        value.to_string()
    }
}

/// The lines of the status report for a file that was read: every key
/// applied with its value (the password shown as its length), then what
/// was not understood. `applied` is `(key, value shown)`.
pub fn status_lines(parsed: &Parsed) -> Vec<String> {
    let s = &parsed.settings;
    let mut lines = Vec::new();
    let mut applied = Vec::new();
    if let Some(v) = &s.theme {
        applied.push(("theme", v.clone()));
    }
    if let Some(v) = &s.style {
        applied.push(("style", v.clone()));
    }
    if let Some(v) = s.rotation {
        applied.push(("rotation", v.to_string()));
    }
    if let Some(v) = s.volume {
        applied.push(("volume", v.to_string()));
    }
    if let Some(v) = &s.name {
        applied.push(("name", v.clone()));
    }
    if let Some(v) = &s.wifi_ssid {
        applied.push(("wifi_ssid", v.clone()));
    }
    if let Some(v) = &s.wifi_password {
        let shown = if v.is_empty() {
            "(empty: an open network)".to_string()
        } else {
            format!("({} characters, not shown)", v.chars().count())
        };
        applied.push(("wifi_password", shown));
    }
    if let Some(v) = s.wifi_band {
        applied.push(("wifi_band", v.name().to_string()));
    }
    if let Some(v) = &s.dac {
        applied.push(("dac", v.clone()));
    }
    if let Some(v) = s.output {
        applied.push(("output", v.name().to_string()));
    }
    if let Some(v) = s.shuffle {
        applied.push(("shuffle", if v { "on" } else { "off" }.to_string()));
    }
    if let Some(v) = s.repeat {
        applied.push(("repeat", v.name().to_string()));
    }
    if let Some(v) = &s.start {
        applied.push(("start", v.clone()));
    }
    if let Some(v) = &s.snapserver {
        applied.push(("snapserver", v.clone()));
    }
    if applied.is_empty() && s.shares.is_empty() {
        lines.push("read: no settings in the file".to_string());
    } else {
        for (k, v) in applied {
            lines.push(format!("read: {k} = {v}"));
        }
    }
    for share in &s.shares {
        let suffix = if share.index == 1 {
            String::new()
        } else {
            share.index.to_string()
        };
        if let Some(v) = &share.url {
            lines.push(format!("read: share{suffix} = {v}"));
        }
        if let Some(v) = &share.user {
            lines.push(format!("read: share{suffix}_user = {v}"));
        }
        if let Some(v) = &share.password {
            lines.push(format!(
                "read: share{suffix}_password = ({} characters, not shown)",
                v.chars().count()
            ));
        }
        if let Some(v) = &share.domain {
            lines.push(format!("read: share{suffix}_domain = {v}"));
        }
    }
    for (n, k) in &parsed.unknown {
        lines.push(format!("line {n}: unknown key \"{k}\", ignored"));
    }
    for (n, k, v) in &parsed.invalid {
        lines.push(format!(
            "line {n}: \"{v}\" is not a value {k} can take, ignored"
        ));
    }
    for n in &parsed.malformed {
        lines.push(format!("line {n}: no \"=\", ignored"));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    const TEMPLATE: &str = include_str!("../../../sdcard/evoconfig.txt");

    #[test]
    fn the_template_parses_clean() {
        let p = parse(TEMPLATE);
        assert!(p.is_clean(), "{p:?}");
        assert_eq!(p.settings.theme.as_deref(), Some("evo-default"));
        assert_eq!(p.settings.style.as_deref(), Some("disc"));
        assert_eq!(p.settings.rotation, Some(0));
        assert_eq!(p.settings.volume, Some(40));
        assert_eq!(p.settings.wifi_ssid.as_deref(), Some("Your Network"));
    }

    #[test]
    fn grammar_bom_crlf_quotes_case_and_empty() {
        let p = parse(
            "\u{feff}THEME = Night-Sky\r\n; note\r\nwifi_ssid = \"  Home \"\r\nvolume = 7\r\nStyle=\r\n",
        );
        assert!(p.is_clean(), "{p:?}");
        assert_eq!(p.settings.theme.as_deref(), Some("Night-Sky"));
        assert_eq!(
            p.settings.wifi_ssid.as_deref(),
            Some("  Home "),
            "quotes keep spaces"
        );
        assert_eq!(p.settings.volume, Some(7));
        assert_eq!(p.settings.style, None, "empty clears");
    }

    #[test]
    fn shares_come_numbered_with_their_credentials() {
        let p = parse(
            "share = smb://nas.local/Music\nshare_user = alice\nshare_password = secret\nshare3 = nfs://192.0.2.5/export/music\nshare2 = smb1://oldbox/music/Albums\nshare2_user = guest\nshare2_password =\nshare9_user = nobody\n",
        );
        assert!(p.is_clean(), "{p:?}");
        let s = &p.settings.shares;
        assert_eq!(s.len(), 3);
        assert_eq!(
            (s[0].index, s[0].url.as_deref()),
            (1, Some("smb://nas.local/Music"))
        );
        assert_eq!(s[0].user.as_deref(), Some("alice"));
        assert_eq!(s[0].password.as_deref(), Some("secret"));
        assert_eq!(
            (s[1].index, s[1].url.as_deref()),
            (2, Some("smb1://oldbox/music/Albums"))
        );
        assert_eq!(s[1].password.as_deref(), Some(""));
        assert_eq!((s[2].index, s[2].user.as_deref()), (3, None));
        let report = status_lines(&p);
        assert!(report
            .iter()
            .any(|l| l == "read: share = smb://nas.local/Music"));
        assert!(report
            .iter()
            .any(|l| l == "read: share_password = (6 characters, not shown)"));
        assert!(report.iter().any(|l| l == "read: share2_user = guest"));
        assert!(!report.iter().any(|l| l.contains("secret")));
    }

    #[test]
    fn a_share_needs_a_scheme_a_host_and_a_path() {
        let p = parse("share = //nas/music\nshare2 = smb://nas\nshare3 = ftp://x/y\nshare4 = smb:///music\nshare5 = smb://nas/music\n");
        assert_eq!(p.invalid.len(), 4, "{p:?}");
        assert_eq!(p.settings.shares.len(), 1);
        assert_eq!(p.settings.shares[0].index, 5);
        // share10 and share_other are not keys.
        let p = parse("share10 = smb://a/b\nshare_other = x\n");
        assert_eq!(p.unknown.len(), 2);
    }

    #[test]
    fn a_snapserver_is_a_host_or_a_host_and_port() {
        let p = parse("snapserver = music.local\n");
        assert_eq!(p.settings.snapserver.as_deref(), Some("music.local"));
        let p = parse("snapserver = 192.0.2.5:1704\n");
        assert_eq!(p.settings.snapserver.as_deref(), Some("192.0.2.5:1704"));
        let p = parse("snapserver = music:room\nsnapserver = a b\n");
        assert_eq!(p.settings.snapserver, None);
        assert_eq!(p.invalid.len(), 2);
        let p = parse("snapserver =\n");
        assert!(p.is_clean());
        assert_eq!(p.settings.snapserver, None);
    }

    #[test]
    fn bad_lines_are_reported_not_fatal() {
        let p = parse("rotation = 45\nvolume = 101\nname = -x\ncolour = red\njust words\nwifi_password = pa#ss word\n");
        assert_eq!(p.settings.rotation, None);
        assert_eq!(p.settings.volume, None);
        assert_eq!(p.settings.name, None);
        assert_eq!(p.invalid.len(), 3);
        assert_eq!(p.unknown, vec![(4, "colour".to_string())]);
        assert_eq!(p.malformed, vec![5]);
        assert_eq!(
            p.settings.wifi_password.as_deref(),
            Some("pa#ss word"),
            "no inline comments"
        );
        let lines = status_lines(&p);
        assert!(lines.iter().any(|l| l.contains("10 characters, not shown")));
        assert!(!lines.iter().any(|l| l.contains("pa#ss")));
    }

    #[test]
    fn dac_and_output_are_read_and_output_is_one_of_three() {
        let p = parse("dac = pcm5102a\noutput = Both\n");
        assert!(p.is_clean(), "{p:?}");
        assert_eq!(p.settings.dac.as_deref(), Some("pcm5102a"));
        assert_eq!(p.settings.output, Some(Output::Both));
        let p = parse("output = headphones\n");
        assert_eq!(p.settings.output, None);
        assert_eq!(p.invalid.len(), 1);
    }

    #[test]
    fn shuffle_repeat_and_start_are_read() {
        let p = parse(
            "shuffle = Off\nrepeat = one\nstart = Radio/Evo Selection.m3u#Classic FM\n",
        );
        assert!(p.is_clean(), "{p:?}");
        assert_eq!(p.settings.shuffle, Some(false));
        assert_eq!(p.settings.repeat, Some(Repeat::One));
        assert_eq!(
            p.settings.start.as_deref(),
            Some("Radio/Evo Selection.m3u#Classic FM")
        );
        let p = parse(
            "start = /Music/Queen\nshuffle = sometimes\nrepeat = twice\n",
        );
        assert_eq!(p.settings.start.as_deref(), Some("Music/Queen"));
        assert_eq!(p.invalid.len(), 2);
        let p = parse("start = ../etc\n");
        assert_eq!(p.settings.start, None);
        assert_eq!(p.invalid.len(), 1, "a path that leaves the card");
        assert_eq!(
            status_lines(&parse("repeat = all\n")),
            ["read: repeat = all"]
        );
    }

    #[test]
    fn a_repeated_key_keeps_its_last_line() {
        let p = parse("volume = 10\nvolume = 20\n");
        assert_eq!(p.settings.volume, Some(20));
    }

    #[test]
    fn set_rewrites_in_place_keeps_the_rest_and_appends_the_new() {
        let text = "# head\r\ntheme = sunrise\r\nvolume = 10\r\n";
        let out = set(text, "Theme", "air");
        assert_eq!(out, "# head\r\ntheme = air\r\nvolume = 10\r\n");
        let out = set(&out, "style", "card");
        assert_eq!(
            out,
            "# head\r\ntheme = air\r\nvolume = 10\r\nstyle = card\r\n"
        );
        // A repeated key collapses to the first line, so the new value wins.
        let out = set("volume = 10\nvolume = 20\n", "volume", "30");
        assert_eq!(out, "volume = 30\n");
        // No trailing newline, and a value that needs quoting.
        let out = set("theme = air", "wifi_ssid", " Home ");
        assert_eq!(out, "theme = air\nwifi_ssid = \" Home \"\n");
        // An empty file.
        assert_eq!(set("", "volume", "5"), "volume = 5\n");
        // The BOM stays at the front.
        let out = set("\u{feff}theme = air\n", "theme", "liquid");
        assert_eq!(out, "\u{feff}theme = liquid\n");
        assert_eq!(parse(&out).settings.theme.as_deref(), Some("liquid"));
    }
}

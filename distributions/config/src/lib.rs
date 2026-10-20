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
    "dac",
    "output",
];

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
    /// The DAC board on the header, by the register's name, for a board
    /// the player cannot recognise by itself (an I2S-only one) or to
    /// override what it found. The distribution knows the names.
    pub dac: Option<String>,
    /// Where the frames go when the board has a speaker and a DAC both.
    pub output: Option<Output>,
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
        if !KEYS.contains(&key.as_str()) {
            out.unknown.push((i + 1, key));
            continue;
        }
        let value = unquote(v.trim());
        if !take(&mut out.settings, &key, value) {
            out.invalid.push((i + 1, key, value.to_string()));
        }
    }
    out
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
    if let Some(v) = &s.dac {
        applied.push(("dac", v.clone()));
    }
    if let Some(v) = s.output {
        applied.push(("output", v.name().to_string()));
    }
    if applied.is_empty() {
        lines.push("read: no settings in the file".to_string());
    } else {
        for (k, v) in applied {
            lines.push(format!("read: {k} = {v}"));
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

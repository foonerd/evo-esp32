// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! `evoconfig.txt` on this board: read from the card's root at boot,
//! the settings applied, `evostatus.txt` written next to it; and a
//! setting changed on the panel written back into the file in place.
//! The grammar and the typed settings are `evo-esp32-config`'s; this is
//! the card and what this firmware does with each key.

use std::path::PathBuf;

use evo_esp32_config::{
    parse, set, status_lines, Settings, FILE_NAME, STATUS_NAME,
};
use evo_esp32_face::{theme, Style, Theme};
use log::{info, warn};

/// What the file said, kept for the status report and for write-back.
pub struct Config {
    path: PathBuf,
    status_path: PathBuf,
    /// The file's text as read, `None` when there was no file.
    text: Option<String>,
    /// The settings, typed; defaults where the file did not say.
    pub settings: Settings,
    /// The status report's lines so far.
    report: Vec<String>,
    /// The network's line, which changes as the link does.
    network: Option<String>,
}

impl Config {
    /// Read `evoconfig.txt` from the card mounted at `root`. A missing
    /// file is not an error: the defaults stand and the status says so.
    pub fn read(root: &str) -> Self {
        let path = PathBuf::from(format!("{root}/{FILE_NAME}"));
        let status_path = PathBuf::from(format!("{root}/{STATUS_NAME}"));
        let mut report = Vec::new();
        let (text, settings) = match std::fs::read_to_string(&path) {
            Ok(text) => {
                let parsed = parse(&text);
                report.extend(status_lines(&parsed));
                if parsed.is_clean() {
                    info!("config: {FILE_NAME} read, every line understood");
                } else {
                    warn!(
                        "config: {FILE_NAME} read; {} unknown, {} invalid, {} malformed",
                        parsed.unknown.len(),
                        parsed.invalid.len(),
                        parsed.malformed.len()
                    );
                }
                (Some(text), parsed.settings)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                info!("config: no {FILE_NAME} on the card; defaults");
                report.push(format!(
                    "no {FILE_NAME} at the card's root; the defaults apply"
                ));
                (None, Settings::default())
            }
            Err(e) => {
                warn!("config: {FILE_NAME}: {e}");
                report.push(format!(
                    "{FILE_NAME} could not be read: {e}; the defaults apply"
                ));
                (None, Settings::default())
            }
        };
        Self {
            path,
            status_path,
            text,
            settings,
            report,
            network: None,
        }
    }

    /// The theme the file names, or `fallback`; an unknown name is
    /// reported and the fallback stands.
    pub fn theme(&mut self, fallback: Theme) -> Theme {
        match self.settings.theme.as_deref() {
            None => fallback,
            Some(name) => match theme::Theme::named(name) {
                Some(t) => t,
                None => {
                    self.report.push(format!(
                        "theme = {name}: no such theme (the themes are {}); {} stands",
                        theme::ALL.iter().map(|t| t.name).collect::<Vec<_>>().join(", "),
                        fallback.name
                    ));
                    fallback
                }
            },
        }
    }

    /// The style the file names, or `fallback`, likewise.
    pub fn style(&mut self, fallback: Style) -> Style {
        match self.settings.style.as_deref() {
            None => fallback,
            Some(name) => match Style::named(name) {
                Some(s) => s,
                None => {
                    self.report.push(format!(
                        "style = {name}: no such style (disc, card or faders); {} stands",
                        fallback.name()
                    ));
                    fallback
                }
            },
        }
    }

    /// A line for the status report about something this firmware did
    /// or does not do yet.
    pub fn note(&mut self, line: impl Into<String>) {
        self.report.push(line.into());
    }

    /// The network's line in the status, replacing the last.
    pub fn set_network(&mut self, line: impl Into<String>) {
        self.network = Some(line.into());
    }

    /// Write `evostatus.txt`: what the player is, then the report, then
    /// the network.
    pub fn write_status(&self, head: &[String]) {
        let mut text = String::new();
        for l in head
            .iter()
            .chain(self.report.iter())
            .chain(self.network.iter())
        {
            text.push_str(l);
            text.push('\n');
        }
        if let Err(e) = std::fs::write(&self.status_path, text) {
            warn!("config: {STATUS_NAME}: {e}");
        }
    }

    /// Put `key = value` into the file in place and write it; the file is
    /// created from nothing if the card had none.
    pub fn save(&mut self, key: &str, value: &str) {
        let text = set(self.text.as_deref().unwrap_or(""), key, value);
        match std::fs::write(&self.path, &text) {
            Ok(()) => {
                info!("config: {key} = {value} written to {FILE_NAME}");
                self.text = Some(text);
            }
            Err(e) => warn!("config: {FILE_NAME}: {e}"),
        }
    }
}

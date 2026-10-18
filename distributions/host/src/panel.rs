// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The panel as a consumer: the companion's pages drawn from the
//! fabric's projections.
//!
//! Nothing here is a plugin. The presenter reads the `playback` and
//! `delivery` projections, maps them to the companion's
//! [`PlayerState`] (the shape its pages were written against) and draws
//! the resting face with the companion's `core`. On a device the same
//! code draws to the panel; here it draws to a buffer and writes a PNG.

use std::path::Path;

use companion_core::ui::{self, face_text_slot, Layout, TextPane};
use companion_core::PlayerState;
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::Size;
use embedded_graphics_simulator::{OutputSettings, SimulatorDisplay};
use evo_esp32_sdk::contract::audio::{delivery, playback};

/// The fabric's state in the companion's words.
///
/// `seek` is milliseconds and `duration` seconds, as Volumio publishes
/// them and the companion's pages expect; `status` is the three words the
/// pages test for.
pub fn player_state(
    pb: &playback::State,
    out: Option<&delivery::State>,
) -> PlayerState {
    let status = match pb.status {
        playback::Status::Playing => "play",
        playback::Status::Paused => "pause",
        playback::Status::Stopped => "stop",
    };
    let track = pb.track.as_ref();
    PlayerState {
        status: Some(status.into()),
        title: track.map(|t| t.title.clone()),
        artist: track.and_then(|t| t.artist.clone()),
        album: track.and_then(|t| t.album.clone()),
        seek: Some(pb.position_ms),
        duration: track.and_then(|t| t.duration_ms).map(|ms| ms / 1000),
        track_type: pb.format.as_ref().map(|f| f.codec.clone()),
        samplerate: pb
            .format
            .as_ref()
            .map(|f| format!("{:.1} kHz", f.sample_rate as f32 / 1000.0)),
        bitdepth: pb.format.as_ref().map(|f| format!("{} bit", f.bits)),
        volume: out.map(|o| o.volume),
        mute: out.map(|o| o.muted),
        ..PlayerState::default()
    }
}

/// The face, drawn into a buffer the size of the panel.
pub struct Panel {
    layout: Layout,
    display: SimulatorDisplay<Rgb565>,
    pane: TextPane,
}

impl Panel {
    /// A panel at `rotation` degrees, showing nothing yet.
    pub fn new(rotation: u16) -> Self {
        let layout = Layout::for_rotation(rotation);
        let frame = layout.frame.size;
        Self {
            layout,
            display: SimulatorDisplay::new(Size::new(
                frame.width,
                frame.height,
            )),
            pane: TextPane::default(),
        }
    }

    /// Draw the resting face for `state`. Full repaint; the host has no
    /// panel to flicker.
    pub fn draw(&mut self, state: &PlayerState) {
        self.pane.set(
            state.title.as_deref().unwrap_or(""),
            state.artist.as_deref().unwrap_or(""),
            state.album.as_deref().unwrap_or(""),
            face_text_slot(&self.layout),
        );
        let Ok(()) = ui::draw(
            &mut self.display,
            &self.layout,
            state,
            None,
            &self.pane,
            None,
            false,
        );
    }

    /// Write what is drawn to `path` as a PNG at panel resolution.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        self.display
            .to_rgb_output_image(&OutputSettings::default())
            .save_png(path)
            .map_err(|e| format!("{}: {e}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embedded_graphics::prelude::{Point, RgbColor};
    use evo_esp32_sdk::contract::audio::Track;

    fn playing() -> playback::State {
        playback::State {
            status: playback::Status::Playing,
            track: Some(Track {
                uri: "file:///a.flac".into(),
                title: "Song".into(),
                artist: Some("Band".into()),
                album: None,
                duration_ms: Some(200_000),
            }),
            position_ms: 65_000,
            format: Some(playback::Format {
                codec: "flac".into(),
                sample_rate: 44_100,
                bits: 16,
                channels: 2,
            }),
        }
    }

    #[test]
    fn the_mapping_speaks_volumio() {
        let out = delivery::State {
            sink: "null".into(),
            volume: 40,
            muted: false,
        };
        let s = player_state(&playing(), Some(&out));
        assert_eq!(s.status.as_deref(), Some("play"));
        assert_eq!(s.title.as_deref(), Some("Song"));
        assert_eq!(s.seek, Some(65_000));
        assert_eq!(s.duration, Some(200));
        assert_eq!(s.volume, Some(40));
        assert_eq!(s.samplerate.as_deref(), Some("44.1 kHz"));
        assert!(s.is_playing());
        let frac = s.progress().unwrap();
        assert!((frac - 0.325).abs() < 0.001);

        let stopped = player_state(&playback::State::default(), None);
        assert_eq!(stopped.status.as_deref(), Some("stop"));
        assert_eq!(stopped.title, None);
    }

    #[test]
    fn the_face_is_drawn() {
        let mut p = Panel::new(0);
        p.draw(&player_state(&playing(), None));
        let lit = (0..320)
            .flat_map(|y| (0..240).map(move |x| Point::new(x, y)))
            .filter(|&pt| p.display.get_pixel(pt) != Rgb565::BLACK)
            .count();
        assert!(lit > 1000, "something was painted: {lit} pixels");
    }
}

// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The panel as a consumer: the companion's pages drawn from the
//! fabric's projections.
//!
//! Nothing here is a plugin. The presenter reads the `playback` and
//! `delivery` projections, maps them to the companion's [`PlayerState`]
//! (the shape its pages were written against) and draws the resting
//! face with the companion's `core` on any RGB565 draw target: the
//! board's panel, or a buffer the host writes as a PNG.
//!
//! A full frame is tens of milliseconds over SPI, so [`Face::refresh`]
//! repaints the whole face only when the scene changes (track, status,
//! format, volume), the text block when the ticker moves, and the seek
//! strip when the position has moved a second.

use companion_core::ui::{self, face_text_slot, Layout, TextPane};
pub use companion_core::PlayerState;
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::DrawTarget;
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

/// What a refresh painted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Repaint {
    /// Nothing changed.
    None,
    /// The seek strip.
    Progress,
    /// The text block, the ticker having moved.
    Rows,
    /// The whole face: a scene change, or the first frame.
    Full,
}

/// The resting face, repainted in place as the state moves.
pub struct Face {
    layout: Layout,
    pane: TextPane,
    /// The last state drawn, with `seek` cleared: the scene.
    scene: Option<PlayerState>,
    /// The last whole second of position drawn.
    second: Option<u64>,
}

impl Face {
    /// A face at `rotation` degrees, nothing drawn yet.
    pub fn new(rotation: u16) -> Self {
        Self {
            layout: Layout::for_rotation(rotation),
            pane: TextPane::default(),
            scene: None,
            second: None,
        }
    }

    /// The layout the face is drawn to; its `frame` is the panel's size.
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// Bring `target` up to date with `state`, painting as little as the
    /// change allows. Call it at the ticker's cadence.
    pub fn refresh<D>(
        &mut self,
        target: &mut D,
        state: &PlayerState,
    ) -> Result<Repaint, D::Error>
    where
        D: DrawTarget<Color = Rgb565>,
    {
        self.pane.set(
            state.title.as_deref().unwrap_or(""),
            state.artist.as_deref().unwrap_or(""),
            state.album.as_deref().unwrap_or(""),
            face_text_slot(&self.layout),
        );
        let scene = PlayerState {
            seek: None,
            ..state.clone()
        };
        let second = state.seek.map(|ms| ms / 1000);
        if self.scene.as_ref() != Some(&scene) {
            ui::draw(
                target,
                &self.layout,
                state,
                None,
                &self.pane,
                None,
                false,
            )?;
            self.scene = Some(scene);
            self.second = second;
            return Ok(Repaint::Full);
        }
        if self.pane.step() {
            ui::draw_rows(target, &self.layout, &self.pane)?;
            return Ok(Repaint::Rows);
        }
        if second != self.second {
            ui::draw_progress(target, &self.layout, state, None)?;
            self.second = second;
            return Ok(Repaint::Progress);
        }
        Ok(Repaint::None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embedded_graphics::prelude::{Point, RgbColor, Size};
    use embedded_graphics_simulator::SimulatorDisplay;
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
    fn the_face_repaints_only_what_moved() {
        let mut face = Face::new(0);
        let mut display = SimulatorDisplay::<Rgb565>::new(Size::new(240, 320));
        let pb = playing();
        let state = player_state(&pb, None);
        assert_eq!(face.refresh(&mut display, &state), Ok(Repaint::Full));
        let lit = (0..320)
            .flat_map(|y| (0..240).map(move |x| Point::new(x, y)))
            .filter(|&pt| display.get_pixel(pt) != Rgb565::BLACK)
            .count();
        assert!(lit > 1000, "something was painted: {lit} pixels");

        assert_eq!(face.refresh(&mut display, &state), Ok(Repaint::None));
        let moved = PlayerState {
            seek: Some(66_500),
            ..state.clone()
        };
        assert_eq!(face.refresh(&mut display, &moved), Ok(Repaint::Progress));
        let paused = PlayerState {
            status: Some("pause".into()),
            ..moved
        };
        assert_eq!(face.refresh(&mut display, &paused), Ok(Repaint::Full));
    }
}

// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The panel on the host: the player's face (`evo-esp32-face`) drawn into
//! a buffer the size of the panel, written as a PNG at the end.

use std::path::Path;

use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics_simulator::{OutputSettings, SimulatorDisplay};
pub use evo_esp32_face::View;
use evo_esp32_face::{Face, Style, Theme};

/// The face, drawn into a buffer the size of the panel.
pub struct Panel {
    face: Face,
    display: SimulatorDisplay<Rgb565>,
}

impl Panel {
    /// A panel at `rotation` degrees in `style` and `theme`, showing
    /// nothing yet.
    pub fn new(rotation: u16, style: Style, theme: Theme) -> Self {
        let face = Face::new(rotation, style, theme);
        let frame = face.layout().frame;
        Self {
            face,
            display: SimulatorDisplay::new(frame),
        }
    }

    /// Bring the buffer up to date with `view`.
    pub fn draw(&mut self, view: &View) {
        let Ok(_) = self.face.refresh_whole(&mut self.display, view);
    }

    /// The face itself, to set its screen, library, facts or art.
    pub fn face_mut(&mut self) -> &mut Face {
        &mut self.face
    }

    /// Write what is drawn to `path` as a PNG at panel resolution.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        self.display
            .to_rgb_output_image(&OutputSettings::default())
            .save_png(path)
            .map_err(|e| format!("{}: {e}", path.display()))
    }
}

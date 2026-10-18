// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The panel on the host: the shared face (`evo-esp32-panel`) drawn into
//! a buffer the size of the panel, written as a PNG at the end.

use std::path::Path;

use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::Size;
use embedded_graphics_simulator::{OutputSettings, SimulatorDisplay};
pub use evo_esp32_panel::player_state;
use evo_esp32_panel::{Face, PlayerState};

/// The face, drawn into a buffer the size of the panel.
pub struct Panel {
    face: Face,
    display: SimulatorDisplay<Rgb565>,
}

impl Panel {
    /// A panel at `rotation` degrees, showing nothing yet.
    pub fn new(rotation: u16) -> Self {
        let face = Face::new(rotation);
        let frame = face.layout().frame.size;
        Self {
            face,
            display: SimulatorDisplay::new(Size::new(
                frame.width,
                frame.height,
            )),
        }
    }

    /// Bring the buffer up to date with `state`.
    pub fn draw(&mut self, state: &PlayerState) {
        let Ok(_) = self.face.refresh(&mut self.display, state);
    }

    /// Write what is drawn to `path` as a PNG at panel resolution.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        self.display
            .to_rgb_output_image(&OutputSettings::default())
            .save_png(path)
            .map_err(|e| format!("{}: {e}", path.display()))
    }
}

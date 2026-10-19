// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The family's themes, as evo-ui defines them.
//!
//! Each token is evo-ui's (`apps/evo-ui-shell/src/styles.css`, OKLCH),
//! converted to sRGB once and quantised to RGB565 here, so the face and
//! the shell share a palette by construction rather than by eye.

use embedded_graphics::pixelcolor::Rgb565;

/// A theme's tokens, evo-ui's names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    /// The theme's name as evo-ui's class names it.
    pub name: &'static str,
    /// The page.
    pub background: Rgb565,
    /// Text on the page.
    pub foreground: Rgb565,
    /// A raised surface: the nav bar, a list row.
    pub card: Rgb565,
    /// A control at rest.
    pub secondary: Rgb565,
    /// Text that supports rather than leads.
    pub muted_fg: Rgb565,
    /// The one colour that means "this": the play button, the progress.
    pub accent: Rgb565,
    /// Text or a glyph on the accent.
    pub accent_fg: Rgb565,
    /// Hairlines and the empty part of a track.
    pub border: Rgb565,
    /// Status: fine.
    pub ok: Rgb565,
    /// Status: attention.
    pub warn: Rgb565,
    /// Status: wrong.
    pub fail: Rgb565,
}

/// 8-bit sRGB to the panel's colour.
const fn rgb(r: u8, g: u8, b: u8) -> Rgb565 {
    Rgb565::new(r >> 3, g >> 2, b >> 3)
}

/// `.theme-evo-default`.
pub const EVO_DEFAULT: Theme = Theme {
    name: "evo-default",
    background: rgb(0, 2, 8), // oklch(0.08 0.03 250)
    foreground: rgb(232, 240, 243), // oklch(0.95 0.01 220)
    card: rgb(0, 5, 20),      // oklch(0.12 0.04 255)
    secondary: rgb(0, 14, 33), // oklch(0.16 0.05 250)
    muted_fg: rgb(125, 147, 158), // oklch(0.65 0.03 230)
    accent: rgb(0, 207, 209), // oklch(0.75 0.18 195)
    accent_fg: rgb(0, 2, 8),  // oklch(0.08 0.03 250)
    border: rgb(16, 34, 57),  // oklch(0.25 0.05 255)
    ok: rgb(77, 185, 86),     // oklch(0.7 0.17 145)
    warn: rgb(217, 165, 20),  // oklch(0.75 0.15 85)
    fail: rgb(241, 77, 76),   // oklch(0.65 0.2 25)
};

/// `.theme-night-sky`.
pub const NIGHT_SKY: Theme = Theme {
    name: "night-sky",
    background: rgb(1, 1, 11),
    foreground: rgb(235, 237, 252),
    card: rgb(3, 3, 24),
    secondary: rgb(5, 9, 38),
    muted_fg: rgb(142, 139, 166),
    accent: rgb(179, 123, 255),
    accent_fg: rgb(4, 4, 17),
    border: rgb(35, 41, 85),
    ok: EVO_DEFAULT.ok,
    warn: EVO_DEFAULT.warn,
    fail: EVO_DEFAULT.fail,
};

/// `.theme-sunrise`.
pub const SUNRISE: Theme = Theme {
    name: "sunrise",
    background: rgb(16, 7, 3),
    foreground: rgb(249, 236, 225),
    card: rgb(27, 14, 8),
    secondary: rgb(38, 22, 15),
    muted_fg: rgb(158, 139, 126),
    accent: rgb(235, 134, 86),
    accent_fg: rgb(29, 13, 7),
    border: rgb(68, 45, 33),
    ok: EVO_DEFAULT.ok,
    warn: EVO_DEFAULT.warn,
    fail: EVO_DEFAULT.fail,
};

/// `.theme-air`, the light one.
pub const AIR: Theme = Theme {
    name: "air",
    background: rgb(239, 242, 245),
    foreground: rgb(14, 23, 30),
    card: rgb(247, 249, 250),
    secondary: rgb(227, 233, 236),
    muted_fg: rgb(75, 87, 95),
    accent: rgb(0, 125, 170),
    accent_fg: rgb(245, 249, 251),
    border: rgb(200, 207, 211),
    ok: EVO_DEFAULT.ok,
    warn: EVO_DEFAULT.warn,
    fail: EVO_DEFAULT.fail,
};

/// `.theme-liquid`.
pub const LIQUID: Theme = Theme {
    name: "liquid",
    background: rgb(0, 4, 16),
    foreground: rgb(221, 239, 243),
    card: rgb(0, 12, 27),
    secondary: rgb(0, 23, 38),
    muted_fg: rgb(107, 141, 149),
    accent: rgb(0, 191, 184),
    accent_fg: rgb(0, 4, 13),
    border: rgb(0, 53, 76),
    ok: EVO_DEFAULT.ok,
    warn: EVO_DEFAULT.warn,
    fail: EVO_DEFAULT.fail,
};

/// Every theme, in evo-ui's order.
pub const ALL: &[Theme] = &[EVO_DEFAULT, NIGHT_SKY, SUNRISE, AIR, LIQUID];

impl Theme {
    /// The theme named `name`, if there is one.
    pub fn named(name: &str) -> Option<Theme> {
        ALL.iter().copied().find(|t| t.name == name)
    }
}

/// `a` towards `b` by `t` of 16: the panel's colours mixed in their own
/// depth, for a tint or an anti-aliased edge.
pub fn mix(a: Rgb565, b: Rgb565, t: u8) -> Rgb565 {
    use embedded_graphics::pixelcolor::RgbColor;
    let t = t.min(16) as u16;
    let ch = |x: u8, y: u8| ((x as u16 * (16 - t) + y as u16 * t) / 16) as u8;
    Rgb565::new(ch(a.r(), b.r()), ch(a.g(), b.g()), ch(a.b(), b.b()))
}

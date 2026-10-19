// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The player's own face.
//!
//! Nothing here is a plugin. The face is a consumer: it reads the
//! `playback` and `delivery` projections as a [`View`] and draws Now
//! Playing on any RGB565 draw target: the board's panel, or a buffer the
//! host writes as a PNG. Two styles ([`Style::Disc`], [`Style::Card`]),
//! the family's themes ([`theme`]), four rotations ([`Layout`]).
//!
//! A full frame is tens of milliseconds over SPI, so [`Face::refresh`]
//! repaints the whole face only when the scene changes (track, status,
//! format, style, theme) and otherwise only what moved: the second's
//! worth of progress (the ring's new arc, the strip, the two times), or
//! the volume slider.

#![no_std]

extern crate alloc;

pub mod art;
pub mod draw;
pub mod layout;
pub mod text;
pub mod theme;
pub mod touch;

use alloc::format;
use alloc::string::{String, ToString};

pub use art::Art;
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::{DrawTarget, Point, Size};
use embedded_graphics::primitives::Rectangle;
use evo_esp32_sdk::contract::audio::{delivery, playback};
pub use layout::{Hotspot, Layout, Slider, Style};
use text::Align;
use theme::mix;
pub use theme::Theme;

/// Where playback stands, as the face tells it apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Status {
    /// Nothing in custody.
    #[default]
    Stopped,
    /// Frames are going out.
    Playing,
    /// Held.
    Paused,
}

/// What the face draws: the two projections, flattened.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct View {
    /// Where playback stands.
    pub status: Status,
    /// The track's title; empty when nothing is in custody.
    pub title: String,
    /// Its artist, or empty.
    pub artist: String,
    /// Its album, or empty.
    pub album: String,
    /// Position, milliseconds.
    pub position_ms: u64,
    /// Length, milliseconds, when known.
    pub duration_ms: Option<u64>,
    /// The stream's codec, as the decoder names it.
    pub codec: String,
    /// Frames per second, 0 until known.
    pub sample_rate: u32,
    /// Bits per sample, 0 until known.
    pub bits: u8,
    /// Level, percent.
    pub volume: u8,
    /// Muted.
    pub muted: bool,
    /// Where the music comes from, for the status strip: "Card".
    pub source: &'static str,
}

impl View {
    /// The face's view of the projections; `source` names the music's
    /// origin for the status strip.
    pub fn new(
        pb: &playback::State,
        out: Option<&delivery::State>,
        source: &'static str,
    ) -> View {
        let track = pb.track.as_ref();
        let format = pb.format.as_ref();
        View {
            status: match pb.status {
                playback::Status::Stopped => Status::Stopped,
                playback::Status::Playing => Status::Playing,
                playback::Status::Paused => Status::Paused,
            },
            title: track.map(|t| t.title.clone()).unwrap_or_default(),
            artist: track.and_then(|t| t.artist.clone()).unwrap_or_default(),
            album: track.and_then(|t| t.album.clone()).unwrap_or_default(),
            position_ms: pb.position_ms,
            duration_ms: track.and_then(|t| t.duration_ms),
            codec: format.map(|f| f.codec.clone()).unwrap_or_default(),
            sample_rate: format.map(|f| f.sample_rate).unwrap_or(0),
            bits: format.map(|f| f.bits).unwrap_or(0),
            volume: out.map(|o| o.volume).unwrap_or(0),
            muted: out.map(|o| o.muted).unwrap_or(false),
            source,
        }
    }

    /// Position over duration, 0 to 1, when both are known.
    pub fn progress(&self) -> Option<f32> {
        let d = self.duration_ms.filter(|&d| d > 0)?;
        Some((self.position_ms as f32 / d as f32).clamp(0.0, 1.0))
    }

    /// "FLAC 96 kHz 16 bit", or empty before the format is known.
    pub fn format_line(&self) -> String {
        if self.sample_rate == 0 {
            return String::new();
        }
        let khz = self.sample_rate / 1000;
        let tenth = (self.sample_rate % 1000) / 100;
        let rate = if tenth == 0 {
            format!("{khz} kHz")
        } else {
            format!("{khz}.{tenth} kHz")
        };
        format!("{} {rate} {} bit", self.codec.to_uppercase(), self.bits)
    }
}

/// `m:ss`, or `h:mm:ss` past the hour.
pub fn clock(ms: u64) -> String {
    let s = ms / 1000;
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// What a refresh painted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Repaint {
    /// Nothing changed.
    None,
    /// The progress: ring or strip, and the times.
    Progress,
    /// The volume slider.
    Volume,
    /// The transport: play became pause or back.
    Transport,
    /// The art: a picture arrived, or went.
    Art,
    /// One stage of a full frame; call again soon for the next.
    Partial,
    /// The whole face: a scene change, or the first frame, complete.
    Full,
}

/// The scene: everything on the face but the position and the volume.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Scene {
    status: Status,
    title: String,
    artist: String,
    album: String,
    format: String,
    duration_ms: Option<u64>,
    style: Style,
    theme: &'static str,
}

/// Now Playing, repainted in place as the view moves.
pub struct Face {
    layout: Layout,
    theme: Theme,
    scene: Option<Scene>,
    second: Option<u64>,
    level: Option<(u8, bool)>,
    /// How far round the ring the accent reaches.
    ring_to: f32,
    /// The next stage of a full paint in progress.
    stage: Option<u8>,
    /// The album's picture, when the distribution has one.
    art: Option<Art>,
    /// The art changed since it was last painted.
    art_due: bool,
}

/// A full frame is painted in stages, one per `refresh`, so the caller's
/// loop (the decoder's tick, on a board) runs between them: no stage
/// holds the loop as long as a whole frame would.
const STAGES: u8 = 10;

/// The card's corner radius: evo-ui's `--radius`, 0.75 rem.
const RADIUS: i32 = 12;
/// The knob on a slider.
const KNOB: i32 = 5;

impl Face {
    /// A face at `rotation` degrees in `style` and `theme`, nothing drawn.
    pub fn new(rotation: u16, style: Style, theme: Theme) -> Self {
        Self {
            layout: Layout::new(rotation, style),
            theme,
            scene: None,
            second: None,
            level: None,
            ring_to: 0.0,
            stage: None,
            art: None,
            art_due: false,
        }
    }

    /// The album's picture, or none: the next refresh repaints the art.
    /// A picture of another size than the layout's art square is drawn
    /// from its top-left corner, so fit it first ([`art::fit`]).
    pub fn set_art(&mut self, art: Option<Art>) {
        if self.art != art {
            self.art = art;
            self.art_due = true;
        }
    }

    /// The album's picture the face holds.
    pub fn art(&self) -> Option<&Art> {
        self.art.as_ref()
    }

    /// The layout the face is drawn to; its `frame` is the panel's size.
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// The theme in use.
    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    /// Change style; the next refresh repaints everything.
    pub fn set_style(&mut self, style: Style) {
        self.layout = Layout::new(self.layout.rotation, style);
        self.scene = None;
    }

    /// Change theme; the next refresh repaints everything.
    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
        self.scene = None;
    }

    /// Bring `target` up to date with `view`, painting as little as the
    /// change allows. A full frame (the first, or a scene change) comes
    /// in ten calls, each returning [`Repaint::Partial`] until the last
    /// returns [`Repaint::Full`]; call again as soon as convenient.
    pub fn refresh<D>(
        &mut self,
        target: &mut D,
        view: &View,
    ) -> Result<Repaint, D::Error>
    where
        D: DrawTarget<Color = Rgb565>,
    {
        let scene = Scene {
            status: view.status,
            title: view.title.clone(),
            artist: view.artist.clone(),
            album: view.album.clone(),
            format: view.format_line(),
            duration_ms: view.duration_ms,
            style: self.layout.style,
            theme: self.theme.name,
        };
        let second = Some(view.position_ms / 1000);
        let level = Some((view.volume, view.muted));
        if let Some(was) = self.scene.as_ref().filter(|was| **was != scene) {
            // Only the status moved: the play button, not the face.
            let only_status = Scene {
                status: scene.status,
                ..was.clone()
            } == scene;
            if only_status {
                self.paint_transport(target, view)?;
                self.scene = Some(scene);
                return Ok(Repaint::Transport);
            }
        }
        if self.scene.as_ref() != Some(&scene) {
            self.scene = Some(scene);
            self.stage = Some(0);
        }
        if let Some(stage) = self.stage {
            self.paint_stage(target, view, stage)?;
            if stage + 1 < STAGES {
                self.stage = Some(stage + 1);
                return Ok(Repaint::Partial);
            }
            self.stage = None;
            self.art_due = false;
            return Ok(Repaint::Full);
        }
        if self.art_due {
            self.paint_art(target)?;
            self.art_due = false;
            return Ok(Repaint::Art);
        }
        if second != self.second {
            self.paint_progress(target, view)?;
            self.second = second;
            return Ok(Repaint::Progress);
        }
        if level != self.level {
            self.paint_volume(target, view)?;
            self.level = level;
            return Ok(Repaint::Volume);
        }
        Ok(Repaint::None)
    }

    /// [`refresh`](Self::refresh) through to a frame that is not partial:
    /// a whole frame in one call, for a host that draws an image.
    pub fn refresh_whole<D>(
        &mut self,
        target: &mut D,
        view: &View,
    ) -> Result<Repaint, D::Error>
    where
        D: DrawTarget<Color = Rgb565>,
    {
        loop {
            let painted = self.refresh(target, view)?;
            if painted != Repaint::Partial {
                return Ok(painted);
            }
        }
    }

    /// One stage of the full frame, in painting order: the ground, the
    /// status strip, the art, the three text lines, the progress, the
    /// volume, the transport, the navigation.
    fn paint_stage<D>(
        &mut self,
        target: &mut D,
        view: &View,
        stage: u8,
    ) -> Result<(), D::Error>
    where
        D: DrawTarget<Color = Rgb565>,
    {
        let l = self.layout;
        let t = self.theme;
        match stage {
            0 => target.clear(t.background),
            1 => {
                // Where the music is from, and the stream.
                let status = if view.format_line().is_empty() {
                    view.source.to_string()
                } else {
                    format!("{}  ·  {}", view.source, view.format_line())
                };
                text::TINY.line(
                    target,
                    &status,
                    l.status,
                    Align::Left,
                    t.muted_fg,
                    t.background,
                )
            }
            2 => self.paint_art(target),
            3..=5 => self.paint_text_line(target, view, stage - 3),
            6 => {
                let frac = view.progress().unwrap_or(0.0);
                self.paint_ring(target, 0.0, frac, true)?;
                self.paint_times(target, view)?;
                self.paint_seek(target, view)?;
                self.second = Some(view.position_ms / 1000);
                Ok(())
            }
            7 => {
                self.paint_volume(target, view)?;
                self.level = Some((view.volume, view.muted));
                Ok(())
            }
            8 => self.paint_transport(target, view),
            _ => self.paint_nav(target),
        }
    }

    /// The art: the album's picture in a disc or on the card, or the
    /// stand-in; the card's text band is laid over its foot either way.
    fn paint_art<D>(&self, target: &mut D) -> Result<(), D::Error>
    where
        D: DrawTarget<Color = Rgb565>,
    {
        let l = self.layout;
        let t = self.theme;
        match (l.style, &self.art) {
            (Style::Disc, Some(art)) => {
                draw::art_disc(target, l.art, art, t.background)
            }
            (Style::Disc, None) => draw::placeholder_disc(target, l.art, &t),
            (Style::Card, art) => {
                match art {
                    Some(art) => draw::art_card(
                        target,
                        l.art,
                        RADIUS,
                        art,
                        t.background,
                    )?,
                    None => draw::placeholder_card(target, l.art, RADIUS, &t)?,
                }
                if let Some((band, band_colour)) = self.card_band() {
                    draw::rounded_corners(
                        target,
                        band,
                        RADIUS,
                        band_colour,
                        t.background,
                        false,
                        true,
                    )?;
                }
                Ok(())
            }
        }
    }

    /// The card style's text band over the art's foot, where the layout
    /// centres the text; the band and its colour.
    fn card_band(&self) -> Option<(Rectangle, Rgb565)> {
        let l = self.layout;
        let t = self.theme;
        (l.style == Style::Card && l.nav_axis == layout::NavAxis::Bar).then(
            || {
                let band_top = l.title.top_left.y - 8;
                let band = Rectangle::new(
                    Point::new(l.art.top_left.x, band_top),
                    Size::new(
                        l.art.size.width,
                        (l.art.top_left.y + l.art.size.height as i32 - band_top)
                            as u32,
                    ),
                );
                (band, mix(t.secondary, t.background, 10))
            },
        )
    }

    /// Text line `n`: 0 the title, 1 the artist, 2 the album.
    fn paint_text_line<D>(
        &self,
        target: &mut D,
        view: &View,
        n: u8,
    ) -> Result<(), D::Error>
    where
        D: DrawTarget<Color = Rgb565>,
    {
        let l = self.layout;
        let t = self.theme;
        let (title, artist, album) = if view.title.is_empty() {
            (
                match view.status {
                    Status::Stopped => "Nothing playing",
                    _ => "",
                },
                "",
                "",
            )
        } else {
            (
                view.title.as_str(),
                view.artist.as_str(),
                view.album.as_str(),
            )
        };
        let centred = l.nav_axis == layout::NavAxis::Bar;
        let band = self.card_band();
        let under = band.map_or(t.background, |(_, colour)| colour);
        let align = match l.style {
            Style::Disc if centred => Align::Centre,
            _ => Align::Left,
        };
        // On the band the lines step down a size, to sit on the art.
        let (artist_font, album_font) = if band.is_some() {
            (&text::SMALL, &text::TINY)
        } else {
            (&text::BODY, &text::SMALL)
        };
        match n {
            0 => text::TITLE.line(
                target,
                title,
                l.title,
                align,
                t.foreground,
                under,
            ),
            1 => artist_font
                .line(target, artist, l.artist, align, t.muted_fg, under),
            _ => album_font.line(
                target,
                album,
                l.album,
                align,
                mix(t.muted_fg, under, 5),
                under,
            ),
        }
    }

    /// The navigation bar or rail, with the first item active.
    fn paint_nav<D>(&self, target: &mut D) -> Result<(), D::Error>
    where
        D: DrawTarget<Color = Rgb565>,
    {
        let l = self.layout;
        let t = self.theme;
        target.fill_solid(&l.nav, t.card)?;
        let hairline = match l.nav_axis {
            layout::NavAxis::Bar => {
                Rectangle::new(l.nav.top_left, Size::new(l.nav.size.width, 1))
            }
            layout::NavAxis::Rail => Rectangle::new(
                Point::new(
                    l.nav.top_left.x + l.nav.size.width as i32 - 1,
                    l.nav.top_left.y,
                ),
                Size::new(1, l.nav.size.height),
            ),
        };
        target.fill_solid(&hairline, t.border)?;
        for i in 0..3 {
            let c = l.nav_item(i);
            let active = i == 0;
            let colour = if active { t.accent } else { t.muted_fg };
            let under = if active {
                let pill_colour = mix(t.card, t.accent, 4);
                let pill =
                    Rectangle::new(c - Point::new(22, 11), Size::new(44, 22));
                draw::rounded(target, pill, 11, pill_colour, t.card)?;
                pill_colour
            } else {
                t.card
            };
            match i {
                0 => draw::icon_note(target, c, 8, colour, under)?,
                1 => draw::icon_list(target, c, 7, colour)?,
                _ => draw::icon_gear(target, c, 8, colour, under)?,
            }
        }
        Ok(())
    }

    /// The three buttons; the big one shows pause while playing.
    fn paint_transport<D>(
        &self,
        target: &mut D,
        view: &View,
    ) -> Result<(), D::Error>
    where
        D: DrawTarget<Color = Rgb565>,
    {
        let (l, t) = (&self.layout, &self.theme);
        let tr = l.transport;
        for (c, forward) in [(tr.prev, false), (tr.next, true)] {
            draw::disc(target, c, tr.skip_r as i32, t.border, t.background)?;
            draw::disc(target, c, tr.skip_r as i32 - 1, t.secondary, t.border)?;
            draw::icon_skip(
                target,
                c,
                tr.skip_r as i32 * 9 / 16,
                forward,
                t.foreground,
                t.secondary,
            )?;
        }
        draw::disc(target, tr.play, tr.play_r as i32, t.accent, t.background)?;
        match view.status {
            Status::Playing => draw::icon_pause(
                target,
                tr.play,
                tr.play_r as i32 / 2,
                t.accent_fg,
            )?,
            _ => draw::icon_play(
                target,
                tr.play,
                tr.play_r as i32 / 2,
                t.accent_fg,
                t.accent,
            )?,
        }
        Ok(())
    }

    fn paint_progress<D>(
        &mut self,
        target: &mut D,
        view: &View,
    ) -> Result<(), D::Error>
    where
        D: DrawTarget<Color = Rgb565>,
    {
        let frac = view.progress().unwrap_or(0.0);
        if frac < self.ring_to {
            self.paint_ring(target, 0.0, frac, true)?;
        } else {
            self.paint_ring(target, self.ring_to, frac, false)?;
        }
        self.paint_times(target, view)?;
        self.paint_seek(target, view)
    }

    /// The ring's track (if `lay_track`) and its accent from `from` to
    /// `to`; nothing in the card style.
    fn paint_ring<D>(
        &mut self,
        target: &mut D,
        from: f32,
        to: f32,
        lay_track: bool,
    ) -> Result<(), D::Error>
    where
        D: DrawTarget<Color = Rgb565>,
    {
        let Some((inner, outer)) = self.layout.ring_radii() else {
            return Ok(());
        };
        let c = self.layout.art_centre();
        if lay_track {
            draw::ring_arc(
                target,
                c,
                (outer, inner),
                (from, to),
                self.theme.border,
                self.theme.accent,
                self.theme.background,
            )?;
        } else {
            draw::arc(
                target,
                c,
                (outer, inner),
                (from, to),
                self.theme.accent,
                self.theme.background,
            )?;
        }
        self.ring_to = to;
        Ok(())
    }

    /// Elapsed and duration.
    fn paint_times<D>(
        &self,
        target: &mut D,
        view: &View,
    ) -> Result<(), D::Error>
    where
        D: DrawTarget<Color = Rgb565>,
    {
        let l = self.layout;
        let t = self.theme;
        text::SMALL.line(
            target,
            &clock(view.position_ms),
            l.elapsed,
            Align::Left,
            t.muted_fg,
            t.background,
        )?;
        let dur = view.duration_ms.map(clock).unwrap_or_default();
        text::SMALL.line(
            target,
            &dur,
            l.duration,
            Align::Right,
            t.muted_fg,
            t.background,
        )
    }

    /// The seek strip with its knob, where the layout has one.
    fn paint_seek<D>(&self, target: &mut D, view: &View) -> Result<(), D::Error>
    where
        D: DrawTarget<Color = Rgb565>,
    {
        match self.layout.seek {
            Some(bar) => self.paint_bar(
                target,
                bar,
                view.progress().unwrap_or(0.0),
                self.theme.accent,
                self.theme.foreground,
            ),
            None => Ok(()),
        }
    }

    /// The volume slider: icon, bar, label.
    fn paint_volume<D>(
        &self,
        target: &mut D,
        view: &View,
    ) -> Result<(), D::Error>
    where
        D: DrawTarget<Color = Rgb565>,
    {
        let l = self.layout;
        let t = self.theme;
        let s = l.volume;
        let frac = view.volume.min(100) as f32 / 100.0;
        // Muted: the bar in the border's colour, the knob hollow.
        let (fill, knob) = if view.muted {
            (t.border, t.muted_fg)
        } else {
            (t.accent, t.foreground)
        };
        self.paint_bar(target, s.bar, frac, fill, knob)?;
        if let Some(c) = s.icon {
            let clear = Rectangle::with_center(c, Size::new(18, 18));
            target.fill_solid(&clear, t.background)?;
            draw::icon_speaker(
                target,
                c,
                7,
                !view.muted,
                t.muted_fg,
                t.background,
            )?;
        }
        if let Some(area) = s.label {
            let label = if view.muted {
                "mute".to_string()
            } else {
                format!("{}%", view.volume)
            };
            text::SMALL.line(
                target,
                &label,
                area,
                Align::Right,
                t.muted_fg,
                t.background,
            )?;
        }
        Ok(())
    }

    /// A bar with its filled part and a knob, cleared first so a shorter
    /// fill replaces a longer one cleanly.
    fn paint_bar<D>(
        &self,
        target: &mut D,
        bar: Rectangle,
        frac: f32,
        fill: Rgb565,
        knob: Rgb565,
    ) -> Result<(), D::Error>
    where
        D: DrawTarget<Color = Rgb565>,
    {
        let t = self.theme;
        let travel = Rectangle::new(
            bar.top_left - Point::new(KNOB + 1, KNOB + 1),
            bar.size + Size::new(2 * (KNOB as u32 + 1), 2 * (KNOB as u32 + 1)),
        );
        target.fill_solid(&travel, t.background)?;
        draw::rounded(target, bar, 2, t.border, t.background)?;
        let filled = (bar.size.width as f32 * frac.clamp(0.0, 1.0)) as u32;
        if filled > 0 {
            draw::rounded(
                target,
                Rectangle::new(
                    bar.top_left,
                    Size::new(filled, bar.size.height),
                ),
                2,
                fill,
                t.background,
            )?;
        }
        let k = Point::new(bar.top_left.x + filled as i32, bar.center().y);
        draw::disc(target, k, KNOB, knob, t.background)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embedded_graphics::prelude::{Dimensions, PointsIter, RgbColor};
    use embedded_graphics_simulator::SimulatorDisplay;
    use evo_esp32_sdk::contract::audio::Track;

    fn playing() -> playback::State {
        playback::State {
            status: playback::Status::Playing,
            track: Some(Track {
                uri: "file:///a.flac".into(),
                title: "Money For Nothing".into(),
                artist: Some("Dire Straits".into()),
                album: Some("Brothers In Arms".into()),
                duration_ms: Some(506_000),
            }),
            position_ms: 119_000,
            format: Some(playback::Format {
                codec: "flac".into(),
                sample_rate: 96_000,
                bits: 16,
                channels: 2,
            }),
        }
    }

    fn out() -> delivery::State {
        delivery::State {
            sink: "i2s".into(),
            volume: 40,
            muted: false,
        }
    }

    #[test]
    fn the_view_reads_the_projections() {
        let v = View::new(&playing(), Some(&out()), "Card");
        assert_eq!(v.status, Status::Playing);
        assert_eq!(v.title, "Money For Nothing");
        assert_eq!(v.format_line(), "FLAC 96 kHz 16 bit");
        assert_eq!(v.volume, 40);
        let frac = v.progress().unwrap();
        assert!((frac - 0.235).abs() < 0.001);
        let mut pb = playing();
        pb.format.as_mut().unwrap().sample_rate = 44_100;
        assert_eq!(
            View::new(&pb, None, "Card").format_line(),
            "FLAC 44.1 kHz 16 bit"
        );
        assert_eq!(clock(119_000), "1:59");
        assert_eq!(clock(3_725_000), "1:02:05");
        let stopped = View::new(&playback::State::default(), None, "Card");
        assert_eq!(stopped.status, Status::Stopped);
        assert_eq!(stopped.format_line(), "");
    }

    #[test]
    fn every_rotation_and_style_fits_its_frame() {
        for rotation in [0u16, 90, 180, 270] {
            for style in [Style::Disc, Style::Card] {
                let l = Layout::new(rotation, style);
                let frame = l.bounds();
                let t = l.transport;
                let button = |c: Point, r: u32| {
                    Rectangle::with_center(c, Size::new(2 * r + 1, 2 * r + 1))
                };
                let grow = |r: Rectangle, by: i32| {
                    Rectangle::new(
                        r.top_left - Point::new(by, by),
                        r.size + Size::new(2 * by as u32, 2 * by as u32),
                    )
                };
                let mut regions = alloc::vec![
                    ("status", l.status),
                    ("volume", grow(l.volume.bar, KNOB + 1)),
                    ("play", button(t.play, t.play_r)),
                    ("prev", button(t.prev, t.skip_r)),
                    ("next", button(t.next, t.skip_r)),
                    ("nav", l.nav),
                ];
                if let Some(c) = l.volume.icon {
                    regions.push((
                        "volume icon",
                        Rectangle::with_center(c, Size::new(18, 18)),
                    ));
                }
                if let Some(a) = l.volume.label {
                    regions.push(("volume label", a));
                }
                if let Some(s) = l.seek {
                    regions.push(("seek", grow(s, KNOB + 1)));
                }
                // The disc's times sit beside the ring's foot, inside the
                // ring's bounding box but outside the ring itself; checked
                // against the circle below rather than the box here.
                match style {
                    Style::Disc => {
                        let (_, outer) = l.ring_radii().unwrap();
                        let c = l.art_centre();
                        for (name, r) in
                            [("elapsed", l.elapsed), ("duration", l.duration)]
                        {
                            for p in [
                                r.top_left,
                                r.top_left + Point::new(r.size.width as i32, 0),
                                r.top_left
                                    + Point::new(0, r.size.height as i32),
                                r.top_left
                                    + Point::new(
                                        r.size.width as i32,
                                        r.size.height as i32,
                                    ),
                            ] {
                                let (dx, dy) = (p.x - c.x, p.y - c.y);
                                assert!(
                                    dx * dx + dy * dy > outer * outer,
                                    "{name} touches the ring at {rotation}"
                                );
                            }
                            assert_eq!(
                                frame.intersection(&r),
                                r,
                                "{name} leaves the frame"
                            );
                        }
                        regions.extend([
                            (
                                "art",
                                grow(l.art, layout::RING_GAP + layout::RING),
                            ),
                            ("title", l.title),
                            ("artist", l.artist),
                            ("album", l.album),
                        ]);
                    }
                    Style::Card => {
                        regions.extend([
                            ("art", l.art),
                            ("elapsed", l.elapsed),
                            ("duration", l.duration),
                        ]);
                        if l.nav_axis == layout::NavAxis::Rail {
                            regions.extend([
                                ("title", l.title),
                                ("artist", l.artist),
                                ("album", l.album),
                            ]);
                        }
                    }
                }
                for (name, r) in &regions {
                    assert_eq!(
                        frame.intersection(r),
                        *r,
                        "{name} at {rotation} {style:?} leaves the frame"
                    );
                }
                for (i, (a, ra)) in regions.iter().enumerate() {
                    for (b, rb) in &regions[i + 1..] {
                        assert!(
                            ra.intersection(rb).is_zero_sized(),
                            "{a} and {b} overlap at {rotation} {style:?}: {ra:?} {rb:?}"
                        );
                    }
                }
                assert_eq!(l.hit(t.play), Some(Hotspot::Play));
                assert_eq!(l.hit(t.next), Some(Hotspot::Next));
                assert_eq!(l.hit(l.nav_item(1)), Some(Hotspot::Nav(1)));
                match l.hit(l.volume.bar.center()) {
                    Some(Hotspot::Volume(f)) => assert!((f - 0.5).abs() < 0.03),
                    other => panic!("volume bar centre hit {other:?}"),
                }
                match style {
                    Style::Disc => {
                        let (inner, outer) = l.ring_radii().unwrap();
                        let c = l.art_centre();
                        let on_ring = c + Point::new((inner + outer) / 2, 0);
                        match l.hit(on_ring) {
                            Some(Hotspot::Seek(f)) => {
                                assert!((f - 0.25).abs() < 0.02)
                            }
                            other => panic!("ring hit {other:?}"),
                        }
                        assert_eq!(l.hit(c), Some(Hotspot::Art));
                    }
                    Style::Card => match l.hit(l.seek.unwrap().center()) {
                        Some(Hotspot::Seek(f)) => {
                            assert!((f - 0.5).abs() < 0.03)
                        }
                        other => panic!("strip centre hit {other:?}"),
                    },
                }
            }
        }
    }

    #[test]
    fn the_face_repaints_only_what_moved() {
        for style in [Style::Disc, Style::Card] {
            let mut face = Face::new(0, style, theme::EVO_DEFAULT);
            let mut display =
                SimulatorDisplay::<Rgb565>::new(face.layout().frame);
            let view = View::new(&playing(), Some(&out()), "Card");
            // The first frame comes in stages, the last of them Full.
            for _ in 1..STAGES {
                assert_eq!(
                    face.refresh(&mut display, &view),
                    Ok(Repaint::Partial)
                );
            }
            assert_eq!(face.refresh(&mut display, &view), Ok(Repaint::Full));
            let lit = display
                .bounding_box()
                .points()
                .filter(|&p| {
                    display.get_pixel(p) != theme::EVO_DEFAULT.background
                })
                .count();
            assert!(
                lit > 5000,
                "{style:?}: something was painted: {lit} pixels"
            );
            assert_eq!(face.refresh(&mut display, &view), Ok(Repaint::None));
            let moved = View {
                position_ms: 120_500,
                ..view.clone()
            };
            assert_eq!(
                face.refresh(&mut display, &moved),
                Ok(Repaint::Progress)
            );
            let louder = View {
                volume: 55,
                ..moved.clone()
            };
            assert_eq!(
                face.refresh(&mut display, &louder),
                Ok(Repaint::Volume)
            );
            let muted = View {
                muted: true,
                ..louder.clone()
            };
            assert_eq!(face.refresh(&mut display, &muted), Ok(Repaint::Volume));
            let paused = View {
                status: Status::Paused,
                ..muted
            };
            // A status change alone repaints the transport, not the face.
            assert_eq!(
                face.refresh(&mut display, &paused),
                Ok(Repaint::Transport)
            );
            face.set_theme(theme::SUNRISE);
            assert_eq!(
                face.refresh_whole(&mut display, &paused),
                Ok(Repaint::Full)
            );
            assert_ne!(display.get_pixel(Point::new(2, 2)), Rgb565::BLACK);
        }
    }

    #[test]
    fn art_arrives_as_its_own_repaint_and_shows_in_both_styles() {
        for style in [Style::Disc, Style::Card] {
            let mut face = Face::new(0, style, theme::EVO_DEFAULT);
            let mut display =
                SimulatorDisplay::<Rgb565>::new(face.layout().frame);
            let view = View::new(&playing(), Some(&out()), "Card");
            face.refresh_whole(&mut display, &view).unwrap();
            let centre = face.layout().art.center();
            let before = display.get_pixel(centre);
            // A flat orange picture at the art's size.
            let size = face.layout().art.size.width;
            let orange = Rgb565::new(31, 40, 0);
            let art = art::fit(1, 1, &[0xFD00], size).unwrap();
            face.set_art(Some(art));
            assert_eq!(face.refresh(&mut display, &view), Ok(Repaint::Art));
            assert_eq!(display.get_pixel(centre), orange, "{style:?}");
            assert_ne!(before, orange);
            // The corner of the art square is not the art's: the disc's
            // and the card's corners are cut, and what was there stays
            // (the disc's ring passes through the square's corners).
            let corner = face.layout().art.top_left;
            assert_ne!(display.get_pixel(corner), orange, "{style:?}");
            if let Some((inner, outer)) = face.layout().ring_radii() {
                let c = face.layout().art_centre();
                let on_ring = (inner + outer) / 2;
                // 45 degrees up and left: inside the art square's corner.
                let d = (on_ring as f32 / core::f32::consts::SQRT_2) as i32;
                let p = Point::new(c.x - d, c.y - d);
                assert!(
                    face.layout().art.contains(p),
                    "the probe is in the square"
                );
                assert_eq!(
                    display.get_pixel(p),
                    theme::EVO_DEFAULT.border,
                    "the ring's track survives the art"
                );
            }
            assert_eq!(face.refresh(&mut display, &view), Ok(Repaint::None));
            // Taking it away repaints the stand-in.
            face.set_art(None);
            assert_eq!(face.refresh(&mut display, &view), Ok(Repaint::Art));
            assert_eq!(display.get_pixel(centre), before, "{style:?}");
            // Setting the same art again is not a change.
            face.set_art(None);
            assert_eq!(face.refresh(&mut display, &view), Ok(Repaint::None));
        }
    }

    #[test]
    fn a_scene_change_mid_paint_starts_the_frame_again() {
        let mut face = Face::new(0, Style::Disc, theme::EVO_DEFAULT);
        let mut display = SimulatorDisplay::<Rgb565>::new(face.layout().frame);
        let view = View::new(&playing(), Some(&out()), "Card");
        assert_eq!(face.refresh(&mut display, &view), Ok(Repaint::Partial));
        assert_eq!(face.refresh(&mut display, &view), Ok(Repaint::Partial));
        let other = View {
            title: "Another".into(),
            ..view
        };
        // The new scene paints from the ground up: STAGES calls again.
        for _ in 1..STAGES {
            assert_eq!(
                face.refresh(&mut display, &other),
                Ok(Repaint::Partial)
            );
        }
        assert_eq!(face.refresh(&mut display, &other), Ok(Repaint::Full));
        assert_eq!(face.refresh(&mut display, &other), Ok(Repaint::None));
    }

    #[test]
    fn text_fits_with_an_ellipsis() {
        let a = &text::TITLE;
        let f = a.fit("Money For Nothing", 60);
        assert!(f.ellipsis);
        assert!(a.width(f.head) + a.glyph('…').advance as i32 <= 60);
        assert_eq!(
            a.fit("Short", 200),
            text::Fitted {
                head: "Short",
                ellipsis: false
            }
        );
        assert_eq!(a.glyph('\u{1F600}').ch, '?');
    }
}

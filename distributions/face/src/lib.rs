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
//!
//! Three screens behind the navigation ([`Screen`]): Now Playing, the
//! Library (the source's folder in view, a row a track, a bar with the
//! track in hand at the foot) and Settings (what the player is). The
//! distribution owns what the list pages show ([`library`]) and what a
//! tap on them does; the face draws and reports the tap.

#![no_std]

extern crate alloc;

pub mod art;
pub mod draw;
pub mod layout;
pub mod library;
pub mod text;
pub mod theme;
pub mod touch;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

pub use art::Art;
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::{DrawTarget, Point, Size};
use embedded_graphics::primitives::Rectangle;
use evo_esp32_queue::Repeat;
use evo_esp32_sdk::contract::audio::{delivery, playback};
pub use layout::{Hotspot, Layout, Screen, Slider, Style};
pub use library::{About, Entry, Fact, Library};
use text::Align;
use theme::mix;
pub use theme::Theme;

/// Where playback stands, as the face tells it apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Status {
    /// Nothing in custody.
    #[default]
    Stopped,
    /// Being reached; nothing out yet.
    Opening,
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
    /// The track's URI, to tell it apart in a list; empty when nothing
    /// is in custody.
    pub uri: String,
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
    /// A word about the link the music depends on ("2.4 GHz Wi-Fi",
    /// "weak Wi-Fi"), shown on the status strip after the source and
    /// before the stream, so the strip's end is what a narrow layout
    /// cuts. Empty when there is nothing to say.
    pub caution: &'static str,
    /// The queue's order is shuffled.
    pub shuffle: bool,
    /// What the queue does at its end.
    pub repeat: Repeat,
    /// Why nothing is playing, when something was asked for and did not
    /// play ("Jazz24: not found (404)"); shown on the status strip in
    /// the format's place. Empty when there is nothing to say.
    pub notice: String,
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
                playback::Status::Opening => Status::Opening,
                playback::Status::Playing => Status::Playing,
                playback::Status::Paused => Status::Paused,
            },
            uri: track.map(|t| t.uri.clone()).unwrap_or_default(),
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
            caution: "",
            shuffle: false,
            repeat: Repeat::Off,
            notice: String::new(),
        }
    }

    /// Whether the queue's modes (shuffle, repeat) apply to what is in
    /// hand: they do to files, not to a station.
    pub fn modes(&self) -> bool {
        self.station().is_none()
    }

    /// The status strip: where the music is from, the caution about the
    /// link if there is one, and the stream, or the notice when there
    /// is one.
    pub fn status_line(&self) -> String {
        let detail = if self.notice.is_empty() {
            self.format_line()
        } else {
            self.notice.clone()
        };
        let mut line = self.source.to_string();
        for part in [self.caution, detail.as_str()] {
            if !part.is_empty() {
                line.push_str("  ·  ");
                line.push_str(part);
            }
        }
        line
    }

    /// The station's name when the track is a stream: its album line,
    /// which the playback plugin keeps for the station's name, else its
    /// title. None for a file.
    pub fn station(&self) -> Option<&str> {
        if !(self.uri.starts_with("http://")
            || self.uri.starts_with("https://"))
        {
            return None;
        }
        Some(if self.album.is_empty() {
            self.title.as_str()
        } else {
            self.album.as_str()
        })
    }

    /// Position over duration, 0 to 1, when both are known.
    pub fn progress(&self) -> Option<f32> {
        let d = self.duration_ms.filter(|&d| d > 0)?;
        Some((self.position_ms as f32 / d as f32).clamp(0.0, 1.0))
    }

    /// "FLAC 96 kHz 16 bit", or empty before the format is known.
    pub fn format_line(&self) -> String {
        if self.status == Status::Opening {
            return "connecting".to_string();
        }
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
    /// A list page's rows: scrolled, or the track in hand changed.
    Rows,
    /// A list page's now-playing bar.
    Bar,
    /// The details sheet, laid over Now Playing.
    Sheet,
    /// One stage of a full frame; call again soon for the next.
    Partial,
    /// The whole face: a scene change, or the first frame, complete.
    Full,
}

/// The scene: everything on the face but the position and the volume.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Scene {
    screen: Screen,
    /// Which listing (or facts) a list page shows; bumped on each set.
    page: u32,
    status: Status,
    shuffle: bool,
    repeat: Repeat,
    modes: bool,
    title: String,
    artist: String,
    album: String,
    format: String,
    duration_ms: Option<u64>,
    style: Style,
    theme: &'static str,
}

impl Scene {
    /// True when only the track in hand (its status, names, format)
    /// differs from `other`: the screen, the page, style and theme hold.
    fn same_but_track(&self, other: &Scene) -> bool {
        self.screen == other.screen
            && self.page == other.page
            && self.style == other.style
            && self.theme == other.theme
    }
}

/// Now Playing, repainted in place as the view moves.
pub struct Face {
    layout: Layout,
    theme: Theme,
    scene: Option<Scene>,
    second: Option<u64>,
    level: Option<(u8, bool)>,
    /// The mode icons are on the face (not for a station), so a tap on
    /// them counts.
    modes_shown: bool,
    /// The details sheet is up over Now Playing.
    sheet: bool,
    /// The sheet was raised since the frame was last painted.
    sheet_due: bool,
    /// How far round the ring the accent reaches.
    ring_to: f32,
    /// The next stage of a full paint in progress.
    stage: Option<u8>,
    /// The album's picture, when the distribution has one.
    art: Option<Art>,
    /// The art changed since it was last painted.
    art_due: bool,
    screen: Screen,
    library: Library,
    about: About,
    /// Bumped when the library or the facts are set.
    page: u32,
    /// The rows changed (a scroll) since they were last painted.
    rows_due: bool,
    /// Finger travel on the list not yet a whole row, pixels.
    scroll_rest: i32,
}

/// A full frame is painted in stages, one per `refresh`, so the caller's
/// loop (the decoder's tick, on a board) runs between them: no stage
/// holds the loop as long as a whole frame would.
const STAGES: u8 = 10;
/// A list page's stages: the ground, the header, the rows in two halves,
/// the bar, the navigation.
const PAGE_STAGES: u8 = 6;
/// The now-playing bar's thumbnail side.
const THUMB: u32 = 28;

/// The card's corner radius: evo-ui's `--radius`, 0.75 rem.
const RADIUS: i32 = 12;
/// The details sheet: its margin from the frame's sides, its padding,
/// and the gap between its parts.
const SHEET_MARGIN: i32 = 14;
const SHEET_PAD: i32 = 14;
const SHEET_GAP: i32 = 6;
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
            modes_shown: false,
            sheet: false,
            sheet_due: false,
            ring_to: 0.0,
            stage: None,
            art: None,
            art_due: false,
            screen: Screen::NowPlaying,
            library: Library::default(),
            about: About::default(),
            page: 0,
            rows_due: false,
            scroll_rest: 0,
        }
    }

    /// The screen in front.
    pub fn screen(&self) -> Screen {
        self.screen
    }

    /// Bring `screen` to the front; the next refresh paints it whole.
    pub fn set_screen(&mut self, screen: Screen) {
        self.screen = screen;
    }

    /// The library's folder in view, from its first row; the next refresh
    /// paints the page whole if it is in front.
    pub fn set_library(&mut self, library: Library) {
        self.library = library;
        self.page = self.page.wrapping_add(1);
        self.scroll_rest = 0;
    }

    /// The library as shown.
    pub fn library(&self) -> &Library {
        &self.library
    }

    /// Scroll the list page in front by `pixels` of finger travel
    /// (positive is the finger moving down, which brings earlier rows
    /// into view). Travel accumulates across calls: a drag comes in
    /// small steps, and the rows move once a whole row's worth is in.
    pub fn scroll(&mut self, pixels: i32) {
        let row = self.layout.page.row;
        self.scroll_rest += pixels;
        let rows = self.scroll_rest / row;
        if rows == 0 {
            return;
        }
        self.scroll_rest -= rows * row;
        let shown = self.layout.page.rows();
        let moved = match self.screen {
            Screen::Library => self.library.scroll(-rows, shown),
            Screen::Settings => self.about.scroll(-rows, shown),
            Screen::NowPlaying => false,
        };
        if moved {
            self.rows_due = true;
        }
    }

    /// The settings page's facts; the view stays where it was scrolled,
    /// within the new list.
    pub fn set_about(&mut self, mut about: About) {
        about.first = self
            .about
            .first
            .min(about.facts.len().saturating_sub(self.layout.page.rows()));
        self.about = about;
        self.page = self.page.wrapping_add(1);
    }

    /// The settings page as shown.
    pub fn about(&self) -> &About {
        &self.about
    }

    /// What is under `p` on the screen in front, for a touch.
    pub fn hit(&self, p: Point) -> Option<Hotspot> {
        if self.sheet && self.screen == Screen::NowPlaying {
            return Some(match self.layout.hit_nav(p) {
                Some(nav) => nav,
                None => Hotspot::Sheet,
            });
        }
        match self.layout.hit_on(self.screen, self.rows_filled(), p) {
            Some(Hotspot::Shuffle | Hotspot::Repeat) if !self.modes_shown => {
                None
            }
            hit => hit,
        }
    }

    /// How many rows the list page in front has filled.
    fn rows_filled(&self) -> usize {
        let shown = self.layout.page.rows();
        match self.screen {
            Screen::Library => self.library.visible(shown).len(),
            Screen::Settings => self.about.visible(shown).len(),
            Screen::NowPlaying => 0,
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
            screen: self.screen,
            page: self.page,
            status: view.status,
            shuffle: view.shuffle,
            repeat: view.repeat,
            modes: view.modes(),
            title: view.title.clone(),
            artist: view.artist.clone(),
            album: view.album.clone(),
            format: view.status_line(),
            duration_ms: view.duration_ms,
            style: self.layout.style,
            theme: self.theme.name,
        };
        let second = Some(view.position_ms / 1000);
        let level = Some((view.volume, view.muted));
        let on_page = self.screen != Screen::NowPlaying;
        if let Some(was) = self.scene.as_ref().filter(|was| **was != scene) {
            if was.same_but_track(&scene) {
                if on_page {
                    // The track in hand changed: the bar, and the rows
                    // for the highlight.
                    self.paint_now_bar(target, view)?;
                    self.paint_rows(target, view)?;
                    self.scene = Some(scene);
                    return Ok(Repaint::Bar);
                }
                // Only the transport's state moved (the status, the
                // modes): the buttons, not the face.
                let only_transport = Scene {
                    status: scene.status,
                    shuffle: scene.shuffle,
                    repeat: scene.repeat,
                    ..was.clone()
                } == scene;
                if only_transport {
                    self.modes_shown = view.modes();
                    self.paint_transport(target, view)?;
                    self.scene = Some(scene);
                    return Ok(Repaint::Transport);
                }
            }
        }
        if self.scene.as_ref() != Some(&scene) {
            self.modes_shown = scene.modes;
            self.scene = Some(scene);
            self.stage = Some(0);
            self.sheet = false;
            self.sheet_due = false;
        }
        let stages = if on_page { PAGE_STAGES } else { STAGES };
        if let Some(stage) = self.stage {
            if on_page {
                self.paint_page_stage(target, view, stage)?;
            } else {
                self.paint_stage(target, view, stage)?;
            }
            if stage + 1 < stages {
                self.stage = Some(stage + 1);
                return Ok(Repaint::Partial);
            }
            self.stage = None;
            self.art_due = false;
            self.rows_due = false;
            return Ok(Repaint::Full);
        }
        if self.art_due {
            self.art_due = false;
            if on_page {
                self.paint_now_bar(target, view)?;
                return Ok(Repaint::Bar);
            }
            self.paint_art(target, view)?;
            return Ok(Repaint::Art);
        }
        if self.rows_due {
            self.rows_due = false;
            if on_page {
                self.paint_rows(target, view)?;
                return Ok(Repaint::Rows);
            }
        }
        if self.sheet_due {
            self.sheet_due = false;
            self.paint_sheet(target, view)?;
            return Ok(Repaint::Sheet);
        }
        if self.sheet {
            // Nothing under the sheet moves until it is put down.
            return Ok(Repaint::None);
        }
        if on_page {
            // The position and the level are not on a list page; remember
            // them so a return to Now Playing starts from the truth.
            self.second = second;
            self.level = level;
            return Ok(Repaint::None);
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

    /// Raise the details sheet over Now Playing (the whole title, artist,
    /// album and format, wrapped), or put it down; the next refresh
    /// paints it, or repaints the face whole without it. A tap anywhere
    /// while it is up is [`Hotspot::Sheet`]; a track change puts it down.
    pub fn show_sheet(&mut self, up: bool) {
        if up == self.sheet {
            return;
        }
        self.sheet = up;
        if up {
            self.sheet_due = true;
        } else {
            self.scene = None;
        }
    }

    /// Whether the details sheet is up.
    pub fn sheet(&self) -> bool {
        self.sheet
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

    /// One stage of a list page: the ground, the header, the rows in two
    /// halves, the bar, the navigation.
    fn paint_page_stage<D>(
        &mut self,
        target: &mut D,
        view: &View,
        stage: u8,
    ) -> Result<(), D::Error>
    where
        D: DrawTarget<Color = Rgb565>,
    {
        let t = self.theme;
        match stage {
            0 => target.clear(t.background),
            1 => self.paint_header(target),
            2 => self.paint_rows_from(target, view, 0, 2),
            3 => self.paint_rows_from(target, view, 1, 2),
            4 => self.paint_now_bar(target, view),
            _ => self.paint_nav(target),
        }
    }

    /// The page's title and, below the root, the back chevron.
    fn paint_header<D>(&self, target: &mut D) -> Result<(), D::Error>
    where
        D: DrawTarget<Color = Rgb565>,
    {
        let (l, t) = (self.layout.page, self.theme);
        let (name, back) = match self.screen {
            Screen::Library => {
                (self.library.name.as_str(), !self.library.path.is_empty())
            }
            _ => ("Settings", false),
        };
        let chevron = Rectangle::new(
            Point::new(l.back.top_left.x + 10, l.header.top_left.y - 1),
            Size::new(16, 20),
        );
        text::TITLE.line(
            target,
            if back { "\u{2039}" } else { "" },
            chevron,
            Align::Centre,
            t.accent,
            t.background,
        )?;
        text::TITLE.line(
            target,
            name,
            l.header,
            Align::Left,
            t.foreground,
            t.background,
        )
    }

    /// Every row slot of the page in front: the entries in view, the
    /// rest cleared.
    fn paint_rows<D>(&self, target: &mut D, view: &View) -> Result<(), D::Error>
    where
        D: DrawTarget<Color = Rgb565>,
    {
        self.paint_rows_from(target, view, 0, 1)
    }

    /// Row slots `part` of `parts` (slot `i` belongs to part `i % parts`).
    fn paint_rows_from<D>(
        &self,
        target: &mut D,
        view: &View,
        part: usize,
        parts: usize,
    ) -> Result<(), D::Error>
    where
        D: DrawTarget<Color = Rgb565>,
    {
        let (l, t) = (self.layout.page, self.theme);
        let shown = l.rows();
        let hairline = mix(t.background, t.border, 8);
        for i in (0..shown).filter(|i| i % parts == part) {
            let slot = l.row_rect(i);
            match self.screen {
                Screen::Library => match self.library.visible(shown).get(i) {
                    Some(entry) => {
                        let playing = entry.uri.is_some()
                            && entry.uri.as_deref() == Some(view.uri.as_str())
                            && view.status != Status::Stopped;
                        self.paint_entry(target, slot, entry, playing)?;
                    }
                    None => target.fill_solid(&slot, t.background)?,
                },
                _ => match self.about.visible(shown).get(i) {
                    Some(fact) => self.paint_fact(target, slot, fact)?,
                    None => target.fill_solid(&slot, t.background)?,
                },
            }
            let foot = Rectangle::new(
                Point::new(slot.top_left.x + 12, slot.top_left.y + l.row - 1),
                Size::new(slot.size.width.saturating_sub(24), 1),
            );
            target.fill_solid(&foot, hairline)?;
        }
        Ok(())
    }

    /// A library row: a folder or a note, then the name; the track in
    /// hand in the accent.
    fn paint_entry<D>(
        &self,
        target: &mut D,
        slot: Rectangle,
        entry: &Entry,
        playing: bool,
    ) -> Result<(), D::Error>
    where
        D: DrawTarget<Color = Rgb565>,
    {
        let t = self.theme;
        let (x, y) = (slot.top_left.x, slot.top_left.y);
        let h = slot.size.height as i32;
        let glyph =
            Rectangle::new(Point::new(x + 10, y + 7), Size::new(16, 14));
        target.fill_solid(&glyph, t.background)?;
        let c = glyph.center();
        if entry.folder {
            // A folder: a tab over a body.
            let tab =
                Rectangle::new(Point::new(c.x - 7, c.y - 6), Size::new(6, 3));
            let body =
                Rectangle::new(Point::new(c.x - 7, c.y - 4), Size::new(14, 10));
            draw::rounded(target, tab, 1, t.muted_fg, t.background)?;
            draw::rounded(target, body, 2, t.muted_fg, t.background)?;
        } else {
            let colour = if playing {
                t.accent
            } else if entry.playable {
                t.muted_fg
            } else {
                t.border
            };
            draw::icon_note(target, c, 6, colour, t.background)?;
        }
        let name = Rectangle::new(
            Point::new(x + 34, y + (h - 15) / 2),
            Size::new(slot.size.width.saturating_sub(34 + 10), 15),
        );
        let colour = if playing {
            t.accent
        } else if entry.playable {
            t.foreground
        } else {
            mix(t.muted_fg, t.background, 6)
        };
        text::BODY.line(
            target,
            &entry.name,
            name,
            Align::Left,
            colour,
            t.background,
        )
    }

    /// A settings row: the fact's name at the left, its value at the
    /// right.
    fn paint_fact<D>(
        &self,
        target: &mut D,
        slot: Rectangle,
        fact: &Fact,
    ) -> Result<(), D::Error>
    where
        D: DrawTarget<Color = Rgb565>,
    {
        let t = self.theme;
        let (x, y) = (slot.top_left.x, slot.top_left.y);
        let (w, h) = (slot.size.width as i32, slot.size.height as i32);
        let name_w = (w - 24) * 2 / 5;
        let name = Rectangle::new(
            Point::new(x + 12, y + (h - 13) / 2),
            Size::new(name_w as u32, 13),
        );
        let value = Rectangle::new(
            Point::new(x + 12 + name_w + 6, y + (h - 13) / 2),
            Size::new((w - 24 - name_w - 6).max(0) as u32, 13),
        );
        text::SMALL.line(
            target,
            &fact.name,
            name,
            Align::Left,
            t.muted_fg,
            t.background,
        )?;
        text::SMALL.line(
            target,
            &fact.value,
            value,
            Align::Right,
            t.foreground,
            t.background,
        )
    }

    /// The now-playing bar at a page's foot: the art's thumbnail (or a
    /// small disc), the title, play or pause.
    fn paint_now_bar<D>(
        &self,
        target: &mut D,
        view: &View,
    ) -> Result<(), D::Error>
    where
        D: DrawTarget<Color = Rgb565>,
    {
        let (l, t) = (self.layout.page, self.theme);
        target.fill_solid(&l.bar, t.card)?;
        let top =
            Rectangle::new(l.bar.top_left, Size::new(l.bar.size.width, 1));
        target.fill_solid(&top, t.border)?;
        let thumb = self
            .art
            .as_ref()
            .and_then(|a| art::fit(a.size, a.size, &a.pixels, THUMB));
        match thumb {
            Some(thumb) => {
                draw::art_card(target, l.bar_art, 4, &thumb, t.card, None)?
            }
            None => {
                let c = l.bar_art.center();
                let r = l.bar_art.size.width as i32 / 2;
                draw::disc(target, c, r, t.secondary, t.card)?;
                draw::icon_note(target, c, r * 5 / 8, t.muted_fg, t.secondary)?;
            }
        }
        self.paint_bar_title(target, view)?;
        let (c, r) = (l.bar_play, l.bar_play_r as i32);
        draw::disc(target, c, r, t.accent, t.card)?;
        match view.status {
            Status::Playing => draw::icon_pause(target, c, r / 2, t.accent_fg),
            _ => draw::icon_play(target, c, r / 2, t.accent_fg, t.accent),
        }
    }

    /// The bar's title: the track's, cut with an ellipsis when too long.
    fn paint_bar_title<D>(
        &self,
        target: &mut D,
        view: &View,
    ) -> Result<(), D::Error>
    where
        D: DrawTarget<Color = Rgb565>,
    {
        let (l, t) = (self.layout.page, self.theme);
        let (title, colour) = if view.title.is_empty() {
            ("Nothing playing", t.muted_fg)
        } else {
            (view.title.as_str(), t.foreground)
        };
        text::BODY.line(target, title, l.bar_title, Align::Left, colour, t.card)
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
            1 => text::TINY.line(
                target,
                &view.status_line(),
                l.status,
                Align::Left,
                t.muted_fg,
                t.background,
            ),
            2 => self.paint_art(target, view),
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
    /// stand-in. Where the card's text band lies over the art's foot the
    /// picture shows through a scrim, and the text lines are painted
    /// with the art since they sit on it.
    fn paint_art<D>(
        &mut self,
        target: &mut D,
        view: &View,
    ) -> Result<(), D::Error>
    where
        D: DrawTarget<Color = Rgb565>,
    {
        let l = self.layout;
        let t = self.theme;
        match (l.style, &self.art) {
            (Style::Disc, Some(art)) => {
                draw::art_disc(target, l.art, art, t.background)
            }
            (Style::Disc, None) => match view.station() {
                Some(name) => draw::station_disc(target, l.art, name, &t),
                None => draw::placeholder_disc(target, l.art, &t),
            },
            (Style::Card, art) => {
                let band = self.card_band();
                match art {
                    Some(art) => draw::art_card(
                        target,
                        l.art,
                        RADIUS,
                        art,
                        t.background,
                        band.map(|(band, colour)| (band.top_left.y, colour)),
                    )?,
                    None => {
                        match view.station() {
                            Some(name) => {
                                let free =
                                    band.map_or(l.art.size.height, |(b, _)| {
                                        (b.top_left.y - l.art.top_left.y) as u32
                                    });
                                draw::station_card(
                                    target, l.art, RADIUS, free, name, &t,
                                )?;
                            }
                            None => draw::placeholder_card(
                                target, l.art, RADIUS, &t,
                            )?,
                        }
                        if let Some((band, colour)) = band {
                            draw::rounded_corners(
                                target,
                                band,
                                RADIUS,
                                colour,
                                t.background,
                                false,
                                true,
                            )?;
                        }
                    }
                }
                if band.is_some() {
                    for n in 0..3 {
                        self.paint_text_line(target, view, n)?;
                    }
                }
                Ok(())
            }
        }
    }

    /// The colour under a point of the card's text band: the band's,
    /// or the art seen through the scrim.
    fn band_ground(&self, band_top: i32, colour: Rgb565, p: Point) -> Rgb565 {
        let art_origin = self.layout.art.top_left;
        match &self.art {
            Some(art) => draw::scrimmed(
                colour,
                art.at(p.x - art_origin.x, p.y - art_origin.y)
                    .unwrap_or(colour),
                p.y - band_top,
            ),
            None => colour,
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

    /// Text line `n`: 0 the title, 1 the artist, 2 the album; a line too
    /// long for its area is cut with an ellipsis, and read whole on the
    /// details sheet.
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
        let (font, line, area, colour) = match n {
            0 => (&text::TITLE, title, l.title, t.foreground),
            1 => (artist_font, artist, l.artist, t.muted_fg),
            _ => (album_font, album, l.album, mix(t.muted_fg, under, 5)),
        };
        match band {
            Some((band, band_colour)) if self.art.is_some() => {
                font.line_over(target, line, area, align, colour, |p| {
                    self.band_ground(band.top_left.y, band_colour, p)
                })
            }
            _ => font.line(target, line, area, align, colour, under),
        }
    }

    /// The details sheet: a card over the middle of Now Playing with the
    /// title, artist, album (or station) and format whole, each wrapped
    /// to the card's width; a line too long for its place on the face
    /// is read here.
    fn paint_sheet<D>(
        &self,
        target: &mut D,
        view: &View,
    ) -> Result<(), D::Error>
    where
        D: DrawTarget<Color = Rgb565>,
    {
        let l = self.layout;
        let t = self.theme;
        // The room: the frame less the navigation, with a margin.
        let room = match l.nav_axis {
            layout::NavAxis::Bar => Rectangle::new(
                Point::zero(),
                Size::new(l.frame.width, l.nav.top_left.y as u32),
            ),
            layout::NavAxis::Rail => Rectangle::new(
                Point::new(l.nav.top_left.x + l.nav.size.width as i32, 0),
                Size::new(l.frame.width - l.nav.size.width, l.frame.height),
            ),
        };
        let width = room.size.width as i32 - 2 * SHEET_MARGIN;
        let inner = width - 2 * SHEET_PAD;
        // The lines, top to bottom: (font, text, colour, lines allowed).
        let (title, artist, album) = if view.title.is_empty() {
            ("Nothing playing", "", "")
        } else {
            (
                view.title.as_str(),
                view.artist.as_str(),
                view.album.as_str(),
            )
        };
        let format = view.format_line();
        let parts: [(&text::Atlas, &str, Rgb565, usize); 4] = [
            (&text::HEADING, title, t.foreground, 4),
            (&text::BODY, artist, t.foreground, 2),
            (&text::BODY, album, t.muted_fg, 3),
            (&text::SMALL, &format, t.muted_fg, 1),
        ];
        // Each part's lines; a gap between parts.
        let mut blocks: Vec<(&text::Atlas, Vec<String>, Rgb565)> = Vec::new();
        let mut height = 0;
        for (font, text, colour, max) in parts.iter() {
            if text.is_empty() {
                continue;
            }
            let lines: Vec<String> = font
                .wrap(text, inner, *max)
                .into_iter()
                .map(|line| {
                    if line.ellipsis {
                        format!("{}…", line.head)
                    } else {
                        line.head.to_string()
                    }
                })
                .collect();
            if !blocks.is_empty() {
                height += SHEET_GAP;
            }
            height += lines.len() as i32 * font.line as i32;
            blocks.push((font, lines, *colour));
        }
        let card_h = (height + 2 * SHEET_PAD).min(room.size.height as i32);
        let card = Rectangle::new(
            Point::new(
                room.top_left.x + (room.size.width as i32 - width) / 2,
                room.top_left.y + (room.size.height as i32 - card_h) / 2,
            ),
            Size::new(width as u32, card_h as u32),
        );
        // A hairline of the border round the card, then the card.
        let rim = Rectangle::new(
            card.top_left - Point::new(1, 1),
            card.size + Size::new(2, 2),
        );
        draw::rounded(target, rim, RADIUS + 1, t.border, t.background)?;
        draw::rounded(target, card, RADIUS, t.card, t.border)?;
        let mut y = card.top_left.y + SHEET_PAD;
        let bottom = card.top_left.y + card_h - SHEET_PAD;
        for (i, (font, lines, colour)) in blocks.iter().enumerate() {
            if i > 0 {
                y += SHEET_GAP;
            }
            let step = font.line as i32;
            for shown in lines {
                if y + step > bottom {
                    return Ok(());
                }
                let area = Rectangle::new(
                    Point::new(card.top_left.x + SHEET_PAD, y),
                    Size::new(inner as u32, step as u32),
                );
                font.line(target, shown, area, Align::Centre, *colour, t.card)?;
                y += step;
            }
        }
        Ok(())
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
            let active = i == usize::from(self.screen.nav_index());
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
        // The modes: flat icons, lit when on; cleared for a station,
        // where they mean nothing.
        let r = tr.mode_r as i32;
        if view.modes() {
            let lit = |on: bool| if on { t.accent } else { t.muted_fg };
            draw::icon_shuffle(
                target,
                tr.shuffle,
                r,
                lit(view.shuffle),
                t.background,
            )?;
            draw::icon_repeat(
                target,
                tr.repeat,
                r,
                view.repeat == Repeat::One,
                lit(view.repeat != Repeat::Off),
                t.background,
            )?;
        } else {
            for c in [tr.shuffle, tr.repeat] {
                target.fill_solid(
                    &Rectangle::new(
                        c - Point::new(r, r),
                        Size::new(2 * r as u32 + 1, 2 * r as u32 + 1),
                    ),
                    t.background,
                )?;
            }
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
                sample_rate: None,
                art: None,
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
        // The strip: source, the caution about the link, the stream; a
        // narrow layout cuts from the end, so the caution comes first.
        let mut v = View::new(&playing(), Some(&out()), "Snapcast");
        assert_eq!(v.status_line(), "Snapcast  ·  FLAC 96 kHz 16 bit");
        v.caution = "2.4 GHz Wi-Fi";
        assert_eq!(
            v.status_line(),
            "Snapcast  ·  2.4 GHz Wi-Fi  ·  FLAC 96 kHz 16 bit"
        );
        v.notice = "the stream ended".into();
        assert_eq!(
            v.status_line(),
            "Snapcast  ·  2.4 GHz Wi-Fi  ·  the stream ended"
        );
        assert_eq!(stopped.status_line(), "Card");
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
    fn the_card_shows_its_art_through_the_text_band() {
        let mut face = Face::new(0, Style::Card, theme::EVO_DEFAULT);
        let mut display = SimulatorDisplay::<Rgb565>::new(face.layout().frame);
        let view = View::new(&playing(), Some(&out()), "Card");
        face.refresh_whole(&mut display, &view).unwrap();
        let (band, band_colour) = face.card_band().expect("portrait card");
        let title = face.layout().title;
        let fg = theme::EVO_DEFAULT.foreground;
        let count_fg = |display: &SimulatorDisplay<Rgb565>| {
            title
                .points()
                .filter(|&p| display.get_pixel(p) == fg)
                .count()
        };
        let glyphs_before = count_fg(&display);
        assert!(glyphs_before > 0, "the title is on the band");
        // Beside the text, in the band, the stand-in is under the band.
        let beside = Point::new(band.top_left.x + 6, title.center().y);
        assert_eq!(display.get_pixel(beside), band_colour);
        let orange = Rgb565::new(31, 40, 0);
        let art =
            art::fit(1, 1, &[0xFD00], face.layout().art.size.width).unwrap();
        face.set_art(Some(art));
        assert_eq!(face.refresh(&mut display, &view), Ok(Repaint::Art));
        // The art shows through, dimmed: neither the band nor the picture.
        let seen = display.get_pixel(beside);
        assert_ne!(seen, band_colour);
        assert_ne!(seen, orange);
        assert_eq!(seen, draw::scrimmed(band_colour, orange, 100));
        // Above the band the picture is itself; the text survived.
        assert_eq!(
            display.get_pixel(Point::new(beside.x, band.top_left.y - 2)),
            orange
        );
        assert_eq!(count_fg(&display), glyphs_before, "the title is repainted");
    }

    #[test]
    fn the_library_page_paints_in_stages_and_repaints_rows_and_bar() {
        use alloc::string::ToString;
        let mut face = Face::new(0, Style::Disc, theme::EVO_DEFAULT);
        let mut display = SimulatorDisplay::<Rgb565>::new(face.layout().frame);
        let view = View::new(&playing(), Some(&out()), "Card");
        face.refresh_whole(&mut display, &view).unwrap();
        let entries: Vec<Entry> = (0..12)
            .map(|i| Entry {
                name: alloc::format!("Track {i}"),
                folder: i == 0,
                uri: (i > 0).then(|| alloc::format!("file:///t{i}.flac")),
                playable: true,
            })
            .collect();
        face.set_screen(Screen::Library);
        face.set_library(Library {
            path: "Album".to_string(),
            name: "Album".to_string(),
            entries,
            first: 0,
        });
        for _ in 1..PAGE_STAGES {
            assert_eq!(face.refresh(&mut display, &view), Ok(Repaint::Partial));
        }
        assert_eq!(face.refresh(&mut display, &view), Ok(Repaint::Full));
        assert_eq!(face.refresh(&mut display, &view), Ok(Repaint::None));
        let page = face.layout().page;
        assert_eq!(face.rows_filled(), page.rows());
        // A touch lands on rows, the bar's button and the nav.
        assert_eq!(face.hit(page.row_rect(1).center()), Some(Hotspot::Row(1)));
        assert_eq!(face.hit(page.bar_play), Some(Hotspot::BarPlay));
        assert_eq!(face.hit(face.layout().nav_item(0)), Some(Hotspot::Nav(0)));
        assert_eq!(face.hit(page.back.center()), Some(Hotspot::Back));
        // A drag comes in steps; the rows move once a row's worth is in,
        // and the repaint is the rows alone.
        face.scroll(-10);
        face.scroll(-10);
        assert_eq!(face.library().first, 0);
        assert_eq!(face.refresh(&mut display, &view), Ok(Repaint::None));
        face.scroll(-10);
        assert_eq!(face.library().first, 1);
        assert_eq!(face.refresh(&mut display, &view), Ok(Repaint::Rows));
        face.scroll(-page.row);
        assert_eq!(face.library().first, 2);
        assert_eq!(face.refresh(&mut display, &view), Ok(Repaint::Rows));
        // The position moving is nothing here.
        let moved = View {
            position_ms: 125_000,
            ..view.clone()
        };
        assert_eq!(face.refresh(&mut display, &moved), Ok(Repaint::None));
        // Another track in hand: the bar and the rows.
        let other = View {
            uri: "file:///t3.flac".to_string(),
            title: "Track 3".to_string(),
            ..moved
        };
        assert_eq!(face.refresh(&mut display, &other), Ok(Repaint::Bar));
        // Back to Now Playing: a whole frame in its ten stages.
        face.set_screen(Screen::NowPlaying);
        for _ in 1..STAGES {
            assert_eq!(
                face.refresh(&mut display, &other),
                Ok(Repaint::Partial)
            );
        }
        assert_eq!(face.refresh(&mut display, &other), Ok(Repaint::Full));
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

    #[test]
    fn names_wrap_to_whole_words() {
        let a = &text::HEADING;
        let w = a.width("France Musique");
        let lines = a.wrap("France Musique Classique Plus", w, 3);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].head, "France Musique");
        assert_eq!(lines[1].head, "Classique Plus");
        assert!(!lines[1].ellipsis);
        let two =
            a.wrap("One Two Three Four Five Six Seven", a.width("One Two"), 2);
        assert_eq!(two.len(), 2);
        assert!(two[1].ellipsis, "the last line is cut, not dropped");
        let long = a.wrap("Supercalifragilistic", a.width("Super"), 3);
        assert_eq!(
            long.len(),
            1,
            "a word wider than the line is cut on its own"
        );
        assert!(long[0].ellipsis);
        assert!(a.wrap("", 100, 3).is_empty());
    }

    #[test]
    fn the_mode_buttons_toggle_for_files_and_vanish_for_a_station() {
        for rotation in [0u16, 90] {
            let mut face = Face::new(rotation, Style::Card, theme::EVO_DEFAULT);
            let mut display =
                SimulatorDisplay::<Rgb565>::new(face.layout().frame);
            let mut view = View::new(&playing(), None, "Card");
            assert_eq!(
                face.refresh_whole(&mut display, &view),
                Ok(Repaint::Full)
            );
            let tr = face.layout().transport;
            assert_eq!(face.hit(tr.shuffle), Some(Hotspot::Shuffle));
            assert_eq!(face.hit(tr.repeat), Some(Hotspot::Repeat));
            view.shuffle = true;
            assert_eq!(
                face.refresh(&mut display, &view),
                Ok(Repaint::Transport),
                "a mode change repaints the buttons alone"
            );
            view.repeat = Repeat::One;
            assert_eq!(
                face.refresh(&mut display, &view),
                Ok(Repaint::Transport)
            );
            assert_eq!(face.refresh(&mut display, &view), Ok(Repaint::None));
            view.uri = "http://stream.example/live".to_string();
            assert!(!view.modes());
            assert_eq!(
                face.refresh_whole(&mut display, &view),
                Ok(Repaint::Full)
            );
            assert_eq!(face.hit(tr.shuffle), None, "no modes on a station");
            assert_eq!(face.hit(tr.repeat), None);
        }
    }

    #[test]
    fn a_station_shows_its_name_and_the_sheet_reads_a_long_title() {
        let mut face = Face::new(0, Style::Disc, theme::EVO_DEFAULT);
        let mut display = SimulatorDisplay::<Rgb565>::new(face.layout().frame);
        let mut view = View::new(&playing(), None, "Card");
        view.uri = "http://stream.example/live".to_string();
        view.album = "Radio Paradise".to_string();
        view.title =
            "A title much too long to fit the width of the panel in one line"
                .to_string();
        assert_eq!(face.refresh_whole(&mut display, &view), Ok(Repaint::Full));
        assert_eq!(view.station(), Some("Radio Paradise"));
        assert_eq!(face.refresh(&mut display, &view), Ok(Repaint::None));
        // A tap on the text raises the sheet; the next refresh paints it
        // and nothing under it moves; a tap anywhere puts it down.
        let on_text = face.layout().title.center();
        assert_eq!(face.hit(on_text), Some(Hotspot::Text));
        face.show_sheet(true);
        assert_eq!(face.refresh(&mut display, &view), Ok(Repaint::Sheet));
        view.position_ms += 5000;
        assert_eq!(face.refresh(&mut display, &view), Ok(Repaint::None));
        assert_eq!(face.hit(face.layout().art.center()), Some(Hotspot::Sheet));
        assert!(matches!(
            face.hit(face.layout().nav_item(1)),
            Some(Hotspot::Nav(1))
        ));
        face.show_sheet(false);
        assert_eq!(face.refresh_whole(&mut display, &view), Ok(Repaint::Full));
        // A track change while it is up puts it down.
        face.show_sheet(true);
        assert_eq!(face.refresh(&mut display, &view), Ok(Repaint::Sheet));
        view.title = "Another".to_string();
        assert_eq!(face.refresh_whole(&mut display, &view), Ok(Repaint::Full));
        assert!(!face.sheet());
        view.uri = "file:///music/a.flac".to_string();
        assert_eq!(view.station(), None);
    }
}

// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Where things go, for a rotation and a style.
//!
//! The display driver turns the pixels; the face only needs to know
//! whether the frame is tall or wide. 0 and 180 share the portrait
//! layout, 90 and 270 the landscape one, so every rotation is served
//! without a layout of its own.
//!
//! In the disc style the progress is on the ring round the art and the
//! ring is the seek control, so the strip under the text is free to be
//! the volume. In the card style the strip seeks and the volume has a
//! row of its own.

use embedded_graphics::prelude::{Point, Size};
use embedded_graphics::primitives::Rectangle;

/// How Now Playing shows the art.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Style {
    /// Art in a circle with the progress on a ring around it.
    #[default]
    Disc,
    /// Art as a rounded square with the text over its foot.
    Card,
}

impl Style {
    /// The style named `name`, if there is one.
    pub fn named(name: &str) -> Option<Style> {
        match name {
            "disc" => Some(Style::Disc),
            "card" => Some(Style::Card),
            _ => None,
        }
    }

    /// The name [`Style::named`] accepts.
    pub fn name(self) -> &'static str {
        match self {
            Style::Disc => "disc",
            Style::Card => "card",
        }
    }
}

/// The pages the navigation switches between.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Screen {
    /// The track in hand.
    #[default]
    NowPlaying,
    /// The source's folders and tracks.
    Library,
    /// What this player is.
    Settings,
}

impl Screen {
    /// The navigation item that stands for the screen.
    pub fn nav_index(self) -> u8 {
        match self {
            Screen::NowPlaying => 0,
            Screen::Library => 1,
            Screen::Settings => 2,
        }
    }

    /// The screen the `i`th navigation item stands for.
    pub fn from_nav(i: u8) -> Screen {
        match i {
            1 => Screen::Library,
            2 => Screen::Settings,
            _ => Screen::NowPlaying,
        }
    }
}

/// A list page (the library, the settings): a header with a back
/// chevron, rows, and at the foot a bar with the track in hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Page {
    /// The header's text area.
    pub header: Rectangle,
    /// Where the back chevron sits and is hit.
    pub back: Rectangle,
    /// The rows' area.
    pub list: Rectangle,
    /// One row's height.
    pub row: i32,
    /// The now-playing bar.
    pub bar: Rectangle,
    /// Its thumbnail square.
    pub bar_art: Rectangle,
    /// Its title line.
    pub bar_title: Rectangle,
    /// Its play/pause button's centre and radius.
    pub bar_play: Point,
    /// The button's radius.
    pub bar_play_r: u32,
}

impl Page {
    /// How many whole rows the list shows.
    pub fn rows(&self) -> usize {
        (self.list.size.height as i32 / self.row).max(0) as usize
    }

    /// The `i`th visible row's rectangle.
    pub fn row_rect(&self, i: usize) -> Rectangle {
        Rectangle::new(
            self.list.top_left + Point::new(0, self.row * i as i32),
            Size::new(self.list.size.width, self.row as u32),
        )
    }
}

/// Which way the navigation runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavAxis {
    /// A bar along the bottom (portrait).
    Bar,
    /// A rail down the left (landscape).
    Rail,
}

/// The transport's three buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transport {
    /// Play/pause, the big one.
    pub play: Point,
    /// Its radius.
    pub play_r: u32,
    /// Previous.
    pub prev: Point,
    /// Next.
    pub next: Point,
    /// The small buttons' radius.
    pub skip_r: u32,
    /// Shuffle, a flat icon outside previous.
    pub shuffle: Point,
    /// Repeat, a flat icon outside next.
    pub repeat: Point,
    /// The mode icons' half-size.
    pub mode_r: u32,
}

/// A slider: an icon, a bar with a knob, a label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slider {
    /// The bar's full extent, a few pixels tall.
    pub bar: Rectangle,
    /// Where the icon is centred, if the slider has one.
    pub icon: Option<Point>,
    /// The label's area, if the slider has one.
    pub label: Option<Rectangle>,
}

/// The regions of Now Playing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    /// The panel, after rotation.
    pub frame: Size,
    /// Degrees, as given.
    pub rotation: u16,
    /// The style laid out.
    pub style: Style,
    /// Source and format.
    pub status: Rectangle,
    /// The art's square (the disc is inscribed in it).
    pub art: Rectangle,
    /// Title line.
    pub title: Rectangle,
    /// Artist line.
    pub artist: Rectangle,
    /// Album line.
    pub album: Rectangle,
    /// Elapsed time.
    pub elapsed: Rectangle,
    /// Duration.
    pub duration: Rectangle,
    /// The seek strip; none when the ring seeks.
    pub seek: Option<Rectangle>,
    /// The volume slider.
    pub volume: Slider,
    /// The buttons.
    pub transport: Transport,
    /// The navigation's area.
    pub nav: Rectangle,
    /// Bar or rail.
    pub nav_axis: NavAxis,
    /// The list pages' regions.
    pub page: Page,
}

/// Height of the bottom bar, width of the side rail.
pub const NAV: u32 = 34;
/// The progress ring's clearance from the art and its stroke.
pub const RING_GAP: i32 = 6;
/// The ring's stroke.
pub const RING: i32 = 4;

impl Layout {
    /// The layout for `rotation` degrees (0, 90, 180, 270) in `style`.
    pub fn new(rotation: u16, style: Style) -> Layout {
        match rotation % 180 {
            0 => Self::portrait(rotation, style),
            _ => Self::landscape(rotation, style),
        }
    }

    fn portrait(rotation: u16, style: Style) -> Layout {
        let (w, h) = (240i32, 320i32);
        let r = |x: i32, y: i32, cw: i32, ch: i32| {
            Rectangle::new(Point::new(x, y), Size::new(cw as u32, ch as u32))
        };
        // Vertical budget, top down: status, art (the disc's ring reaches
        // 10 px past the art), three text lines, the times (at the ring's
        // foot for the disc, on the seek row for the card), the volume
        // row, the transport, the nav bar. Nothing overlaps; the test
        // checks.
        let common =
            |art, title, artist, album, elapsed, duration, seek| Layout {
                frame: Size::new(w as u32, h as u32),
                rotation,
                style,
                status: r(8, 3, w - 16, 14),
                art,
                title,
                artist,
                album,
                elapsed,
                duration,
                seek,
                volume: Slider {
                    bar: r(40, 237, 150, 4),
                    icon: Some(Point::new(22, 239)),
                    label: Some(r(196, 232, 36, 13)),
                },
                transport: Transport {
                    play: Point::new(w / 2, 266),
                    play_r: 18,
                    prev: Point::new(w / 2 - 56, 266),
                    next: Point::new(w / 2 + 56, 266),
                    skip_r: 14,
                    shuffle: Point::new(w / 2 - 100, 266),
                    repeat: Point::new(w / 2 + 100, 266),
                    mode_r: 9,
                },
                nav: r(0, h - NAV as i32, w, NAV as i32),
                nav_axis: NavAxis::Bar,
                page: Self::page(r(0, 0, w, h - NAV as i32)),
            };
        match style {
            Style::Disc => common(
                r(50, 28, 140, 140),
                r(12, 180, w - 24, 18),
                r(12, 199, w - 24, 15),
                r(12, 214, w - 24, 13),
                r(8, 158, 50, 13),
                r(w - 8 - 50, 158, 50, 13),
                None,
            ),
            Style::Card => common(
                r(30, 22, 180, 180),
                r(42, 156, 156, 18),
                r(42, 175, 156, 14),
                r(42, 189, 156, 12),
                r(16, 212, 36, 13),
                r(w - 16 - 36, 212, 36, 13),
                Some(r(60, 217, w - 120, 4)),
            ),
        }
    }

    fn landscape(rotation: u16, style: Style) -> Layout {
        let (w, h) = (320i32, 240i32);
        let r = |x: i32, y: i32, cw: i32, ch: i32| {
            Rectangle::new(Point::new(x, y), Size::new(cw as u32, ch as u32))
        };
        // The rail, the art (ring to 10 px past it), then a text column:
        // status, three lines, the times, the transport. Under the art
        // the rows run the width: the disc's volume; the card's volume
        // and, below it, the seek with the times at its ends.
        let tx = 212;
        let col = w - tx - 10;
        let right = w - 10;
        let (seek, volume) = match style {
            Style::Disc => (
                None,
                Slider {
                    bar: r(86, 192, right - 36 - 8 - 86, 4),
                    icon: Some(Point::new(66, 194)),
                    label: Some(r(right - 36, 187, 36, 13)),
                },
            ),
            Style::Card => (
                Some(r(90, 210, right - 36 - 8 - 90, 4)),
                Slider {
                    bar: r(86, 182, right - 36 - 8 - 86, 4),
                    icon: Some(Point::new(66, 184)),
                    label: Some(r(right - 36, 177, 36, 13)),
                },
            ),
        };
        let (elapsed, duration) = match style {
            Style::Disc => (r(tx, 88, 40, 12), r(right - 40, 88, 40, 12)),
            Style::Card => (r(46, 205, 36, 13), r(right - 36, 205, 36, 13)),
        };
        Layout {
            frame: Size::new(w as u32, h as u32),
            rotation,
            style,
            status: r(tx, 14, col, 12),
            art: r(46, 20, 140, 140),
            title: r(tx, 34, col, 18),
            artist: r(tx, 55, col, 15),
            album: r(tx, 72, col, 13),
            elapsed,
            duration,
            seek,
            volume,
            // The column has no room beside next: the modes sit on a row
            // of their own under the transport.
            transport: Transport {
                play: Point::new(262, 128),
                play_r: 20,
                prev: Point::new(262 - 40, 128),
                next: Point::new(262 + 40, 128),
                skip_r: 13,
                shuffle: Point::new(262 - 20, 162),
                repeat: Point::new(262 + 20, 162),
                mode_r: 8,
            },
            nav: r(0, 0, NAV as i32, h),
            nav_axis: NavAxis::Rail,
            page: Self::page(r(NAV as i32, 0, w - NAV as i32, h)),
        }
    }

    /// A list page inside `area` (the frame less the navigation): the
    /// header, as many rows as fit, the now-playing bar at the foot.
    fn page(area: Rectangle) -> Page {
        let (x0, y0) = (area.top_left.x, area.top_left.y);
        let (w, h) = (area.size.width as i32, area.size.height as i32);
        let r = |x: i32, y: i32, cw: i32, ch: i32| {
            Rectangle::new(Point::new(x, y), Size::new(cw as u32, ch as u32))
        };
        let header_h = 30;
        let bar_h = 44;
        let row = 28;
        let list_h = (h - header_h - bar_h) / row * row;
        let bar_y = y0 + h - bar_h;
        Page {
            header: r(x0 + 34, y0 + 7, w - 34 - 10, 18),
            back: r(x0, y0, 34, header_h),
            list: r(x0, y0 + header_h, w, list_h),
            row,
            bar: r(x0, bar_y, w, bar_h),
            bar_art: r(x0 + 8, bar_y + 8, 28, 28),
            bar_title: r(x0 + 44, bar_y + 14, w - 44 - 52, 16),
            bar_play: Point::new(x0 + w - 26, bar_y + bar_h / 2),
            bar_play_r: 14,
        }
    }

    /// The art's centre.
    pub fn art_centre(&self) -> Point {
        self.art.center()
    }

    /// The ring's inner and outer radii, when the style has one.
    pub fn ring_radii(&self) -> Option<(i32, i32)> {
        match self.style {
            Style::Disc => {
                let inner = self.art.size.width as i32 / 2 + RING_GAP;
                Some((inner, inner + RING))
            }
            Style::Card => None,
        }
    }

    /// The whole frame.
    pub fn bounds(&self) -> Rectangle {
        Rectangle::new(Point::zero(), self.frame)
    }

    /// The centre of the `i`th navigation item.
    pub fn nav_item(&self, i: usize) -> Point {
        let n = 3;
        match self.nav_axis {
            NavAxis::Bar => Point::new(
                self.nav.top_left.x
                    + (self.nav.size.width as i32 * (2 * i as i32 + 1))
                        / (2 * n),
                self.nav.center().y,
            ),
            NavAxis::Rail => Point::new(
                self.nav.center().x,
                self.nav.top_left.y + 30 + 40 * i as i32,
            ),
        }
    }

    /// A touch controller's point, reported in the panel's native 240 by
    /// 320 portrait frame whatever the display was told, brought into
    /// this layout's rotated frame.
    pub fn map_touch(&self, x: u16, y: u16) -> Point {
        let (x, y) = (i32::from(x), i32::from(y));
        let (nw, nh) = (240i32, 320i32);
        match self.rotation % 360 {
            90 => Point::new(y, nw - 1 - x),
            180 => Point::new(nw - 1 - x, nh - 1 - y),
            270 => Point::new(nh - 1 - y, x),
            _ => Point::new(x, y),
        }
    }

    /// What is under `p` on `screen`, for a touch; `rows` is how many
    /// rows a list page has filled.
    pub fn hit_on(
        &self,
        screen: Screen,
        rows: usize,
        p: Point,
    ) -> Option<Hotspot> {
        match screen {
            Screen::NowPlaying => self.hit(p),
            Screen::Library | Screen::Settings => self.hit_page(rows, p),
        }
    }

    /// What is under `p` on a list page with `rows` rows filled.
    fn hit_page(&self, rows: usize, p: Point) -> Option<Hotspot> {
        if let Some(nav) = self.hit_nav(p) {
            return Some(nav);
        }
        let page = &self.page;
        if page.back.contains(p) {
            return Some(Hotspot::Back);
        }
        if page.bar.contains(p) {
            let (dx, dy) = (p.x - page.bar_play.x, p.y - page.bar_play.y);
            if dx * dx + dy * dy <= (page.bar_play_r as i32 + 8).pow(2) {
                return Some(Hotspot::BarPlay);
            }
            return Some(Hotspot::Bar);
        }
        if page.list.contains(p) {
            let i = ((p.y - page.list.top_left.y) / page.row) as usize;
            return Some(if i < rows {
                Hotspot::Row(i as u8)
            } else {
                Hotspot::List
            });
        }
        None
    }

    pub(crate) fn hit_nav(&self, p: Point) -> Option<Hotspot> {
        if !self.nav.contains(p) {
            return None;
        }
        let i = (0..3).min_by_key(|&i| {
            let c = self.nav_item(i);
            (p.x - c.x).abs() + (p.y - c.y).abs()
        })?;
        Some(Hotspot::Nav(i as u8))
    }

    /// What is under `p` on Now Playing, for a touch.
    pub fn hit(&self, p: Point) -> Option<Hotspot> {
        let t = &self.transport;
        let within = |c: Point, r: u32| {
            let (dx, dy) = (p.x - c.x, p.y - c.y);
            dx * dx + dy * dy <= (r as i32 + 6).pow(2)
        };
        if within(t.play, t.play_r) {
            return Some(Hotspot::Play);
        }
        if within(t.prev, t.skip_r) {
            return Some(Hotspot::Previous);
        }
        if within(t.next, t.skip_r) {
            return Some(Hotspot::Next);
        }
        if within(t.shuffle, t.mode_r) {
            return Some(Hotspot::Shuffle);
        }
        if within(t.repeat, t.mode_r) {
            return Some(Hotspot::Repeat);
        }
        // A slider is hit 12 px above and below its bar; where two
        // sliders' reaches meet, the nearer bar takes the touch.
        let along = |bar: Rectangle| {
            let touch = Rectangle::new(
                bar.top_left - Point::new(4, 12),
                bar.size + Size::new(8, 24),
            );
            touch.contains(p).then(|| {
                let f = ((p.x - bar.top_left.x) as f32 / bar.size.width as f32)
                    .clamp(0.0, 1.0);
                ((p.y - bar.center().y).abs(), f)
            })
        };
        let volume =
            along(self.volume.bar).map(|(d, f)| (d, Hotspot::Volume(f)));
        let seek = self
            .seek
            .and_then(along)
            .map(|(d, f)| (d, Hotspot::Seek(f)));
        if let Some((_, hotspot)) =
            [volume, seek].into_iter().flatten().min_by_key(|(d, _)| *d)
        {
            return Some(hotspot);
        }
        if let Some(nav) = self.hit_nav(p) {
            return Some(nav);
        }
        if let Some((inner, outer)) = self.ring_radii() {
            let c = self.art_centre();
            let (dx, dy) = (p.x - c.x, p.y - c.y);
            let d2 = dx * dx + dy * dy;
            if d2 >= (inner - 8).pow(2) && d2 <= (outer + 8).pow(2) {
                return Some(Hotspot::Seek(turns(dx, dy)));
            }
        }
        if [self.title, self.artist, self.album]
            .iter()
            .any(|r| r.contains(p))
        {
            return Some(Hotspot::Text);
        }
        if self.art.contains(p) {
            return Some(Hotspot::Art);
        }
        None
    }
}

/// Clockwise turns from twelve o'clock of the direction `dx, dy` (screen
/// coordinates, y down), 0 to 1, without a trigonometric function: the
/// octant, then a linear ramp inside it, within a hundredth of a turn.
fn turns(dx: i32, dy: i32) -> f32 {
    let (ax, ay) = (dx.abs() as f32, dy.abs() as f32);
    if ax == 0.0 && ay == 0.0 {
        return 0.0;
    }
    // Angle from the vertical axis within the quadrant, 0..0.25.
    let q = if ay >= ax {
        0.125 * ax / ay
    } else {
        0.25 - 0.125 * ay / ax
    };
    match (dx >= 0, dy < 0) {
        (true, true) => q,
        (true, false) => 0.5 - q,
        (false, false) => 0.5 + q,
        (false, true) => 1.0 - q,
    }
}

/// Something touchable on Now Playing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Hotspot {
    /// Play or pause.
    Play,
    /// Previous track.
    Previous,
    /// Next track.
    Next,
    /// A position in the track, 0 to 1: the strip, or the ring.
    Seek(f32),
    /// A level, 0 to 1, on the volume bar.
    Volume(f32),
    /// A navigation item: 0 Now Playing, 1 Library, 2 Settings.
    Nav(u8),
    /// The art.
    Art,
    /// The shuffle icon: toggles the queue's order.
    Shuffle,
    /// The repeat icon: steps the queue's repeat mode.
    Repeat,
    /// The text block: title, artist, album. Opens the details sheet.
    Text,
    /// Anywhere on the details sheet while it is up: puts it down.
    Sheet,
    /// A list page's back chevron.
    Back,
    /// The `i`th visible row of a list page.
    Row(u8),
    /// The list's empty part, below the rows.
    List,
    /// The now-playing bar's play/pause button.
    BarPlay,
    /// The rest of the now-playing bar.
    Bar,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turns_round_the_clock() {
        assert!((turns(0, -10) - 0.0).abs() < 0.01);
        assert!((turns(10, 0) - 0.25).abs() < 0.01);
        assert!((turns(0, 10) - 0.5).abs() < 0.01);
        assert!((turns(-10, 0) - 0.75).abs() < 0.01);
        assert!((turns(7, -7) - 0.125).abs() < 0.01);
        assert!((turns(-7, 7) - 0.625).abs() < 0.01);
    }

    #[test]
    fn the_nearer_of_two_sliders_takes_the_touch() {
        let l = Layout::new(90, Style::Card);
        let seek = l.seek.expect("the card seeks on a strip");
        let (vy, sy) = (l.volume.bar.center().y, seek.center().y);
        assert!(sy - vy <= 24 + 4, "the two reaches meet between the bars");
        let x = l.volume.bar.center().x;
        assert!(matches!(
            l.hit(Point::new(x, vy + 6)),
            Some(Hotspot::Volume(_))
        ));
        assert!(matches!(
            l.hit(Point::new(x, sy - 6)),
            Some(Hotspot::Seek(_))
        ));
    }
}

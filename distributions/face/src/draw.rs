// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Shapes, drawn as rows of pixels with anti-aliased edges.
//!
//! The panel is reached over SPI one window at a time, and a window of a
//! single pixel costs as much as a window of a hundred. So filled shapes
//! are produced row by row: a solid span through the middle, and at each
//! end the one or two pixels the edge crosses, blended between the shape
//! and the colour it sits on by how much of the pixel the shape covers.
//! The face knows what it painted under every shape, so no frame buffer
//! is read back; and at 240 by 320 the stair-step on a circle is what
//! separates a sketch from a product.
//!
//! A shape with texture inside it (the placeholder disc's grooves, the
//! progress ring with its accent) is composed per pixel from the radius
//! and sent as one window a row, rather than as a stack of shapes each
//! with its own edge windows: on the board a window costs about 70 µs,
//! and the disc drawn as twelve rings cost 433 ms.

use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::{DrawTarget, Point, Size};
use embedded_graphics::primitives::Rectangle;

use crate::art::Art;
use crate::theme::{mix, Theme};

/// The widest edge run the row blender handles at once.
const RUN: usize = 48;
/// The widest row a composed shape sends at once: the ring round the
/// largest art (radius 80) is 163 pixels across.
const ROW: usize = 256;

/// A horizontal span.
fn span<D>(
    target: &mut D,
    x0: i32,
    x1: i32,
    y: i32,
    colour: Rgb565,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb565>,
{
    if x1 > x0 {
        target.fill_solid(
            &Rectangle::new(Point::new(x0, y), Size::new((x1 - x0) as u32, 1)),
            colour,
        )?;
    }
    Ok(())
}

/// Distance from the origin in sixteenths of a pixel.
fn dist16(dx: i32, dy: i32) -> i32 {
    (((dx * dx + dy * dy) as u32) << 8).isqrt() as i32
}

/// How much of the pixel at `dx, dy` a disc of radius `r` covers, 0..=16:
/// one-pixel-wide analytic coverage on the edge, as the eye reads it.
fn cover_disc(dx: i32, dy: i32, r: i32) -> u8 {
    (r * 16 + 8 - dist16(dx, dy)).clamp(0, 16) as u8
}

/// A row of a shape: `cover(x)` gives 0..=16 for each pixel; solid runs
/// are one window, edge pixels are blended and sent as short runs.
fn row<D, F>(
    target: &mut D,
    x0: i32,
    x1: i32,
    y: i32,
    colour: Rgb565,
    bg: Rgb565,
    cover: F,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb565>,
    F: Fn(i32) -> u8,
{
    let mut buf = [bg; RUN];
    let mut x = x0;
    while x < x1 {
        let c = cover(x);
        if c == 16 {
            let mut e = x + 1;
            while e < x1 && cover(e) == 16 {
                e += 1;
            }
            span(target, x, e, y, colour)?;
            x = e;
        } else if c == 0 {
            x += 1;
        } else {
            let mut n = 0;
            while x + (n as i32) < x1 && n < RUN {
                let cc = cover(x + n as i32);
                if cc == 0 || cc == 16 {
                    break;
                }
                buf[n] = mix(bg, colour, cc);
                n += 1;
            }
            target.fill_contiguous(
                &Rectangle::new(Point::new(x, y), Size::new(n as u32, 1)),
                buf[..n].iter().copied(),
            )?;
            x += n as i32;
        }
    }
    Ok(())
}

/// A filled disc on `bg`.
pub fn disc<D>(
    target: &mut D,
    c: Point,
    r: i32,
    colour: Rgb565,
    bg: Rgb565,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb565>,
{
    for dy in -r - 1..=r + 1 {
        row(
            target,
            c.x - r - 1,
            c.x + r + 2,
            c.y + dy,
            colour,
            bg,
            |x| cover_disc(x - c.x, dy, r),
        )?;
    }
    Ok(())
}

/// A ring between two radii on `bg`.
pub fn ring<D>(
    target: &mut D,
    c: Point,
    outer: i32,
    inner: i32,
    colour: Rgb565,
    bg: Rgb565,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb565>,
{
    for dy in -outer - 1..=outer + 1 {
        row(
            target,
            c.x - outer - 1,
            c.x + outer + 2,
            c.y + dy,
            colour,
            bg,
            |x| {
                let dx = x - c.x;
                let o = cover_disc(dx, dy, outer);
                let i = cover_disc(dx, dy, inner);
                o.saturating_sub(i)
            },
        )?;
    }
    Ok(())
}

/// Part of a ring: the band between `radii.1` (inner) and `radii.0`
/// (outer), from `turns.0` to `turns.1` clockwise from twelve o'clock (0
/// to 1). The radial edges are anti-aliased; the two ends are cut on the
/// pixel.
pub fn arc<D>(
    target: &mut D,
    c: Point,
    radii: (i32, i32),
    turns: (f32, f32),
    colour: Rgb565,
    bg: Rgb565,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb565>,
{
    let (outer, inner) = radii;
    let Some(sector) = Sector::new(turns) else {
        return Ok(());
    };
    for dy in -outer - 1..=outer + 1 {
        row(
            target,
            c.x - outer - 1,
            c.x + outer + 2,
            c.y + dy,
            colour,
            bg,
            |x| {
                let dx = x - c.x;
                if !sector.inside(dx, dy) {
                    return 0;
                }
                cover_disc(dx, dy, outer)
                    .saturating_sub(cover_disc(dx, dy, inner))
            },
        )?;
    }
    Ok(())
}

/// The ring's track and the accent from `turns.0` to `turns.1` over it,
/// in one pass: each row of the band goes as one window a side (one
/// where the sides meet), every pixel composed from its radius and
/// angle exactly as [`ring`] then [`arc`] would have left it.
pub fn ring_arc<D>(
    target: &mut D,
    c: Point,
    radii: (i32, i32),
    turns: (f32, f32),
    track: Rgb565,
    accent: Rgb565,
    bg: Rgb565,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb565>,
{
    let (outer, inner) = radii;
    let sector = Sector::new(turns);
    let mut buf = [bg; ROW];
    for dy in -outer - 1..=outer + 1 {
        let mut dx = -outer - 1;
        while dx <= outer + 1 {
            let cov = |dx: i32| {
                cover_disc(dx, dy, outer)
                    .saturating_sub(cover_disc(dx, dy, inner))
            };
            if cov(dx) == 0 {
                dx += 1;
                continue;
            }
            let start = dx;
            let mut n = 0;
            while dx <= outer + 1 && n < ROW {
                let cv = cov(dx);
                if cv == 0 {
                    break;
                }
                let on = sector.as_ref().is_some_and(|s| s.inside(dx, dy));
                let colour = if on { accent } else { track };
                buf[n] = if cv == 16 {
                    colour
                } else {
                    mix(bg, colour, cv)
                };
                n += 1;
                dx += 1;
            }
            target.fill_contiguous(
                &Rectangle::new(
                    Point::new(c.x + start, c.y + dy),
                    Size::new(n as u32, 1),
                ),
                buf[..n].iter().copied(),
            )?;
        }
    }
    Ok(())
}

/// The angular test of an arc from `turns.0` to `turns.1` clockwise from
/// twelve o'clock; `None` for an empty arc.
struct Sector {
    a: (i32, i32),
    b: (i32, i32),
    wide: bool,
}

impl Sector {
    fn new(turns: (f32, f32)) -> Option<Self> {
        let to = turns.1.clamp(0.0, 1.0);
        let from = turns.0.clamp(0.0, to);
        if to <= from {
            return None;
        }
        // Direction vectors of the two ends, in 1/1024 units; "inside" is
        // clockwise of `a` and anticlockwise of `b`. Screen y grows
        // downward, so clockwise on screen is anticlockwise in the usual
        // sense.
        let dir = |turns: f32| {
            let (s, cc) = sin_cos_turns(turns);
            (s, -cc) // twelve o'clock is up; x = sin, y = -cos
        };
        Some(Self {
            a: dir(from),
            b: dir(to),
            wide: to - from > 0.5,
        })
    }

    fn inside(&self, dx: i32, dy: i32) -> bool {
        // cross(a, p) >= 0 means p is clockwise of a on screen.
        let ca = self.a.0 * dy - self.a.1 * dx;
        let cb = self.b.0 * dy - self.b.1 * dx;
        if self.wide {
            ca >= 0 || cb <= 0
        } else {
            ca >= 0 && cb <= 0
        }
    }
}

/// sin and cos of `turns` (0 to 1) in 1/1024 units, from a quarter-wave
/// table: the chip has no FPU and this is called twice an arc.
fn sin_cos_turns(turns: f32) -> (i32, i32) {
    const QUARTER: [i16; 65] = [
        0, 25, 50, 75, 100, 125, 150, 175, 200, 224, 249, 273, 297, 321, 345,
        369, 392, 415, 438, 460, 483, 505, 526, 548, 569, 590, 610, 630, 650,
        669, 688, 706, 724, 742, 759, 775, 792, 807, 822, 837, 851, 865, 878,
        891, 903, 915, 926, 936, 946, 955, 964, 972, 980, 987, 993, 999, 1004,
        1008, 1012, 1016, 1019, 1021, 1023, 1024, 1024,
    ];
    // core has no rem_euclid for f32; a turn wraps by hand.
    let whole = turns as i32 as f32;
    let frac = if turns - whole < 0.0 {
        turns - whole + 1.0
    } else {
        turns - whole
    };
    let i = (frac * 256.0) as usize % 256;
    let (q, k) = (i / 64, i % 64);
    let (s, c) = (QUARTER[k] as i32, QUARTER[64 - k] as i32);
    match q {
        0 => (s, c),
        1 => (c, -s),
        2 => (-s, -c),
        _ => (-c, s),
    }
}

/// A rectangle with rounded corners on `bg`.
pub fn rounded<D>(
    target: &mut D,
    area: Rectangle,
    radius: i32,
    colour: Rgb565,
    bg: Rgb565,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb565>,
{
    rounded_corners(target, area, radius, colour, bg, true, true)
}

/// A rectangle with its top and/or bottom corners rounded, on `bg`.
pub fn rounded_corners<D>(
    target: &mut D,
    area: Rectangle,
    radius: i32,
    colour: Rgb565,
    bg: Rgb565,
    top: bool,
    bottom: bool,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb565>,
{
    let (x0, y0) = (area.top_left.x, area.top_left.y);
    let (w, h) = (area.size.width as i32, area.size.height as i32);
    let r = radius.min(w / 2).min(h / 2);
    // Corner circle centres.
    let (lx, rx) = (x0 + r, x0 + w - 1 - r);
    let (ty, by) = (y0 + r, y0 + h - 1 - r);
    for yy in 0..h {
        let y = y0 + yy;
        let corner_y = if top && y < ty {
            Some(ty)
        } else if bottom && y > by {
            Some(by)
        } else {
            None
        };
        match corner_y {
            None => span(target, x0, x0 + w, y, colour)?,
            Some(cy) => row(target, x0, x0 + w, y, colour, bg, |x| {
                if x < lx {
                    cover_disc(x - lx, y - cy, r)
                } else if x > rx {
                    cover_disc(x - rx, y - cy, r)
                } else {
                    16
                }
            })?,
        }
    }
    Ok(())
}

/// A filled triangle on `bg`, edges anti-aliased by 4x4 supersampling.
pub fn triangle<D>(
    target: &mut D,
    pts: [Point; 3],
    colour: Rgb565,
    bg: Rgb565,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb565>,
{
    let (x0, x1) = (
        pts.iter().map(|p| p.x).min().unwrap_or(0),
        pts.iter().map(|p| p.x).max().unwrap_or(0) + 1,
    );
    let (y0, y1) = (
        pts.iter().map(|p| p.y).min().unwrap_or(0),
        pts.iter().map(|p| p.y).max().unwrap_or(0) + 1,
    );
    // Edge functions in quarter-pixel units; a consistent winding.
    let p = pts.map(|q| (q.x * 4, q.y * 4));
    let area = (p[1].0 - p[0].0) * (p[2].1 - p[0].1)
        - (p[1].1 - p[0].1) * (p[2].0 - p[0].0);
    if area == 0 {
        return Ok(());
    }
    let sign = area.signum();
    let inside = |x4: i32, y4: i32| {
        (0..3).all(|i| {
            let (a, b) = (p[i], p[(i + 1) % 3]);
            sign * ((b.0 - a.0) * (y4 - a.1) - (b.1 - a.1) * (x4 - a.0)) >= 0
        })
    };
    for y in y0..y1 {
        row(target, x0, x1, y, colour, bg, |x| {
            let mut n = 0;
            for sy in 0..4 {
                for sx in 0..4 {
                    if inside(x * 4 + sx, y * 4 + sy) {
                        n += 1;
                    }
                }
            }
            n
        })?;
    }
    Ok(())
}

/// The play glyph: a triangle pointing right, sized to a button of `r`.
pub fn icon_play<D>(
    target: &mut D,
    c: Point,
    r: i32,
    colour: Rgb565,
    bg: Rgb565,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb565>,
{
    let (h, w) = (r * 4 / 5, r * 7 / 10);
    triangle(
        target,
        [
            Point::new(c.x - w / 2 + 1, c.y - h),
            Point::new(c.x - w / 2 + 1, c.y + h),
            Point::new(c.x + w / 2 + w / 3 + 1, c.y),
        ],
        colour,
        bg,
    )
}

/// The pause glyph: two bars.
pub fn icon_pause<D>(
    target: &mut D,
    c: Point,
    r: i32,
    colour: Rgb565,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb565>,
{
    let (h, bar, gap) = (r * 3 / 4, (r * 3 / 10).max(2), (r * 3 / 10).max(2));
    for x0 in [c.x - gap - bar, c.x + gap] {
        target.fill_solid(
            &Rectangle::new(
                Point::new(x0, c.y - h),
                Size::new(bar as u32, (2 * h) as u32),
            ),
            colour,
        )?;
    }
    Ok(())
}

/// Previous or next: a triangle and a bar.
pub fn icon_skip<D>(
    target: &mut D,
    c: Point,
    r: i32,
    forward: bool,
    colour: Rgb565,
    bg: Rgb565,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb565>,
{
    let s = if forward { 1 } else { -1 };
    let h = r * 7 / 10;
    triangle(
        target,
        [
            Point::new(c.x - s * r * 9 / 10, c.y - h),
            Point::new(c.x - s * r * 9 / 10, c.y + h),
            Point::new(c.x + s * r * 3 / 10, c.y),
        ],
        colour,
        bg,
    )?;
    let bar_x = c.x + s * r * 5 / 10;
    target.fill_solid(
        &Rectangle::new(
            Point::new(bar_x.min(bar_x + s * 2), c.y - h),
            Size::new(3, (2 * h) as u32),
        ),
        colour,
    )
}

/// A musical note, `r` tall from the head's centre to the flag's top.
pub fn icon_note<D>(
    target: &mut D,
    c: Point,
    r: i32,
    colour: Rgb565,
    bg: Rgb565,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb565>,
{
    let head = (r * 3 / 8).max(3);
    disc(
        target,
        Point::new(c.x - r / 4, c.y + r / 2),
        head,
        colour,
        bg,
    )?;
    let stem = Rectangle::new(
        Point::new(c.x - r / 4 + head - 2, c.y - r / 2),
        Size::new(2, (r + 2) as u32),
    );
    target.fill_solid(&stem, colour)?;
    triangle(
        target,
        [
            Point::new(c.x - r / 4 + head - 2, c.y - r / 2),
            Point::new(c.x - r / 4 + head - 2, c.y - r / 2 + r / 2),
            Point::new(c.x - r / 4 + head + r / 2, c.y - r / 2 + r / 3),
        ],
        colour,
        bg,
    )
}

/// Three lines: a list.
pub fn icon_list<D>(
    target: &mut D,
    c: Point,
    r: i32,
    colour: Rgb565,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb565>,
{
    for i in -1..=1 {
        target.fill_solid(
            &Rectangle::new(
                Point::new(c.x - r, c.y + i * (r * 2 / 3) - 1),
                Size::new((2 * r) as u32, 2),
            ),
            colour,
        )?;
    }
    Ok(())
}

/// A gear: a ring with four teeth.
pub fn icon_gear<D>(
    target: &mut D,
    c: Point,
    r: i32,
    colour: Rgb565,
    bg: Rgb565,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb565>,
{
    ring(target, c, r * 3 / 4, r * 3 / 8, colour, bg)?;
    let t = (r / 4).max(2);
    for (dx, dy) in [(0, -1), (0, 1), (-1, 0), (1, 0)] {
        let p = Point::new(c.x + dx * (r - t / 2), c.y + dy * (r - t / 2));
        target.fill_solid(
            &Rectangle::new(
                Point::new(p.x - t / 2, p.y - t / 2),
                Size::new(t as u32, t as u32),
            ),
            colour,
        )?;
    }
    Ok(())
}

/// The stand-in for album art in a circle: a dark disc with grooves and
/// a label carrying the note, until an artwork plugin supplies a picture.
pub fn placeholder_disc<D>(
    target: &mut D,
    area: Rectangle,
    theme: &Theme,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb565>,
{
    let c = area.center();
    let r = area.size.width as i32 / 2;
    let vinyl = Rgb565::new(3, 6, 4);
    let groove = Rgb565::new(5, 9, 6);
    let label_r = r * 9 / 20;
    let label = mix(theme.card, theme.accent, 4);
    let bg = theme.background;
    // One window a row: the disc's edge on the background, a groove
    // every four pixels from four inside the edge to three outside the
    // label, and the label, each blended as its own shape would be.
    let pixel = |d16: i32| {
        let cover = |radius: i32| (radius * 16 + 8 - d16).clamp(0, 16) as u8;
        let edge = cover(r);
        if edge == 0 {
            return bg;
        }
        let mut colour = if edge == 16 {
            vinyl
        } else {
            mix(bg, vinyl, edge)
        };
        let mut g = r - 4;
        while g > label_r + 3 {
            let cut = cover(g).saturating_sub(cover(g - 1));
            if cut == 16 {
                colour = groove;
            } else if cut > 0 {
                colour = mix(vinyl, groove, cut);
            }
            g -= 4;
        }
        let on_label = cover(label_r);
        if on_label == 16 {
            label
        } else if on_label > 0 {
            mix(vinyl, label, on_label)
        } else {
            colour
        }
    };
    let mut buf = [bg; ROW];
    let width = (2 * r + 3) as usize;
    for dy in -r - 1..=r + 1 {
        for (i, px) in buf[..width].iter_mut().enumerate() {
            let dx = i as i32 - r - 1;
            *px = pixel(dist16(dx, dy));
        }
        target.fill_contiguous(
            &Rectangle::new(
                Point::new(c.x - r - 1, c.y + dy),
                Size::new(width as u32, 1),
            ),
            buf[..width].iter().copied(),
        )?;
    }
    icon_note(target, c, label_r * 5 / 4, theme.accent, label)
}

/// Album art in a circle inscribed in `area`, its edge blended onto
/// `bg`: one window a row, each pixel the picture's, the rim by how
/// much of the pixel the circle covers.
pub fn art_disc<D>(
    target: &mut D,
    area: Rectangle,
    art: &Art,
    bg: Rgb565,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb565>,
{
    let r = area.size.width as i32 / 2;
    let c = area.center();
    let origin = area.top_left;
    let mut buf = [bg; ROW];
    let width = (2 * r + 3) as usize;
    for dy in -r - 1..=r + 1 {
        let y = c.y + dy;
        // Only the pixels the circle touches are sent: the square's
        // corners belong to whatever is drawn round the disc.
        let mut span: Option<(usize, usize)> = None;
        for (i, px) in buf[..width].iter_mut().enumerate() {
            let x = c.x - r - 1 + i as i32;
            let cover = cover_disc(x - c.x, dy, r);
            if cover == 0 {
                continue;
            }
            let p = art.at(x - origin.x, y - origin.y).unwrap_or(bg);
            *px = if cover == 16 { p } else { mix(bg, p, cover) };
            span = Some(span.map_or((i, i), |(first, _)| (first, i)));
        }
        if let Some((first, last)) = span {
            target.fill_contiguous(
                &Rectangle::new(
                    Point::new(c.x - r - 1 + first as i32, y),
                    Size::new((last - first + 1) as u32, 1),
                ),
                buf[first..=last].iter().copied(),
            )?;
        }
    }
    Ok(())
}

/// Album art filling `area` with its corners rounded by `radius` onto
/// `bg`: one window a row.
pub fn art_card<D>(
    target: &mut D,
    area: Rectangle,
    radius: i32,
    art: &Art,
    bg: Rgb565,
    scrim: Option<(i32, Rgb565)>,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb565>,
{
    let (x0, y0) = (area.top_left.x, area.top_left.y);
    let (w, h) = (area.size.width as i32, area.size.height as i32);
    let r = radius.min(w / 2).min(h / 2);
    let (lx, rx) = (x0 + r, x0 + w - 1 - r);
    let (ty, by) = (y0 + r, y0 + h - 1 - r);
    let mut buf = [bg; ROW];
    let width = (w as usize).min(ROW);
    for yy in 0..h {
        let y = y0 + yy;
        let dim = |p| match scrim {
            Some((top, band)) if y >= top => scrimmed(band, p, y - top),
            _ => p,
        };
        let cy = if y < ty {
            Some(ty)
        } else if y > by {
            Some(by)
        } else {
            None
        };
        // Only the pixels the shape touches are sent; the corners stay.
        let mut span: Option<(usize, usize)> = None;
        for (i, px) in buf[..width].iter_mut().enumerate() {
            let x = x0 + i as i32;
            let cover = match cy {
                Some(cy) if x < lx => cover_disc(x - lx, y - cy, r),
                Some(cy) if x > rx => cover_disc(x - rx, y - cy, r),
                _ => 16,
            };
            if cover == 0 {
                continue;
            }
            let p = dim(art.at(i as i32, yy).unwrap_or(bg));
            *px = if cover == 16 { p } else { mix(bg, p, cover) };
            span = Some(span.map_or((i, i), |(first, _)| (first, i)));
        }
        if let Some((first, last)) = span {
            target.fill_contiguous(
                &Rectangle::new(
                    Point::new(x0 + first as i32, y),
                    Size::new((last - first + 1) as u32, 1),
                ),
                buf[first..=last].iter().copied(),
            )?;
        }
    }
    Ok(())
}

/// How much of the art shows through the scrim, sixteenths.
const SCRIM_ART: u8 = 5;
/// The scrim's soft top edge, rows.
const SCRIM_EDGE: i32 = 8;

/// An art pixel `p` seen through the text band's scrim, `into` rows
/// below the band's top: the band's colour with the art showing
/// through, after a soft edge.
pub fn scrimmed(band: Rgb565, p: Rgb565, into: i32) -> Rgb565 {
    let into = into.clamp(0, SCRIM_EDGE) as u8;
    let art = 16 - (16 - SCRIM_ART) * into / SCRIM_EDGE as u8;
    mix(band, p, art)
}

/// The stand-in for album art on a card: a tinted square with the note.
pub fn placeholder_card<D>(
    target: &mut D,
    area: Rectangle,
    radius: i32,
    theme: &Theme,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb565>,
{
    rounded(target, area, radius, theme.secondary, theme.background)?;
    let c = area.center() - Point::new(0, area.size.height as i32 / 8);
    icon_note(
        target,
        c,
        area.size.height as i32 / 3,
        mix(theme.secondary, theme.accent, 6),
        theme.secondary,
    )
}

/// A loudspeaker, `r` tall at the cone's mouth: body, cone, and a sound
/// wave when `sounding`.
pub fn icon_speaker<D>(
    target: &mut D,
    c: Point,
    r: i32,
    sounding: bool,
    colour: Rgb565,
    bg: Rgb565,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb565>,
{
    let (body_w, body_h) = ((r / 2).max(2), (r / 3).max(2));
    let mouth = c.x - r / 6;
    target.fill_solid(
        &Rectangle::new(
            Point::new(mouth - body_w, c.y - body_h),
            Size::new(body_w as u32, (2 * body_h + 1) as u32),
        ),
        colour,
    )?;
    // The cone as two triangles sharing the mouth's edge.
    let (lip_x, back_x) = (mouth + r / 2, mouth - 1);
    triangle(
        target,
        [
            Point::new(back_x, c.y - body_h),
            Point::new(lip_x, c.y - r),
            Point::new(lip_x, c.y + r),
        ],
        colour,
        bg,
    )?;
    triangle(
        target,
        [
            Point::new(back_x, c.y - body_h),
            Point::new(back_x, c.y + body_h),
            Point::new(lip_x, c.y + r),
        ],
        colour,
        bg,
    )?;
    if sounding {
        let wave = Point::new(lip_x - 1, c.y);
        arc(
            target,
            wave,
            (r * 5 / 6 + 1, r * 5 / 6 - 1),
            (0.14, 0.36),
            colour,
            bg,
        )?;
    }
    Ok(())
}

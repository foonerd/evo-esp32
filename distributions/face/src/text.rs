// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Text from the glyph atlas the build script made of Inter.
//!
//! A glyph is drawn as one small window of pixels blended between the
//! text colour and the colour it sits on, so the edges are anti-aliased
//! without a frame buffer: the face knows what it painted under the text.

use alloc::vec::Vec;

use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::{DrawTarget, Point, Size};
use embedded_graphics::primitives::Rectangle;

use crate::theme::mix;

/// One glyph's place in an [`Atlas`].
#[derive(Debug, Clone, Copy)]
pub struct Glyph {
    /// The character.
    pub ch: char,
    /// How far the pen moves after it.
    pub advance: i16,
    /// The bitmap's offset from the pen, x.
    pub x0: i16,
    /// The bitmap's offset from the baseline, y (negative is up).
    pub y0: i16,
    /// Bitmap width.
    pub w: u8,
    /// Bitmap height.
    pub h: u8,
    /// Where its rows start in the atlas's bitmaps, 4 bits a pixel.
    pub offset: u32,
}

/// One face at one size.
#[derive(Debug)]
pub struct Atlas {
    /// Baseline to the top of the tallest glyph.
    pub ascent: i16,
    /// Baseline to the bottom of the lowest glyph (negative).
    pub descent: i16,
    /// Line to line.
    pub line: i16,
    /// Glyphs, sorted by character.
    pub glyphs: &'static [Glyph],
    /// Coverage, two pixels a byte, high nibble first.
    pub bitmaps: &'static [u8],
}

include!(concat!(env!("OUT_DIR"), "/glyphs.rs"));

/// The widest glyph box any atlas here produces, in pixels a side.
const MAX_SIDE: usize = 32;
/// The widest line area painted in one row.
const ROW: usize = 320;

impl Atlas {
    /// The glyph for `c`, or the one for `?` when the atlas lacks it.
    pub fn glyph(&self, c: char) -> &Glyph {
        match self.glyphs.binary_search_by_key(&c, |g| g.ch) {
            Ok(i) => &self.glyphs[i],
            Err(_) => self.glyph('?'),
        }
    }

    /// The pen's travel over `text`.
    pub fn width(&self, text: &str) -> i32 {
        text.chars().map(|c| self.glyph(c).advance as i32).sum()
    }

    /// Height from the top of the ascent to the bottom of the descent.
    pub fn height(&self) -> i32 {
        (self.ascent - self.descent) as i32
    }

    /// `text`, or as much of it as fits `width` with an ellipsis.
    pub fn fit<'a>(&self, text: &'a str, width: i32) -> Fitted<'a> {
        if self.width(text) <= width {
            return Fitted {
                head: text,
                ellipsis: false,
            };
        }
        let dots = self.glyph('…').advance as i32;
        let mut used = 0;
        let mut end = 0;
        for (i, c) in text.char_indices() {
            let w = self.glyph(c).advance as i32;
            if used + w + dots > width {
                break;
            }
            used += w;
            end = i + c.len_utf8();
        }
        Fitted {
            head: text[..end].trim_end(),
            ellipsis: true,
        }
    }

    /// `text` broken at spaces into lines no wider than `width`, at most
    /// `max_lines` of them, the last cut with an ellipsis if more would
    /// not fit. A word wider than the line is cut on its own line.
    pub fn wrap<'a>(
        &self,
        text: &'a str,
        width: i32,
        max_lines: usize,
    ) -> Vec<Fitted<'a>> {
        let mut lines: Vec<Fitted<'a>> = Vec::new();
        let mut rest = text.trim();
        while !rest.is_empty() && lines.len() < max_lines {
            if lines.len() + 1 == max_lines || self.width(rest) <= width {
                lines.push(self.fit(rest, width));
                break;
            }
            // The longest head of whole words that fits.
            let mut cut = 0;
            for (i, _) in rest.match_indices(' ') {
                if self.width(&rest[..i]) <= width {
                    cut = i;
                } else {
                    break;
                }
            }
            if cut == 0 {
                lines.push(self.fit(rest, width));
                let word_end = rest.find(' ').unwrap_or(rest.len());
                rest = rest[word_end..].trim_start();
            } else {
                lines.push(Fitted {
                    head: &rest[..cut],
                    ellipsis: false,
                });
                rest = rest[cut..].trim_start();
            }
        }
        lines
    }

    /// Draw `text` with its baseline's left end at `pen`, `fg` over `bg`.
    /// Returns the pen after the last glyph.
    pub fn draw<D>(
        &self,
        target: &mut D,
        text: &str,
        pen: Point,
        fg: Rgb565,
        bg: Rgb565,
    ) -> Result<Point, D::Error>
    where
        D: DrawTarget<Color = Rgb565>,
    {
        self.draw_over(target, text, pen, fg, |_| bg)
    }

    /// [`Atlas::draw`] over a ground that may differ pixel by pixel.
    pub fn draw_over<D>(
        &self,
        target: &mut D,
        text: &str,
        pen: Point,
        fg: Rgb565,
        ground: impl Fn(Point) -> Rgb565,
    ) -> Result<Point, D::Error>
    where
        D: DrawTarget<Color = Rgb565>,
    {
        let mut x = pen.x;
        let mut px = [fg; MAX_SIDE * MAX_SIDE];
        for c in text.chars() {
            let g = self.glyph(c);
            if g.w > 0 && g.h > 0 {
                let (w, h) = (g.w as usize, g.h as usize);
                let origin = Point::new(x + g.x0 as i32, pen.y + g.y0 as i32);
                let stride = w.div_ceil(2);
                for row in 0..h {
                    let bytes = &self.bitmaps
                        [g.offset as usize + row * stride..][..stride];
                    for col in 0..w {
                        let byte = bytes[col / 2];
                        let cov =
                            if col % 2 == 0 { byte >> 4 } else { byte & 0xF };
                        let bg =
                            ground(origin + Point::new(col as i32, row as i32));
                        // 0..=15 coverage onto the mixer's 0..=16.
                        px[row * w + col] = mix(bg, fg, cov + (cov >> 3));
                    }
                }
                let area =
                    Rectangle::new(origin, Size::new(g.w as u32, g.h as u32));
                target.fill_contiguous(&area, px[..w * h].iter().copied())?;
            }
            x += g.advance as i32;
        }
        Ok(Point::new(x, pen.y))
    }

    /// Draw `text` fitted to `width`, aligned within it, on a line whose
    /// top is `top`; the area is cleared to `bg` first so a shorter text
    /// replaces a longer one cleanly.
    pub fn line<D>(
        &self,
        target: &mut D,
        text: &str,
        area: Rectangle,
        align: Align,
        fg: Rgb565,
        bg: Rgb565,
    ) -> Result<(), D::Error>
    where
        D: DrawTarget<Color = Rgb565>,
    {
        target.fill_solid(&area, bg)?;
        self.line_text(target, text, area, align, fg, |_| bg)
    }

    /// [`Atlas::line`] over a ground that may differ pixel by pixel (art
    /// under a scrim): the area is laid with the ground first.
    pub fn line_over<D>(
        &self,
        target: &mut D,
        text: &str,
        area: Rectangle,
        align: Align,
        fg: Rgb565,
        ground: impl Fn(Point) -> Rgb565,
    ) -> Result<(), D::Error>
    where
        D: DrawTarget<Color = Rgb565>,
    {
        self.lay(target, area, &ground)?;
        self.line_text(target, text, area, align, fg, ground)
    }

    /// Lay `area` with `ground`, a row at a time.
    fn lay<D>(
        &self,
        target: &mut D,
        area: Rectangle,
        ground: impl Fn(Point) -> Rgb565,
    ) -> Result<(), D::Error>
    where
        D: DrawTarget<Color = Rgb565>,
    {
        let mut buf = [Rgb565::default(); ROW];
        let (x0, w) = (area.top_left.x, (area.size.width as usize).min(ROW));
        for yy in 0..area.size.height as i32 {
            let y = area.top_left.y + yy;
            for (i, px) in buf[..w].iter_mut().enumerate() {
                *px = ground(Point::new(x0 + i as i32, y));
            }
            target.fill_contiguous(
                &Rectangle::new(Point::new(x0, y), Size::new(w as u32, 1)),
                buf[..w].iter().copied(),
            )?;
        }
        Ok(())
    }

    fn line_text<D>(
        &self,
        target: &mut D,
        text: &str,
        area: Rectangle,
        align: Align,
        fg: Rgb565,
        ground: impl Fn(Point) -> Rgb565,
    ) -> Result<(), D::Error>
    where
        D: DrawTarget<Color = Rgb565>,
    {
        let fitted = self.fit(text, area.size.width as i32);
        let w = self.width(fitted.head)
            + if fitted.ellipsis {
                self.glyph('…').advance as i32
            } else {
                0
            };
        let x = match align {
            Align::Left => area.top_left.x,
            Align::Centre => area.top_left.x + (area.size.width as i32 - w) / 2,
            Align::Right => area.top_left.x + area.size.width as i32 - w,
        };
        let baseline = area.top_left.y
            + (area.size.height as i32 - self.height()) / 2
            + self.ascent as i32;
        let pen = self.draw_over(
            target,
            fitted.head,
            Point::new(x, baseline),
            fg,
            &ground,
        )?;
        if fitted.ellipsis {
            self.draw_over(target, "…", pen, fg, &ground)?;
        }
        Ok(())
    }
}

/// Text cut to a width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fitted<'a> {
    /// What is drawn.
    pub head: &'a str,
    /// Whether an ellipsis follows it.
    pub ellipsis: bool,
}

/// Where a line sits in its area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    /// Flush left.
    Left,
    /// Centred.
    Centre,
    /// Flush right.
    Right,
}

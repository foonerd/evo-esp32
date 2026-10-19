// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Album art, as the face holds it: a square of RGB565 pixels at the
//! art area's size, cut and scaled from whatever picture the album
//! carried. Decoding the picture is the distribution's business (a
//! JPEG decoder on the chip, the `image` crate on the host); the face
//! takes pixels.

use alloc::vec::Vec;

use embedded_graphics::pixelcolor::raw::RawU16;
use embedded_graphics::pixelcolor::Rgb565;

/// A square picture at the size the face draws it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Art {
    /// The side, in pixels; `pixels` holds `size * size` of them.
    pub size: u32,
    /// RGB565 in the draw target's raw order, row by row.
    pub pixels: Vec<u16>,
}

impl Art {
    /// The pixel at `(x, y)`; the background for a point off the square.
    pub fn at(&self, x: i32, y: i32) -> Option<Rgb565> {
        if x < 0 || y < 0 || x >= self.size as i32 || y >= self.size as i32 {
            return None;
        }
        self.pixels
            .get(y as usize * self.size as usize + x as usize)
            .map(|&p| Rgb565::from(RawU16::new(p)))
    }
}

/// `size` square pixels from a `width` by `height` picture of RGB565
/// pixels: the largest centred square of the source, scaled down by
/// averaging the source pixels under each output pixel (a box filter,
/// integer arithmetic). A source smaller than `size` is scaled up by
/// repetition. `None` if the picture is empty or its length is wrong.
pub fn fit(width: u32, height: u32, pixels: &[u16], size: u32) -> Option<Art> {
    if width == 0 || height == 0 || size == 0 {
        return None;
    }
    if pixels.len() != (width as usize) * (height as usize) {
        return None;
    }
    let side = width.min(height);
    let (x_off, y_off) = ((width - side) / 2, (height - side) / 2);
    let mut out = Vec::with_capacity((size * size) as usize);
    for oy in 0..size {
        // The source rows under output row `oy`, at least one.
        let y0 = y_off + oy * side / size;
        let y1 = (y_off + ((oy + 1) * side).div_ceil(size)).max(y0 + 1);
        for ox in 0..size {
            let x0 = x_off + ox * side / size;
            let x1 = (x_off + ((ox + 1) * side).div_ceil(size)).max(x0 + 1);
            let (mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32);
            for y in y0..y1 {
                let row = &pixels[(y * width) as usize..][..width as usize];
                for &p in &row[x0 as usize..x1 as usize] {
                    r += u32::from(p >> 11);
                    g += u32::from((p >> 5) & 0x3f);
                    b += u32::from(p & 0x1f);
                    n += 1;
                }
            }
            let (r, g, b) = ((r + n / 2) / n, (g + n / 2) / n, (b + n / 2) / n);
            out.push(((r as u16) << 11) | ((g as u16) << 5) | b as u16);
        }
    }
    Some(Art { size, pixels: out })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn fit_crops_to_the_centre_square_and_averages() {
        // 4 wide, 2 high: the centre square is columns 1..3.
        let px = [1u16, 2, 3, 4, 5, 6, 7, 8];
        let art = fit(4, 2, &px, 2).unwrap();
        assert_eq!(art.pixels, vec![2, 3, 6, 7]);
        // Down to one pixel: the four are averaged per channel.
        let one = fit(4, 2, &px, 1).unwrap();
        assert_eq!(one.size, 1);
        assert_eq!(one.pixels.len(), 1);
        // Up from one pixel: repetition.
        let up = fit(1, 1, &[0xF800], 3).unwrap();
        assert_eq!(up.pixels, vec![0xF800; 9]);
        assert!(fit(2, 2, &px, 2).is_none(), "length must match");
        assert!(fit(0, 2, &[], 2).is_none());
    }

    #[test]
    fn at_reads_pixels_and_nothing_outside() {
        let art = Art {
            size: 2,
            pixels: vec![0xF800, 0x07E0, 0x001F, 0xFFFF],
        };
        assert_eq!(art.at(1, 0), Some(Rgb565::from(RawU16::new(0x07E0))));
        assert_eq!(art.at(2, 0), None);
        assert_eq!(art.at(-1, 1), None);
    }
}

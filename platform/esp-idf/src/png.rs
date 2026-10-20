// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! PNG pictures (a station's logo, mostly) decoded to RGB565 by the
//! `png` crate, which handles every colour type, bit depth, palette and
//! interlace the format has; what comes out is a [`jpeg::Image`] like
//! the JPEG decoder's, at the picture's own size. A logo is usually
//! drawn over transparency, so its alpha is composited onto the colour
//! the picture will sit on, given in RGB565.

use std::io::Cursor;

use crate::jpeg::Image;

/// The longest side admitted: a logo is small; a photograph this size
/// would be megabytes of PSRAM and seconds of core.
const MAX_SIDE: u32 = 1024;
/// What the decoder may hold at once, beyond the picture itself.
const LIMIT: usize = 4 * 1024 * 1024;

/// Decode `png`, compositing any transparency onto `under`.
pub fn decode(png: &[u8], under: u16) -> Result<Image, String> {
    let mut limits = png::Limits::default();
    limits.bytes = LIMIT;
    let mut decoder = png::Decoder::new_with_limits(Cursor::new(png), limits);
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    decoder.set_ignore_text_chunk(true);
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let (width, height) = {
        let info = reader.info();
        (info.width, info.height)
    };
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
        return Err(format!("{width}x{height} is more than {MAX_SIDE} a side"));
    }
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| "too large to lay out".to_string())?;
    let mut buf = vec![0u8; size];
    let out = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    let samples = out.color_type.samples();
    let (ur, ug, ub) = expand(under);
    let mut pixels = Vec::with_capacity(width as usize * height as usize);
    for row in buf[..out.buffer_size()].chunks_exact(out.line_size) {
        for px in row[..width as usize * samples].chunks_exact(samples) {
            let (r, g, b, a) = match samples {
                1 => (px[0], px[0], px[0], 255),
                2 => (px[0], px[0], px[0], px[1]),
                3 => (px[0], px[1], px[2], 255),
                _ => (px[0], px[1], px[2], px[3]),
            };
            let over = |c: u8, u: u8| {
                ((u32::from(c) * u32::from(a)
                    + u32::from(u) * (255 - u32::from(a))
                    + 127)
                    / 255) as u8
            };
            pixels.push(pack(over(r, ur), over(g, ug), over(b, ub)));
        }
    }
    Ok(Image {
        width: width as u16,
        height: height as u16,
        pixels,
        divisor: 1,
    })
}

/// RGB565 to 8-bit channels, the low bits filled from the high.
fn expand(c: u16) -> (u8, u8, u8) {
    let r = ((c >> 11) & 0x1f) as u8;
    let g = ((c >> 5) & 0x3f) as u8;
    let b = (c & 0x1f) as u8;
    (
        (r << 3) | (r >> 2),
        (g << 2) | (g >> 4),
        (b << 3) | (b >> 2),
    )
}

/// 8-bit channels to RGB565.
fn pack(r: u8, g: u8, b: u8) -> u16 {
    (u16::from(r >> 3) << 11) | (u16::from(g >> 2) << 5) | u16::from(b >> 3)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channels_round_trip() {
        for c in [0u16, 0xffff, 0xf800, 0x07e0, 0x001f, 0x8410] {
            let (r, g, b) = expand(c);
            assert_eq!(pack(r, g, b), c);
        }
    }
}

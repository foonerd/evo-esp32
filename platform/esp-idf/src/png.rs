// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! PNG pictures (a station's logo, mostly) decoded to RGB565 by the
//! `png` crate, which handles every colour type, bit depth, palette and
//! interlace the format has; what comes out is a [`jpeg::Image`] like
//! the JPEG decoder's, shrunk by a whole factor to near the size asked
//! for as the rows come out. A logo is usually drawn over transparency,
//! so its alpha is composited onto the colour the picture will sit on,
//! given in RGB565.

use std::io::Cursor;

use crate::jpeg::Image;

/// The longest side admitted.
const MAX_SIDE: u32 = 2048;
/// The longest side of an interlaced picture, which is decoded whole.
const INTERLACED_MAX_SIDE: u32 = 512;
/// What the decoder may hold at once, beyond the picture itself.
const LIMIT: usize = 4 * 1024 * 1024;
/// Rows decoded between two breaths. The decoder runs on a thread above
/// the idle task; a large logo beside a decoder near the edge of the
/// core kept the idle task off it for seconds (the task watchdog said
/// so), and a millisecond every so many rows gives it its turn.
const BREATH_ROWS: u32 = 16;

/// Decode `png`, compositing any transparency onto `under`, shrunk on
/// the way to at most `target` pixels a side: the rows come out of the
/// decoder one at a time and are averaged into the smaller picture, so
/// a large logo costs one row and the result, not the whole picture
/// three times over (a 981-pixel logo was six megabytes and eight
/// seconds decoded whole). An interlaced PNG (Adam7) cannot be read a
/// row at a time and is decoded whole when small, refused when not.
pub fn decode(png: &[u8], under: u16, target: u32) -> Result<Image, String> {
    let mut limits = png::Limits::default();
    limits.bytes = LIMIT;
    let mut decoder = png::Decoder::new_with_limits(Cursor::new(png), limits);
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    decoder.set_ignore_text_chunk(true);
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let (width, height, interlaced) = {
        let info = reader.info();
        (info.width, info.height, info.interlaced)
    };
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
        return Err(format!("{width}x{height} is more than {MAX_SIDE} a side"));
    }
    let target = target.max(1);
    // Every `k` source pixels a side become one.
    let k = width.max(height).div_ceil(target).max(1);
    let (out_w, out_h) =
        (width.div_ceil(k) as usize, height.div_ceil(k) as usize);
    let (color, _) = reader.output_color_type();
    let samples = color.samples();
    let (ur, ug, ub) = expand(under);
    let over = |c: u8, a: u8, u: u8| {
        (u32::from(c) * u32::from(a)
            + u32::from(u) * (255 - u32::from(a))
            + 127)
            / 255
    };
    // One output row's running sums: r, g, b and the count behind each.
    let mut sums = vec![[0u32; 4]; out_w];
    let mut pixels = Vec::with_capacity(out_w * out_h);
    let mut rows_in = 0u32;
    let flush = |sums: &mut Vec<[u32; 4]>, pixels: &mut Vec<u16>| {
        for s in sums.iter_mut() {
            let n = s[3].max(1);
            pixels.push(pack(
                (s[0] / n) as u8,
                (s[1] / n) as u8,
                (s[2] / n) as u8,
            ));
            *s = [0; 4];
        }
    };
    let take_row = |row: &[u8], sums: &mut Vec<[u32; 4]>| {
        for (x, px) in row[..width as usize * samples]
            .chunks_exact(samples)
            .enumerate()
        {
            let (r, g, b, a) = match samples {
                1 => (px[0], px[0], px[0], 255),
                2 => (px[0], px[0], px[0], px[1]),
                3 => (px[0], px[1], px[2], 255),
                _ => (px[0], px[1], px[2], px[3]),
            };
            let s = &mut sums[x / k as usize];
            s[0] += over(r, a, ur);
            s[1] += over(g, a, ug);
            s[2] += over(b, a, ub);
            s[3] += 1;
        }
    };
    if interlaced {
        if width.max(height) > INTERLACED_MAX_SIDE {
            return Err(format!(
                "interlaced and {width}x{height}: more than {INTERLACED_MAX_SIDE} a side"
            ));
        }
        let size = reader
            .output_buffer_size()
            .ok_or_else(|| "too large to lay out".to_string())?;
        let mut buf = vec![0u8; size];
        let out = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
        for row in buf[..out.buffer_size()].chunks_exact(out.line_size) {
            take_row(row, &mut sums);
            rows_in += 1;
            if rows_in % k == 0 {
                flush(&mut sums, &mut pixels);
            }
        }
    } else {
        while let Some(row) = reader.next_row().map_err(|e| e.to_string())? {
            take_row(row.data(), &mut sums);
            rows_in += 1;
            if rows_in % k == 0 {
                flush(&mut sums, &mut pixels);
            }
            if rows_in % BREATH_ROWS == 0 {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
    }
    if rows_in % k != 0 {
        flush(&mut sums, &mut pixels);
    }
    if pixels.len() != out_w * out_h {
        return Err(format!("{} pixels out for {out_w}x{out_h}", pixels.len()));
    }
    Ok(Image {
        width: out_w as u16,
        height: out_h as u16,
        pixels,
        divisor: k as u8,
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

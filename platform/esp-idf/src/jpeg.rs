// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Espressif's JPEG decoder (TJpgDec) for album art.
//!
//! A cover is hundreds to a thousand-odd pixels a side and the face's
//! art square is 140 or 180; TJpgDec decodes at 1/2, 1/4 or 1/8 of the
//! picture's size, skipping the inverse transform altogether at 1/8, so
//! [`decode`] asks for the largest reduction that still leaves the
//! picture at least `min_side` pixels on its shorter side and the face
//! fits the rest. Baseline JPEG only: a progressive file is refused by
//! the decoder and the face keeps its stand-in.

use core::fmt;

use esp_idf_svc::sys::esp_jpeg as c;
use esp_idf_svc::sys::{esp_err_t, ESP_ERR_NO_MEM, ESP_OK};

/// A decoded picture: RGB565 in the panel's raw order, row by row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// Width in pixels, after scaling.
    pub width: u16,
    /// Height in pixels, after scaling.
    pub height: u16,
    /// `width * height` pixels.
    pub pixels: Vec<u16>,
    /// The reduction applied: 1, 2, 4 or 8.
    pub divisor: u8,
}

/// Why a picture could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JpegError {
    /// The decoder did not accept the data: not a JPEG, or a progressive
    /// one.
    Undecodable(esp_err_t),
    /// No memory for the decoder's structures.
    NoMemory,
    /// The decoder wrote another number of bytes than its own
    /// dimensions promise.
    Shape {
        /// The width it reported.
        width: u16,
        /// The height it reported.
        height: u16,
        /// The bytes it wrote.
        bytes: usize,
    },
}

impl fmt::Display for JpegError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Undecodable(e) => write!(f, "not decodable (esp_err {e})"),
            Self::NoMemory => write!(f, "no memory for the decoder"),
            Self::Shape {
                width,
                height,
                bytes,
            } => write!(f, "{width}x{height} but {bytes} bytes out"),
        }
    }
}

impl std::error::Error for JpegError {}

/// The picture's size as stored, without decoding it.
pub fn info(jpeg: &[u8]) -> Result<(u16, u16), JpegError> {
    let mut cfg = config(jpeg, c::esp_jpeg_image_scale_t_JPEG_IMAGE_SCALE_0);
    let mut out = c::esp_jpeg_image_output_t::default();
    // SAFETY: `cfg` points at `jpeg` for the duration of the call and the
    // call reads only headers; `out` is a plain struct the call fills.
    match unsafe { c::esp_jpeg_get_image_info(&mut cfg, &mut out) } {
        ESP_OK => Ok((out.width, out.height)),
        e => Err(code(e)),
    }
}

/// The largest reduction (1, 2, 4 or 8) that keeps the shorter side at
/// least `min_side`, or 1 when none does.
pub fn divisor_for(width: u16, height: u16, min_side: u32) -> u8 {
    let side = u32::from(width.min(height));
    [8u8, 4, 2]
        .into_iter()
        .find(|&d| side / u32::from(d) >= min_side)
        .unwrap_or(1)
}

/// Decode `jpeg` at the largest reduction that keeps its shorter side at
/// least `min_side` pixels.
pub fn decode(jpeg: &[u8], min_side: u32) -> Result<Image, JpegError> {
    let (width, height) = info(jpeg)?;
    let divisor = divisor_for(width, height, min_side);
    let scale = match divisor {
        8 => c::esp_jpeg_image_scale_t_JPEG_IMAGE_SCALE_1_8,
        4 => c::esp_jpeg_image_scale_t_JPEG_IMAGE_SCALE_1_4,
        2 => c::esp_jpeg_image_scale_t_JPEG_IMAGE_SCALE_1_2,
        _ => c::esp_jpeg_image_scale_t_JPEG_IMAGE_SCALE_0,
    };
    // `esp_jpeg_get_image_info` reports the stored size whatever the
    // scale; the scaled sides are the stored ones divided, rounded up,
    // and the decoder says what it actually produced.
    let d = u16::from(divisor);
    let (w, h) = (width.div_ceil(d), height.div_ceil(d));
    let mut pixels = vec![0u16; usize::from(w) * usize::from(h)];
    let mut cfg = config(jpeg, scale);
    cfg.outbuf = pixels.as_mut_ptr().cast();
    cfg.outbuf_size = (pixels.len() * 2) as u32;
    let mut out = c::esp_jpeg_image_output_t::default();
    // SAFETY: `cfg` points at `jpeg` and at `pixels`, both alive and
    // exclusively ours for the call; the decoder writes at most
    // `outbuf_size` bytes.
    let rc = unsafe { c::esp_jpeg_decode(&mut cfg, &mut out) };
    if rc != ESP_OK {
        return Err(code(rc));
    }
    let produced = usize::from(out.width) * usize::from(out.height);
    if out.width > w || out.height > h || out.output_len != produced * 2 {
        return Err(JpegError::Shape {
            width: out.width,
            height: out.height,
            bytes: out.output_len,
        });
    }
    pixels.truncate(produced);
    Ok(Image {
        width: out.width,
        height: out.height,
        pixels,
        divisor,
    })
}

fn config(
    jpeg: &[u8],
    scale: c::esp_jpeg_image_scale_t,
) -> c::esp_jpeg_image_cfg_t {
    let mut cfg = c::esp_jpeg_image_cfg_t::default();
    cfg.indata = jpeg.as_ptr().cast_mut();
    cfg.indata_size = jpeg.len() as u32;
    cfg.out_format = c::esp_jpeg_image_format_t_JPEG_IMAGE_FORMAT_RGB565;
    cfg.out_scale = scale;
    // The panel takes RGB565 big-endian on the wire; the display driver
    // swaps, so the decoder leaves the bytes in native order.
    cfg.flags.set_swap_color_bytes(0);
    cfg
}

fn code(e: esp_err_t) -> JpegError {
    if e == ESP_ERR_NO_MEM as esp_err_t {
        JpegError::NoMemory
    } else {
        JpegError::Undecodable(e)
    }
}

#[cfg(test)]
mod tests {
    use super::divisor_for;

    #[test]
    fn the_divisor_keeps_the_shorter_side_at_least_the_minimum() {
        assert_eq!(divisor_for(1400, 1400, 140), 8); // 175
        assert_eq!(divisor_for(600, 597, 140), 4); // 149
        assert_eq!(divisor_for(600, 597, 180), 2); // 298
        assert_eq!(divisor_for(300, 300, 140), 2); // 150
        assert_eq!(divisor_for(200, 200, 140), 1);
        assert_eq!(divisor_for(160, 160, 180), 1); // smaller than wanted
    }
}

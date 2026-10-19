// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The glyph atlas: Inter rasterised at the sizes the face uses, written
//! as Rust the crate includes. Four bits of coverage a pixel, so text is
//! anti-aliased against the colour it is drawn over; no font machinery on
//! the target.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use ab_glyph::{Font, FontRef, Glyph, PxScale, ScaleFont};

/// (name, file, pixel size). The names are the face's type scale.
const FACES: &[(&str, &str, f32)] = &[
    ("TITLE", "Inter-SemiBold.ttf", 16.0),
    ("HEADING", "Inter-SemiBold.ttf", 18.0),
    ("BODY", "Inter-Regular.ttf", 13.0),
    ("SMALL", "Inter-Regular.ttf", 11.0),
    ("TINY", "Inter-Regular.ttf", 9.0),
];

/// ASCII, Latin-1, the dashes and quotes, the ellipsis, the chevron, the
/// middle dot's neighbours, and the note.
fn charset() -> Vec<char> {
    let mut set: Vec<char> = (0x20u32..0x7F)
        .chain(0xA0..0x100)
        .filter_map(char::from_u32)
        .collect();
    set.extend("–—‘’“”…‹›♪".chars());
    set.sort_unstable();
    set
}

/// The renderer blends one glyph at a time into a stack buffer this many
/// pixels a side (`text::MAX_SIDE`).
const MAX_SIDE: usize = 32;

fn main() {
    let fonts = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/fonts");
    let out = Path::new(&env::var("OUT_DIR").unwrap()).join("glyphs.rs");
    let mut src = String::new();
    let chars = charset();
    for (name, file, px) in FACES {
        let path = fonts.join(file);
        println!("cargo:rerun-if-changed={}", path.display());
        let data = fs::read(&path)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let font = FontRef::try_from_slice(&data).expect("a TrueType font");
        let scaled = font.as_scaled(PxScale::from(*px));
        let ascent = scaled.ascent().round() as i16;
        let descent = scaled.descent().round() as i16;
        let line = (scaled.height() + scaled.line_gap()).round() as i16;

        let mut bitmaps: Vec<u8> = Vec::new();
        let mut glyphs = String::new();
        let mut count = 0usize;
        for &c in &chars {
            let id = font.glyph_id(c);
            if id.0 == 0 && c != ' ' {
                continue;
            }
            let advance = scaled.h_advance(id).round() as i16;
            let glyph: Glyph =
                id.with_scale_and_position(*px, ab_glyph::point(0.0, 0.0));
            let (x0, y0, w, h, offset) = match font.outline_glyph(glyph) {
                Some(outlined) => {
                    let b = outlined.px_bounds();
                    let (w, h) =
                        (b.width().ceil() as usize, b.height().ceil() as usize);
                    assert!(
                        w <= MAX_SIDE && h <= MAX_SIDE,
                        "{name} {c:?} is {w}x{h}, over the renderer's {MAX_SIDE}"
                    );
                    let offset = bitmaps.len();
                    let mut cov = vec![0u8; w * h];
                    outlined.draw(|x, y, v| {
                        let (x, y) = (x as usize, y as usize);
                        if x < w && y < h {
                            cov[y * w + x] =
                                (v.clamp(0.0, 1.0) * 15.0).round() as u8;
                        }
                    });
                    for row in cov.chunks(w) {
                        for pair in row.chunks(2) {
                            bitmaps.push(
                                pair[0] << 4
                                    | pair.get(1).copied().unwrap_or(0),
                            );
                        }
                    }
                    (
                        b.min.x.floor() as i16,
                        b.min.y.floor() as i16,
                        w as u8,
                        h as u8,
                        offset as u32,
                    )
                }
                None => (0, 0, 0, 0, bitmaps.len() as u32),
            };
            let _ = writeln!(
                glyphs,
                "    Glyph {{ ch: {:?}, advance: {advance}, x0: {x0}, y0: {y0}, w: {w}, h: {h}, offset: {offset} }},",
                c
            );
            count += 1;
        }
        let _ = writeln!(
            src,
            "/// Inter {px} px, {count} glyphs.\npub static {name}: Atlas = Atlas {{\n    ascent: {ascent}, descent: {descent}, line: {line},\n    glyphs: &[\n{glyphs}    ],\n    bitmaps: &{bitmaps:?},\n}};\n"
        );
    }
    fs::write(&out, src).unwrap();
}

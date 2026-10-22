// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! A FLAC file's pictures turned into padding before the demuxer sees
//! them.
//!
//! A ripped FLAC carries its cover in a PICTURE metadata block, often
//! half a megabyte, sometimes more; Symphonia 0.6 reads a picture block
//! whole into memory at the probe (its visual-bytes limit is a `TODO`
//! for FLAC) and this player takes its art from the folder, not the
//! file. On a card that read is a third of a second on the bus the
//! panel shares; on a share it is the network's. The block header is
//! one byte of type: rewritten as PADDING the demuxer seeks past the
//! block instead. The file is not touched: [`Patched`] substitutes the
//! byte as it is read, so every offset — and every block boundary the
//! card's read-ahead keeps — stays where it was.

use std::io::{Read, Seek, SeekFrom};

/// The metadata block type of a picture, and of padding.
const PICTURE: u8 = 6;
const PADDING: u8 = 1;
/// The most blocks a header chain is followed; a file is not that
/// decorated, and a corrupt chain stops here.
const MOST_BLOCKS: usize = 64;

/// The bytes to substitute so a FLAC file's pictures read as padding:
/// `(offset, byte)` for the header byte of each PICTURE block, the
/// is-last bit kept. Empty for a file that is not FLAC. The source is
/// left at its start.
pub fn picture_patches<S: Read + Seek>(
    source: &mut S,
) -> std::io::Result<Vec<(u64, u8)>> {
    let mut patches = Vec::new();
    source.seek(SeekFrom::Start(0))?;
    let mut marker = [0u8; 4];
    if source.read_exact(&mut marker).is_err() || &marker != b"fLaC" {
        source.seek(SeekFrom::Start(0))?;
        return Ok(patches);
    }
    let mut at = 4u64;
    for _ in 0..MOST_BLOCKS {
        let mut header = [0u8; 4];
        if source.read_exact(&mut header).is_err() {
            break;
        }
        let last = header[0] & 0x80;
        let kind = header[0] & 0x7f;
        let len =
            u64::from(u32::from_be_bytes([0, header[1], header[2], header[3]]));
        if kind == PICTURE {
            patches.push((at, last | PADDING));
        }
        if last != 0 {
            break;
        }
        at += 4 + len;
        source.seek(SeekFrom::Start(at))?;
    }
    source.seek(SeekFrom::Start(0))?;
    Ok(patches)
}

/// A seekable source with single bytes substituted as they are read.
pub struct Patched<S> {
    inner: S,
    /// Sorted by offset.
    patches: Vec<(u64, u8)>,
    pos: u64,
}

impl<S: Read + Seek> Patched<S> {
    /// `source` with `patches` laid over it; the source is assumed at
    /// its start.
    pub fn new(inner: S, mut patches: Vec<(u64, u8)>) -> Self {
        patches.sort_unstable_by_key(|p| p.0);
        Self {
            inner,
            patches,
            pos: 0,
        }
    }
}

impl<S: Read> Read for Patched<S> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        let from = self.pos;
        let to = from + n as u64;
        for &(at, byte) in &self.patches {
            if at >= from && at < to {
                buf[(at - from) as usize] = byte;
            }
        }
        self.pos = to;
        Ok(n)
    }
}

impl<S: Seek> Seek for Patched<S> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.pos = self.inner.seek(pos)?;
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A FLAC-shaped file: marker, STREAMINFO (34 bytes), a comment
    /// block, a picture, a second picture marked last, then "audio".
    fn flac_shaped() -> (Vec<u8>, Vec<(u64, u8)>) {
        let mut f = b"fLaC".to_vec();
        let mut block = |last: bool, kind: u8, body: &[u8]| {
            let len = body.len() as u32;
            let at = f.len() as u64;
            f.push(if last { 0x80 } else { 0 } | kind);
            f.extend_from_slice(&len.to_be_bytes()[1..]);
            f.extend_from_slice(body);
            at
        };
        block(false, 0, &[0x11; 34]);
        block(false, 4, &[0x22; 50]);
        let first = block(false, PICTURE, &[0x33; 700]);
        let second = block(true, PICTURE, &[0x44; 300]);
        f.extend_from_slice(&[0xFF; 200]);
        (f, vec![(first, PADDING), (second, 0x80 | PADDING)])
    }

    #[test]
    fn pictures_are_found_and_the_last_bit_kept() {
        let (bytes, want) = flac_shaped();
        let mut c = Cursor::new(bytes);
        assert_eq!(picture_patches(&mut c).unwrap(), want);
        assert_eq!(c.position(), 0, "left at the start");
    }

    #[test]
    fn a_file_that_is_not_flac_gets_no_patches() {
        let mut c = Cursor::new(b"RIFF....WAVEfmt ".to_vec());
        assert!(picture_patches(&mut c).unwrap().is_empty());
        assert_eq!(c.position(), 0);
        let mut short = Cursor::new(b"fL".to_vec());
        assert!(picture_patches(&mut short).unwrap().is_empty());
    }

    #[test]
    fn the_patched_bytes_read_as_padding_and_nothing_else_moves() {
        let (bytes, patches) = flac_shaped();
        let mut p = Patched::new(Cursor::new(bytes.clone()), patches.clone());
        // Odd-sized reads so a patch falls mid-buffer and at an edge.
        let mut out = Vec::new();
        let mut buf = [0u8; 37];
        loop {
            let n = p.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        assert_eq!(out.len(), bytes.len());
        for (i, (a, b)) in out.iter().zip(&bytes).enumerate() {
            match patches.iter().find(|(at, _)| *at == i as u64) {
                Some((_, byte)) => assert_eq!(a, byte, "at {i}"),
                None => assert_eq!(a, b, "at {i}"),
            }
        }
        // A seek onto a patch, then a read across it.
        let (at, byte) = patches[1];
        p.seek(SeekFrom::Start(at - 3)).unwrap();
        let mut six = [0u8; 6];
        p.read_exact(&mut six).unwrap();
        assert_eq!(six[3], byte);
        assert_eq!(&six[..3], &bytes[at as usize - 3..at as usize]);
    }
}

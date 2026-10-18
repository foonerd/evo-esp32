// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The host's frame sink: a DAC made of a clock and a file.
//!
//! A real output takes frames into a short buffer and lets them out at
//! the sample rate; when the buffer is full it takes nothing, and the
//! decoder waits. This sink does the same with the wall clock: it holds
//! up to [`LEAD`] of audio, drains it at the stream's rate, and what
//! drains goes to a WAV file if one was asked for, so a run on a desktop
//! leaves something to listen to. Without a file the frames drain into
//! nothing and only the pacing remains.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use evo_esp32_sdk::contract::audio::playback::{Format, FrameSink};

/// How much audio the sink holds ahead of the clock.
pub const LEAD: Duration = Duration::from_millis(200);

/// A paced sink, optionally recording to a WAV file.
pub struct PacedWav {
    path: Option<PathBuf>,
    out: Option<Wav>,
    format: Option<Format>,
    /// Interleaved samples taken but not yet let out.
    held: VecDeque<i16>,
    /// When the stream started, and frames let out since.
    clock: Option<Instant>,
    let_out: u64,
    /// Frames taken in all, for the run's report.
    pub taken: u64,
}

impl PacedWav {
    /// A sink that records to `path` if given.
    pub fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            out: None,
            format: None,
            held: VecDeque::new(),
            clock: None,
            let_out: 0,
            taken: 0,
        }
    }

    fn channels(&self) -> usize {
        self.format
            .as_ref()
            .map_or(1, |f| usize::from(f.channels).max(1))
    }

    fn rate(&self) -> u64 {
        self.format.as_ref().map_or(0, |f| u64::from(f.sample_rate))
    }

    /// Frames the clock says should have left by now.
    fn due(&self) -> u64 {
        match self.clock {
            Some(start) => {
                start.elapsed().as_micros() as u64 * self.rate() / 1_000_000
            }
            None => 0,
        }
    }

    /// Let out what the clock has released, to the file or to nothing.
    fn drain(&mut self) {
        let ch = self.channels();
        let release = self.due().saturating_sub(self.let_out);
        let samples = (release as usize * ch).min(self.held.len());
        if samples == 0 {
            return;
        }
        let out: Vec<i16> = self.held.drain(..samples).collect();
        self.let_out += (samples / ch) as u64;
        if let Some(w) = self.out.as_mut() {
            if let Err(e) = w.write(&out) {
                eprintln!("wav: {e}");
                self.out = None;
            }
        }
    }

    /// Room left in the buffer, in frames.
    fn room(&self) -> usize {
        let lead = (LEAD.as_micros() as u64 * self.rate() / 1_000_000) as usize;
        lead.saturating_sub(self.held.len() / self.channels())
    }
}

impl FrameSink for PacedWav {
    fn start(&mut self, format: &Format) {
        self.stop();
        self.format = Some(format.clone());
        self.clock = Some(Instant::now());
        self.let_out = 0;
        if let Some(p) = &self.path {
            match Wav::create(p, format) {
                Ok(w) => self.out = Some(w),
                Err(e) => eprintln!("wav: {}: {e}", p.display()),
            }
        }
    }

    fn write(&mut self, interleaved: &[i16]) -> usize {
        self.drain();
        let ch = self.channels();
        let frames = (interleaved.len() / ch).min(self.room());
        self.held.extend(&interleaved[..frames * ch]);
        self.taken += frames as u64;
        frames
    }

    fn flush(&mut self) {
        self.held.clear();
        // The clock carries on; what was due and dropped is not owed.
        self.let_out = self.due();
    }

    fn stop(&mut self) {
        if self.clock.is_none() {
            return;
        }
        // The tail goes out whole; a real sink would play it to the end.
        let rest: Vec<i16> = self.held.drain(..).collect();
        if let Some(mut w) = self.out.take() {
            if let Err(e) = w.write(&rest).and_then(|()| w.finish()) {
                eprintln!("wav: {e}");
            }
        }
        self.clock = None;
    }
}

/// A 16-bit PCM WAV being written; the sizes are patched at the end.
pub struct Wav {
    file: BufWriter<File>,
    bytes: u32,
}

impl Wav {
    /// Create `path` with a header for `format`.
    pub fn create(path: &Path, format: &Format) -> io::Result<Self> {
        let mut file = BufWriter::new(File::create(path)?);
        let ch = u16::from(format.channels);
        let rate = format.sample_rate;
        let block = ch * 2;
        file.write_all(b"RIFF")?;
        file.write_all(&0u32.to_le_bytes())?; // patched in finish
        file.write_all(b"WAVEfmt ")?;
        file.write_all(&16u32.to_le_bytes())?;
        file.write_all(&1u16.to_le_bytes())?; // PCM
        file.write_all(&ch.to_le_bytes())?;
        file.write_all(&rate.to_le_bytes())?;
        file.write_all(&(rate * u32::from(block)).to_le_bytes())?;
        file.write_all(&block.to_le_bytes())?;
        file.write_all(&16u16.to_le_bytes())?;
        file.write_all(b"data")?;
        file.write_all(&0u32.to_le_bytes())?; // patched in finish
        Ok(Self { file, bytes: 0 })
    }

    /// Append interleaved samples.
    pub fn write(&mut self, samples: &[i16]) -> io::Result<()> {
        for s in samples {
            self.file.write_all(&s.to_le_bytes())?;
        }
        self.bytes += (samples.len() * 2) as u32;
        Ok(())
    }

    /// Patch the sizes and close.
    pub fn finish(mut self) -> io::Result<()> {
        self.file.flush()?;
        let f = self.file.get_mut();
        f.seek(SeekFrom::Start(4))?;
        f.write_all(&(36 + self.bytes).to_le_bytes())?;
        f.seek(SeekFrom::Start(40))?;
        f.write_all(&self.bytes.to_le_bytes())?;
        f.flush()
    }
}

/// Write a test tone: `ms` of 16-bit stereo at `rate`, a 440 Hz sine on
/// both channels. For the check and the tests, which need a real file.
pub fn write_tone(path: &Path, rate: u32, ms: u64) -> io::Result<()> {
    let format = Format {
        codec: "pcm_s16le".into(),
        sample_rate: rate,
        bits: 16,
        channels: 2,
    };
    let mut w = Wav::create(path, &format)?;
    let frames = rate as u64 * ms / 1000;
    let mut buf = Vec::with_capacity(2 * 1024);
    for i in 0..frames {
        let t = i as f64 / f64::from(rate);
        let s = ((t * 440.0 * std::f64::consts::TAU).sin() * 12000.0) as i16;
        buf.push(s);
        buf.push(s);
        if buf.len() == buf.capacity() {
            w.write(&buf)?;
            buf.clear();
        }
    }
    w.write(&buf)?;
    w.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt() -> Format {
        Format {
            codec: "pcm_s16le".into(),
            sample_rate: 1000,
            bits: 16,
            channels: 2,
        }
    }

    #[test]
    fn the_sink_holds_a_lead_and_then_refuses() {
        let mut s = PacedWav::new(None);
        s.start(&fmt());
        // 1000 frames/s and a 200 ms lead: 200 frames of room at t=0.
        let took = s.write(&[0i16; 2 * 1000]);
        assert!(took <= 200 && took > 150, "took {took}");
        assert_eq!(s.write(&[0i16; 2 * 100]), 0, "full");
        std::thread::sleep(Duration::from_millis(60));
        let more = s.write(&[0i16; 2 * 1000]);
        assert!(more >= 40, "the clock released {more}");
    }

    #[test]
    fn flush_empties_and_stop_writes_the_tail() {
        let dir = std::env::temp_dir()
            .join(format!("evo-host-sink-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.wav");
        let mut s = PacedWav::new(Some(path.clone()));
        s.start(&fmt());
        s.write(&[1i16; 2 * 50]);
        s.flush();
        assert!(s.held.is_empty());
        s.write(&[2i16; 2 * 30]);
        s.stop();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(bytes.len(), 44 + 30 * 2 * 2);
        assert_eq!(
            u32::from_le_bytes(bytes[40..44].try_into().unwrap()),
            30 * 2 * 2
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_tone_file_is_a_wav_of_the_length_asked() {
        let dir = std::env::temp_dir()
            .join(format!("evo-host-tone-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tone.wav");
        write_tone(&path, 8000, 250).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len(), 44 + 2000 * 4);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

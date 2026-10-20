// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Album art for the face, fetched off the main loop.
//!
//! The picture is the one the track names (`Track::art`: a station's
//! logo, on the card or on the web) or, for a file on the card, its
//! folder's `cover.jpg`, `folder.jpg`, `albumart.jpg` or `front.jpg`
//! (the card's file system is case-insensitive). It is read and decoded
//! by one worker thread below the main task's priority, so the decoder's
//! tick is never kept waiting: TJpgDec on a 600-pixel cover is a second
//! or more of core on this chip, and the ring holds a quarter of a
//! second. A file is read in small pieces with a yield between, since
//! the card shares its SPI bus with the music being decoded. Requests go
//! to the worker through a channel and the newest wins; the result comes
//! back through another and the main loop hands it to the face, which
//! repaints the art alone. One picture is fetched once; the next track
//! from the same folder, or the same station, keeps it. A cover is
//! cropped to its centre square; a logo is set whole on a square of the
//! face's background, since its width is its shape.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use esp_idf_svc::hal::task::thread::{MallocCap, ThreadSpawnConfiguration};
use esp_idf_svc::sys;
use evo_esp32_face::art::{self, Art};
use evo_esp32_platform_esp_idf::jpeg::{self, Image};
use evo_esp32_platform_esp_idf::{png, HttpOpener};
use evo_esp32_sdk::contract::audio::playback::StreamOpener;
use evo_esp32_sdk::contract::audio::Track;
use log::{debug, info, warn};

/// File names tried in the album folder, in order.
const NAMES: [&str; 4] =
    ["cover.jpg", "folder.jpg", "albumart.jpg", "front.jpg"];
/// The most a picture from the web may be; a logo is tens of kilobytes.
const WEB_LIMIT: usize = 1024 * 1024;
/// The worker's stack; TJpgDec's work area is heap, this is the file
/// walk, the decode call and the fit.
const STACK: usize = 12 * 1024;
/// Below the main task, which runs at [`MAIN_PRIORITY`].
const WORKER_PRIORITY: u8 = 1;
/// The main task's priority once the fetcher exists: ESP-IDF starts it
/// at 1, level with the worker, which would share the core by turns.
pub const MAIN_PRIORITY: u8 = 2;
/// A piece of the file per bus turn, and the pause that lets the music
/// have the bus between pieces.
const PIECE: usize = 4096;
const BREATH: Duration = Duration::from_millis(2);

/// Where a picture comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// An album folder on the card: the first of [`NAMES`] in it.
    Folder(PathBuf),
    /// One picture on the card, a station's logo.
    File(PathBuf),
    /// A picture on the web, a station's logo.
    Url(String),
}

/// What a fetch found.
pub struct Found {
    /// What it was fetched from.
    pub source: Source,
    /// The picture at the face's size, or none.
    pub art: Option<Art>,
}

/// One request to the worker.
struct Ask {
    source: Source,
    /// The side of the face's art square.
    size: u32,
    /// The face's background, under a logo's transparency and around a
    /// logo that is not square.
    under: u16,
}

/// Fetches pictures, one at a time, on its own thread.
pub struct Fetcher {
    requests: Sender<Ask>,
    results: Receiver<Found>,
    /// Where the picture the face shows, or is fetching, comes from.
    current: Option<Source>,
    /// The side of the face's art square.
    size: u32,
    /// The face's background colour, RGB565.
    under: u16,
}

impl Fetcher {
    /// A fetcher for art squares of `size` pixels on a face whose
    /// background is `under`. Raises the calling task's priority so the
    /// worker runs beneath it.
    pub fn new(size: u32, under: u16) -> Result<Self, std::io::Error> {
        // SAFETY: a null handle names the calling task; raising its
        // priority is a plain scheduler call.
        unsafe {
            sys::vTaskPrioritySet(
                core::ptr::null_mut(),
                u32::from(MAIN_PRIORITY),
            );
        }
        let (requests, inbox) = channel::<Ask>();
        let (outbox, results) = channel::<Found>();
        let before = ThreadSpawnConfiguration::get();
        ThreadSpawnConfiguration {
            name: Some(c"art"),
            stack_size: STACK,
            priority: WORKER_PRIORITY,
            // The stack lives in PSRAM: this thread never runs while the
            // flash cache is off (it writes no flash), and internal SRAM
            // is what the Wi-Fi stack and the DMA rings compete for.
            stack_alloc_caps: MallocCap::Spiram | MallocCap::Cap8bit,
            ..Default::default()
        }
        .set()
        .map_err(std::io::Error::other)?;
        std::thread::Builder::new()
            .stack_size(STACK)
            .spawn(move || work(&inbox, &outbox))?;
        before
            .unwrap_or_default()
            .set()
            .map_err(std::io::Error::other)?;
        Ok(Self {
            requests,
            results,
            current: None,
            size,
            under,
        })
    }

    /// The face's art square changed size (a style change) or its
    /// background (a theme change): fetch the current picture again for
    /// the new face. `true` when a fetch is asked for, which is when the
    /// face should drop the old art.
    pub fn refit(&mut self, size: u32, under: u16) -> bool {
        if size == self.size && under == self.under {
            return false;
        }
        self.size = size;
        self.under = under;
        match &self.current {
            Some(source) => self.ask(source.clone()),
            None => false,
        }
    }

    /// `track` is playing: fetch its picture unless it is already in
    /// hand. `true` when the face should drop the art it shows: a fetch
    /// is asked for, or the track has no picture to look for (a stream
    /// without a logo).
    pub fn track(&mut self, track: &Track) -> bool {
        let Some(source) = source_of(track) else {
            return self.current.take().is_some();
        };
        if self.current.as_ref() == Some(&source) {
            return false;
        }
        self.current = Some(source.clone());
        self.ask(source)
    }

    fn ask(&self, source: Source) -> bool {
        // The worker is gone only if its thread died, which is logged.
        self.requests
            .send(Ask {
                source,
                size: self.size,
                under: self.under,
            })
            .is_ok()
    }

    /// A finished fetch, if one is waiting; only the one for the current
    /// picture counts, an older one is dropped.
    pub fn poll(&mut self) -> Option<Found> {
        loop {
            match self.results.try_recv() {
                Ok(found) if Some(&found.source) == self.current.as_ref() => {
                    return Some(found)
                }
                Ok(_) => continue,
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => {
                    return None
                }
            }
        }
    }
}

/// Where a track's picture is: what it names, else its folder on the
/// card, else nowhere.
fn source_of(track: &Track) -> Option<Source> {
    if let Some(art) = track.art.as_deref() {
        if let Some(path) = art.strip_prefix("file://") {
            return Some(Source::File(PathBuf::from(path)));
        }
        if art.starts_with("http://") || art.starts_with("https://") {
            return Some(Source::Url(art.to_string()));
        }
    }
    track
        .uri
        .strip_prefix("file://")
        .map(Path::new)
        .and_then(Path::parent)
        .map(|dir| Source::Folder(dir.to_path_buf()))
}

/// The worker: the newest request wins, the rest are dropped unread.
fn work(inbox: &Receiver<Ask>, outbox: &Sender<Found>) {
    while let Ok(mut ask) = inbox.recv() {
        while let Ok(newer) = inbox.try_recv() {
            ask = newer;
        }
        let before = internal_free();
        let art = match &ask.source {
            Source::Folder(dir) => fetch_folder(dir, ask.size),
            Source::File(path) => fetch_file(path, ask.size, ask.under)
                .unwrap_or_else(|e| {
                    warn!("art: {}: {e}", path.display());
                    None
                }),
            Source::Url(url) => fetch_url(url, ask.size, ask.under)
                .unwrap_or_else(|e| {
                    warn!("art: {url}: {e}");
                    None
                }),
        };
        debug!(
            "art: internal SRAM {before} B before, {} B after",
            internal_free()
        );
        if outbox
            .send(Found {
                source: ask.source,
                art,
            })
            .is_err()
        {
            return;
        }
    }
    warn!("art: the main loop is gone; worker stopping");
}

/// A logo on the card, set whole on a square of the background.
fn fetch_file(
    path: &Path,
    size: u32,
    under: u16,
) -> Result<Option<Art>, String> {
    let started = Instant::now();
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let bytes = read_in_pieces(file).map_err(|e| e.to_string())?;
    let read = started.elapsed();
    let image = decode(&bytes, size, under)?;
    drop(bytes);
    info!(
        "art: {} {}x{} read in {} ms, decoded in {} ms",
        path.display(),
        image.width,
        image.height,
        read.as_millis(),
        (started.elapsed() - read).as_millis()
    );
    Ok(letterbox(&image, size, under))
}

/// A logo on the web, set whole on a square of the background.
fn fetch_url(url: &str, size: u32, under: u16) -> Result<Option<Art>, String> {
    let started = Instant::now();
    let mut stream = HttpOpener.open(url)?;
    let mut bytes = Vec::new();
    let mut piece = vec![0u8; PIECE];
    loop {
        let n = stream.read(&mut piece)?;
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&piece[..n]);
        if bytes.len() > WEB_LIMIT {
            return Err(format!("more than {} KB", WEB_LIMIT / 1024));
        }
    }
    drop(stream);
    let fetched = started.elapsed();
    let image = decode(&bytes, size, under)?;
    info!(
        "art: {url} {} KB in {} ms, {}x{} decoded in {} ms",
        bytes.len() / 1024,
        fetched.as_millis(),
        image.width,
        image.height,
        (started.elapsed() - fetched).as_millis()
    );
    Ok(letterbox(&image, size, under))
}

/// A picture by what its first bytes say it is.
fn decode(bytes: &[u8], size: u32, under: u16) -> Result<Image, String> {
    match bytes {
        [0xff, 0xd8, ..] => {
            jpeg::decode(bytes, size).map_err(|e| e.to_string())
        }
        [0x89, b'P', b'N', b'G', ..] => png::decode(bytes, under),
        _ => Err("neither a JPEG nor a PNG".to_string()),
    }
}

/// The picture whole, centred on a square of `under` as long as its
/// longer side, fitted to `size`.
fn letterbox(image: &Image, size: u32, under: u16) -> Option<Art> {
    let (w, h) = (usize::from(image.width), usize::from(image.height));
    let side = w.max(h);
    if side == 0 {
        return None;
    }
    let mut square = vec![under; side * side];
    let (x0, y0) = ((side - w) / 2, (side - h) / 2);
    for (row, src) in image.pixels.chunks_exact(w).enumerate() {
        let at = (y0 + row) * side + x0;
        square[at..at + w].copy_from_slice(src);
    }
    art::fit(side as u32, side as u32, &square, size)
}

fn internal_free() -> usize {
    // SAFETY: a plain getter.
    unsafe { sys::heap_caps_get_free_size(sys::MALLOC_CAP_INTERNAL) }
}

/// The first picture the folder has, decoded and fitted to `size`.
fn fetch_folder(dir: &Path, size: u32) -> Option<Art> {
    for name in NAMES {
        let path = dir.join(name);
        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        let started = Instant::now();
        let bytes = match read_in_pieces(file) {
            Ok(bytes) => bytes,
            Err(e) => {
                warn!("art: {}: {e}", path.display());
                return None;
            }
        };
        let read = started.elapsed();
        let kb = bytes.len() / 1024;
        let image = match jpeg::decode(&bytes, size) {
            Ok(image) => image,
            Err(e) => {
                warn!("art: {}: {e}", path.display());
                return None;
            }
        };
        drop(bytes);
        let decoded = started.elapsed() - read;
        let art = art::fit(
            u32::from(image.width),
            u32::from(image.height),
            &image.pixels,
            size,
        );
        info!(
            "art: {} {kb} KB read in {} ms, {}x{} at 1/{} decoded in {} ms, fitted to {size} in {} ms",
            path.display(),
            read.as_millis(),
            image.width,
            image.height,
            image.divisor,
            decoded.as_millis(),
            (started.elapsed() - read - decoded).as_millis()
        );
        return art;
    }
    info!("art: nothing in {}", dir.display());
    None
}

/// The whole file, a piece at a time with a breath between, so the
/// music's reads get the bus.
fn read_in_pieces(mut file: std::fs::File) -> std::io::Result<Vec<u8>> {
    let len = file.metadata().map(|m| m.len() as usize).unwrap_or(0);
    let mut bytes = Vec::with_capacity(len);
    let mut piece = vec![0u8; PIECE];
    loop {
        let n = file.read(&mut piece)?;
        if n == 0 {
            return Ok(bytes);
        }
        bytes.extend_from_slice(&piece[..n]);
        std::thread::sleep(BREATH);
    }
}

// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Album art for the face, fetched off the main loop.
//!
//! The picture is the album folder's `cover.jpg`, `folder.jpg`,
//! `albumart.jpg` or `front.jpg` (the card's file system is
//! case-insensitive), read and decoded by one worker thread below the
//! main task's priority, so the decoder's tick is never kept waiting:
//! TJpgDec on a 600-pixel cover is a second or more of core on this
//! chip, and the ring holds a quarter of a second. The file is read in
//! small pieces with a yield between, since the card shares its SPI bus
//! with the music being decoded. Requests go to the worker through a
//! channel and the newest wins; the result comes back through another
//! and the main loop hands it to the face, which repaints the art
//! alone. One album's art is fetched once; the next track from the same
//! folder keeps it.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use esp_idf_svc::hal::task::thread::ThreadSpawnConfiguration;
use esp_idf_svc::sys;
use evo_esp32_face::art::{self, Art};
use evo_esp32_platform_esp_idf::jpeg;
use log::{debug, info, warn};

/// File names tried in the album folder, in order.
const NAMES: [&str; 4] =
    ["cover.jpg", "folder.jpg", "albumart.jpg", "front.jpg"];
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

/// What a fetch found.
pub struct Found {
    /// The album folder it was fetched for.
    pub dir: PathBuf,
    /// The picture at the face's size, or none.
    pub art: Option<Art>,
}

/// Fetches album art, one folder at a time, on its own thread.
pub struct Fetcher {
    requests: Sender<(PathBuf, u32)>,
    results: Receiver<Found>,
    /// The folder whose art the face shows or is fetching.
    current: Option<PathBuf>,
    /// The side of the face's art square.
    size: u32,
}

impl Fetcher {
    /// A fetcher for art squares of `size` pixels. Raises the calling
    /// task's priority so the worker runs beneath it.
    pub fn new(size: u32) -> Result<Self, std::io::Error> {
        // SAFETY: a null handle names the calling task; raising its
        // priority is a plain scheduler call.
        unsafe {
            sys::vTaskPrioritySet(
                core::ptr::null_mut(),
                u32::from(MAIN_PRIORITY),
            );
        }
        let (requests, inbox) = channel::<(PathBuf, u32)>();
        let (outbox, results) = channel::<Found>();
        ThreadSpawnConfiguration {
            name: Some(c"art"),
            stack_size: STACK,
            priority: WORKER_PRIORITY,
            ..Default::default()
        }
        .set()
        .map_err(std::io::Error::other)?;
        std::thread::Builder::new()
            .stack_size(STACK)
            .spawn(move || work(&inbox, &outbox))?;
        ThreadSpawnConfiguration::default()
            .set()
            .map_err(std::io::Error::other)?;
        Ok(Self {
            requests,
            results,
            current: None,
            size,
        })
    }

    /// The face's art square changed size (a style change): fetch the
    /// current folder's art again at the new size. `true` when a fetch
    /// is asked for, which is when the face should drop the old art.
    pub fn resize(&mut self, size: u32) -> bool {
        if size == self.size {
            return false;
        }
        self.size = size;
        match &self.current {
            Some(dir) => self.requests.send((dir.clone(), size)).is_ok(),
            None => false,
        }
    }

    /// The track at `uri` (a `file://` path) is playing: fetch its
    /// folder's art unless that folder's is already in hand. `true` when
    /// a fetch is asked for, which is when the face should drop the old
    /// art.
    pub fn track(&mut self, uri: &str) -> bool {
        let Some(dir) = uri
            .strip_prefix("file://")
            .map(Path::new)
            .and_then(Path::parent)
            .map(Path::to_path_buf)
        else {
            return false;
        };
        if self.current.as_ref() == Some(&dir) {
            return false;
        }
        self.current = Some(dir.clone());
        // The worker is gone only if its thread died, which is logged.
        self.requests.send((dir, self.size)).is_ok()
    }

    /// A finished fetch, if one is waiting; only the one for the current
    /// folder counts, an older one is dropped.
    pub fn poll(&mut self) -> Option<Found> {
        loop {
            match self.results.try_recv() {
                Ok(found) if Some(&found.dir) == self.current.as_ref() => {
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

/// The worker: the newest request wins, the rest are dropped unread.
fn work(inbox: &Receiver<(PathBuf, u32)>, outbox: &Sender<Found>) {
    while let Ok(mut ask) = inbox.recv() {
        while let Ok(newer) = inbox.try_recv() {
            ask = newer;
        }
        let (dir, size) = ask;
        let before = internal_free();
        let art = fetch(&dir, size);
        debug!(
            "art: internal SRAM {before} B before, {} B after",
            internal_free()
        );
        if outbox.send(Found { dir, art }).is_err() {
            return;
        }
    }
    warn!("art: the main loop is gone; worker stopping");
}

fn internal_free() -> usize {
    // SAFETY: a plain getter.
    unsafe { sys::heap_caps_get_free_size(sys::MALLOC_CAP_INTERNAL) }
}

/// The first picture the folder has, decoded and fitted to `size`.
fn fetch(dir: &Path, size: u32) -> Option<Art> {
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

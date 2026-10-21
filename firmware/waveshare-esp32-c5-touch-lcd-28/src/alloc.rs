// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Where the Rust heap lives, by the size of the allocation.
//!
//! The ESP32-C5 has one SRAM for code, stacks, DMA and data, and 8 MB
//! of PSRAM behind the cache. ESP-IDF's own rule sends every `malloc`
//! under 16 KB to SRAM, which fills it with what need not be there: the
//! card's listing, the steward's records, the face's text, thousands of
//! small strings (60 KB of them on a card of fifty tracks). What must be
//! in SRAM asks for it by capability: the I2S ring and the SD card's DMA
//! buffers (the drivers), task stacks (the kernel), the Wi-Fi driver's
//! pools. Rust's heap is placed by size: the small things, which are
//! records and text, go to PSRAM; the medium buffers, which are the
//! decoder's packets and frames and the sink's scratch and are read in
//! tight loops, go to SRAM (measured: the same buffers in PSRAM cost
//! the decoder a third more of the core at 96 kHz FLAC and let the DMA
//! run dry during a Wi-Fi search); the large buffers, the stream's ring
//! and the album art, go to PSRAM as they always did. Each falls back to
//! the other memory when its first choice is out.
//!
//! SRAM is also what the drivers take at run time without asking the
//! Rust heap: the Wi-Fi driver's transmit buffers (1.6 KB each, DMA-
//! capable internal, as many as are in flight), lwIP's and the card's
//! bounce buffer. So the medium band keeps a floor: once internal SRAM
//! is down to [`FLOOR`], a medium allocation goes to PSRAM first, and
//! what the drivers need stays theirs. Measured before the floor: an
//! MP3 track's decoder took internal from 49 KB free to 7 KB, every one
//! of its buffers landing in the band. The heartbeat's `heap:` line
//! reports the band's live bytes in each memory and how often the floor
//! spoke.

use core::alloc::{GlobalAlloc, Layout};
use core::ffi::c_void;
use core::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use esp_idf_svc::sys;

/// The allocator behind `Box`, `Vec`, `String` and the rest.
pub struct Placed;

/// Allocations from this size up to [`LARGE`] are the decoder's and the
/// sink's working buffers: SRAM first.
const MEDIUM: usize = 512;
/// From this size up, PSRAM first: the stream's ring, the art.
const LARGE: usize = 16 * 1024;
/// Internal SRAM kept free of the medium band, for the drivers.
const FLOOR: usize = 32 * 1024;

const SPIRAM: u32 = sys::MALLOC_CAP_SPIRAM | sys::MALLOC_CAP_8BIT;
const INTERNAL: u32 = sys::MALLOC_CAP_INTERNAL | sys::MALLOC_CAP_8BIT;

/// The medium band's live bytes and blocks in each memory, and the
/// allocations the floor sent to PSRAM; for the heartbeat.
static SRAM_BYTES: AtomicUsize = AtomicUsize::new(0);
static SRAM_BLOCKS: AtomicUsize = AtomicUsize::new(0);
static PSRAM_BYTES: AtomicUsize = AtomicUsize::new(0);
static PSRAM_BLOCKS: AtomicUsize = AtomicUsize::new(0);
static DIVERTED: AtomicUsize = AtomicUsize::new(0);

/// What the heartbeat says of the medium band.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Account {
    /// Live bytes of the band in SRAM.
    pub sram_bytes: usize,
    /// Live blocks of the band in SRAM.
    pub sram_blocks: usize,
    /// Live bytes of the band in PSRAM.
    pub psram_bytes: usize,
    /// Live blocks of the band in PSRAM.
    pub psram_blocks: usize,
    /// Medium allocations the floor sent to PSRAM, since boot.
    pub diverted: usize,
}

impl Account {
    /// The account now.
    pub fn now() -> Self {
        Self {
            sram_bytes: SRAM_BYTES.load(Relaxed),
            sram_blocks: SRAM_BLOCKS.load(Relaxed),
            psram_bytes: PSRAM_BYTES.load(Relaxed),
            psram_blocks: PSRAM_BLOCKS.load(Relaxed),
            diverted: DIVERTED.load(Relaxed),
        }
    }

    /// One line for the heartbeat.
    pub fn line(&self) -> String {
        format!(
            "medium band {} KB in {} blocks in SRAM, {} KB in {} in PSRAM, {} sent to PSRAM by the floor",
            self.sram_bytes / 1024,
            self.sram_blocks,
            self.psram_bytes / 1024,
            self.psram_blocks,
            self.diverted
        )
    }
}

/// Whether `p` is in PSRAM, by its address.
fn in_psram(p: *mut u8) -> bool {
    let a = p as usize;
    a >= sys::SOC_EXTRAM_DATA_LOW as usize
        && a < sys::SOC_EXTRAM_DATA_HIGH as usize
}

/// The capabilities tried for `size`, in order.
fn prefer(size: usize) -> [u32; 2] {
    if (MEDIUM..LARGE).contains(&size) {
        // SAFETY: a plain getter; sums the registered heaps' counters.
        let free = unsafe { sys::heap_caps_get_free_size(INTERNAL) };
        if free < FLOOR + size {
            DIVERTED.fetch_add(1, Relaxed);
            [SPIRAM, INTERNAL]
        } else {
            [INTERNAL, SPIRAM]
        }
    } else {
        [SPIRAM, INTERNAL]
    }
}

fn account(p: *mut u8, size: usize, freed: bool) {
    if !(MEDIUM..LARGE).contains(&size) {
        return;
    }
    let (bytes, blocks) = if in_psram(p) {
        (&PSRAM_BYTES, &PSRAM_BLOCKS)
    } else {
        (&SRAM_BYTES, &SRAM_BLOCKS)
    };
    if freed {
        bytes.fetch_sub(size, Relaxed);
        blocks.fetch_sub(1, Relaxed);
    } else {
        bytes.fetch_add(size, Relaxed);
        blocks.fetch_add(1, Relaxed);
    }
}

unsafe impl GlobalAlloc for Placed {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // The heap aligns to 4 on its own; `aligned_alloc` serves any
        // power of two and frees with the same `free`.
        let align = layout.align().max(4);
        for caps in prefer(layout.size()) {
            let p = sys::heap_caps_aligned_alloc(align, layout.size(), caps);
            if !p.is_null() {
                account(p.cast(), layout.size(), false);
                return p.cast();
            }
        }
        core::ptr::null_mut()
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        account(ptr, layout.size(), true);
        sys::heap_caps_free(ptr.cast::<c_void>());
    }
}

// Espressif's audio codecs (the MP3 and AAC decoders) take their working
// memory through four hooks the library defines weak, for an application
// to place as it sees fit; the library's own go to `malloc`, which on
// ESP-IDF means internal SRAM for anything under 16 KB. A decoder's state
// is tens of kilobytes held for the length of a track (measured: MP3's
// took 29 KB of internal, the floor above notwithstanding, since it is
// not the Rust heap). Here they go to PSRAM first. These live in the
// firmware crate, whose objects the linker takes whole, so the strong
// definitions are present before the library's weak ones are met.

/// `module` is the codec's name, for a log the library does not keep.
#[no_mangle]
pub unsafe extern "C" fn media_lib_module_malloc(
    _module: *const core::ffi::c_char,
    size: usize,
) -> *mut c_void {
    let p = sys::heap_caps_malloc(size, SPIRAM);
    if p.is_null() {
        sys::heap_caps_malloc(size, INTERNAL)
    } else {
        p
    }
}

/// As `calloc`, zeroed.
#[no_mangle]
pub unsafe extern "C" fn media_lib_module_calloc(
    _module: *const core::ffi::c_char,
    n: usize,
    size: usize,
) -> *mut c_void {
    let p = sys::heap_caps_calloc(n, size, SPIRAM);
    if p.is_null() {
        sys::heap_caps_calloc(n, size, INTERNAL)
    } else {
        p
    }
}

/// As `realloc`; a block moves to PSRAM when it grows.
#[no_mangle]
pub unsafe extern "C" fn media_lib_module_realloc(
    _module: *const core::ffi::c_char,
    ptr: *mut c_void,
    size: usize,
) -> *mut c_void {
    let p = sys::heap_caps_realloc(ptr, size, SPIRAM);
    if p.is_null() {
        sys::heap_caps_realloc(ptr, size, INTERNAL)
    } else {
        p
    }
}

/// The heap's `free` serves blocks from either memory.
#[no_mangle]
pub unsafe extern "C" fn media_lib_free(ptr: *mut c_void) {
    sys::heap_caps_free(ptr);
}

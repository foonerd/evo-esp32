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
use core::sync::atomic::{
    AtomicBool, AtomicPtr, AtomicUsize, Ordering::Relaxed,
};

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
/// The codecs' live bytes in each memory, through the hooks below.
static CODEC_SRAM: AtomicUsize = AtomicUsize::new(0);
static CODEC_PSRAM: AtomicUsize = AtomicUsize::new(0);

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
    /// The codecs' live bytes in SRAM.
    pub codec_sram: usize,
    /// The codecs' live bytes in PSRAM.
    pub codec_psram: usize,
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
            codec_sram: CODEC_SRAM.load(Relaxed),
            codec_psram: CODEC_PSRAM.load(Relaxed),
        }
    }

    /// One line for the heartbeat.
    pub fn line(&self) -> String {
        format!(
            "medium band {} KB in {} blocks in SRAM, {} KB in {} in PSRAM, {} sent to PSRAM by the floor; codec {} KB in SRAM, {} KB in PSRAM",
            self.sram_bytes / 1024,
            self.sram_blocks,
            self.psram_bytes / 1024,
            self.psram_blocks,
            self.diverted,
            self.codec_sram / 1024,
            self.codec_psram / 1024
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
        under_floor(size)
    } else {
        [SPIRAM, INTERNAL]
    }
}

/// SRAM first while [`FLOOR`] would still be free after `size`, else
/// PSRAM first.
fn under_floor(size: usize) -> [u32; 2] {
    // SAFETY: a plain getter; sums the registered heaps' counters.
    let free = unsafe { sys::heap_caps_get_free_size(INTERNAL) };
    if free < FLOOR + size {
        DIVERTED.fetch_add(1, Relaxed);
        [SPIRAM, INTERNAL]
    } else {
        [INTERNAL, SPIRAM]
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
// ESP-IDF means internal SRAM for anything under 16 KB, outside the
// floor's reach (measured: MP3's state took 29 KB of internal). Here
// they follow the medium band's rule: SRAM while the floor holds, PSRAM
// after, so the decoder's hot state runs from the faster memory as far
// as the drivers can spare it (the same state in PSRAM cost the MP3
// decoder a quarter more of the core). These live in the firmware
// crate, whose objects the linker takes whole, so the strong definitions
// are present before the library's weak ones are met.

fn codec_account(p: *mut c_void, freed: bool) {
    if p.is_null() {
        return;
    }
    // SAFETY: `p` came from heap_caps; the size is the block's own.
    let size = unsafe { sys::heap_caps_get_allocated_size(p) };
    let bytes = if in_psram(p.cast()) {
        &CODEC_PSRAM
    } else {
        &CODEC_SRAM
    };
    if freed {
        bytes.fetch_sub(size, Relaxed);
    } else {
        bytes.fetch_add(size, Relaxed);
    }
}

fn codec_log(module: *const core::ffi::c_char, size: usize, p: *mut c_void) {
    let module = if module.is_null() {
        "?"
    } else {
        // SAFETY: the library passes its own static name.
        unsafe { core::ffi::CStr::from_ptr(module) }
            .to_str()
            .unwrap_or("?")
    };
    log::info!(
        "codec: {module} takes {size} B in {}",
        if p.is_null() {
            "nothing (out of memory)"
        } else if in_psram(p.cast()) {
            "PSRAM"
        } else {
            "SRAM"
        }
    );
}

/// `module` is the codec's name, for a log the library does not keep.
#[no_mangle]
pub unsafe extern "C" fn media_lib_module_malloc(
    module: *const core::ffi::c_char,
    size: usize,
) -> *mut c_void {
    let mut p = core::ptr::null_mut();
    for caps in under_floor(size) {
        p = sys::heap_caps_malloc(size, caps);
        if !p.is_null() {
            break;
        }
    }
    codec_account(p, false);
    codec_log(module, size, p);
    p
}

/// As `calloc`, zeroed.
#[no_mangle]
pub unsafe extern "C" fn media_lib_module_calloc(
    module: *const core::ffi::c_char,
    n: usize,
    size: usize,
) -> *mut c_void {
    let mut p = core::ptr::null_mut();
    for caps in under_floor(n.saturating_mul(size)) {
        p = sys::heap_caps_calloc(n, size, caps);
        if !p.is_null() {
            break;
        }
    }
    codec_account(p, false);
    codec_log(module, n.saturating_mul(size), p);
    p
}

/// As `realloc`: the block is placed again by the rule at its new size.
#[no_mangle]
pub unsafe extern "C" fn media_lib_module_realloc(
    module: *const core::ffi::c_char,
    ptr: *mut c_void,
    size: usize,
) -> *mut c_void {
    codec_account(ptr, true);
    let mut p = core::ptr::null_mut();
    for caps in under_floor(size) {
        p = sys::heap_caps_realloc(ptr, size, caps);
        if !p.is_null() {
            break;
        }
    }
    // A failed realloc leaves the old block where it was.
    codec_account(if p.is_null() { ptr } else { p }, false);
    codec_log(module, size, p);
    p
}

/// The heap's `free` serves blocks from either memory.
#[no_mangle]
pub unsafe extern "C" fn media_lib_free(ptr: *mut c_void) {
    codec_account(ptr, true);
    sys::heap_caps_free(ptr);
}

/// A chunk the card's driver reaches by DMA: internal, DMA-capable,
/// asked for by capability so the band's floor has no say. The ring's
/// 8 KB chunk is in the medium band, and left to the band it landed in
/// SRAM or PSRAM by how much was free the instant a track opened; in
/// PSRAM the driver copies it a block at a time through its bounce
/// buffer, a third of the core, and a 96 kHz FLAC loses its lead (R52:
/// the same album, 9 % of the core for the reader after one boot of
/// 0.1.5 and 35 % after the next). Asked for by capability at each
/// track's start it still failed once internal memory had fragmented
/// behind the network, the page's server and a few hours of tracks
/// (955 dry runs in five minutes at 96 kHz, 2026-10-23), so one chunk's
/// worth is now set aside at boot, when it is certainly there, and lent
/// to each track's ring in turn: [`reserve_card_chunk`].
struct DmaChunk {
    p: *mut u8,
    len: usize,
}

// SAFETY: the memory is owned by this value alone; heap_caps memory is
// not tied to a thread.
unsafe impl Send for DmaChunk {}

impl org_evoframework_playback_symphonia::ahead::Chunk for DmaChunk {
    fn bytes(&mut self) -> &mut [u8] {
        // SAFETY: `p` is `len` bytes of live, aligned allocation.
        unsafe { core::slice::from_raw_parts_mut(self.p, self.len) }
    }
}

impl Drop for DmaChunk {
    fn drop(&mut self) {
        // SAFETY: `p` came from heap_caps and is freed once, here.
        unsafe { sys::heap_caps_free(self.p.cast()) };
    }
}

/// The reserve: internal DMA memory taken once at boot and never freed.
static RESERVE: AtomicPtr<u8> = AtomicPtr::new(core::ptr::null_mut());
static RESERVE_LEN: AtomicUsize = AtomicUsize::new(0);
/// Whether a ring holds the reserve now.
static RESERVE_LENT: AtomicBool = AtomicBool::new(false);
/// Rings reading through the driver's bounce buffer right now: their
/// chunk is not in memory the card's DMA reaches. The heartbeat says so.
static BOUNCING: AtomicUsize = AtomicUsize::new(0);

/// Set `len` bytes of internal DMA memory aside for the card's ring.
/// Called once, early, before the network and the page's server take
/// their share of internal memory; without it every track's chunk is
/// a fresh allocation that may fail into the bounce path.
pub fn reserve_card_chunk(len: usize) {
    let caps =
        sys::MALLOC_CAP_DMA | sys::MALLOC_CAP_INTERNAL | sys::MALLOC_CAP_8BIT;
    // SAFETY: an allocation call with a plain size and capability set.
    let p = unsafe { sys::heap_caps_aligned_alloc(4, len, caps) };
    if p.is_null() {
        log::warn!("ring: no {len} B of internal DMA memory to set aside for the card's chunk");
        return;
    }
    RESERVE_LEN.store(len, Relaxed);
    RESERVE.store(p.cast(), Relaxed);
    log::info!(
        "ring: {len} B of internal DMA memory set aside for the card's chunk"
    );
}

/// How many rings read through the bounce path right now.
pub fn bouncing() -> usize {
    BOUNCING.load(Relaxed)
}

/// The reserve, lent to one ring at a time; given back on drop.
struct Lent {
    p: *mut u8,
    len: usize,
}

// SAFETY: as `DmaChunk`; the lender's flag is what makes it exclusive.
unsafe impl Send for Lent {}

impl org_evoframework_playback_symphonia::ahead::Chunk for Lent {
    fn bytes(&mut self) -> &mut [u8] {
        // SAFETY: `p` is `len` bytes of live, aligned allocation that no
        // other ring holds while the flag is set.
        unsafe { core::slice::from_raw_parts_mut(self.p, self.len) }
    }
}

impl Drop for Lent {
    fn drop(&mut self) {
        RESERVE_LENT.store(false, Relaxed);
    }
}

/// A chunk on the heap, counted while it lives.
struct Bouncing(Box<dyn org_evoframework_playback_symphonia::ahead::Chunk>);

impl org_evoframework_playback_symphonia::ahead::Chunk for Bouncing {
    fn bytes(&mut self) -> &mut [u8] {
        self.0.bytes()
    }
}

impl Drop for Bouncing {
    fn drop(&mut self) {
        BOUNCING.fetch_sub(1, Relaxed);
    }
}

/// Where the card ring's chunks come from: the reserve when it is free
/// and the size is its, else a fresh allocation in internal DMA memory,
/// else the heap and the bounce path; see [`DmaChunk`].
pub fn card_chunk(
    len: usize,
) -> Box<dyn org_evoframework_playback_symphonia::ahead::Chunk> {
    if len == RESERVE_LEN.load(Relaxed)
        && RESERVE_LENT
            .compare_exchange(false, true, Relaxed, Relaxed)
            .is_ok()
    {
        let p = RESERVE.load(Relaxed);
        if !p.is_null() {
            return Box::new(Lent { p, len });
        }
        RESERVE_LENT.store(false, Relaxed);
    }
    let caps =
        sys::MALLOC_CAP_DMA | sys::MALLOC_CAP_INTERNAL | sys::MALLOC_CAP_8BIT;
    // SAFETY: an allocation call with a plain size and capability set.
    let p = unsafe { sys::heap_caps_aligned_alloc(4, len, caps) };
    if p.is_null() {
        log::warn!(
            "ring: no {len} B of DMA memory for the card's chunk; the heap, which may be the slow path"
        );
        BOUNCING.fetch_add(1, Relaxed);
        return Box::new(Bouncing(
            org_evoframework_playback_symphonia::ahead::heap_chunk(len),
        ));
    }
    log::info!("ring: {len} B chunk for the card in internal DMA memory");
    Box::new(DmaChunk { p: p.cast(), len })
}

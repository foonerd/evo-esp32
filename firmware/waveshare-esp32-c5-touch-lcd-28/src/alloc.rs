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
//! pools, the panel's transfer buffer (the board crate). Rust's heap
//! goes to PSRAM, SRAM only when PSRAM is out.
//!
//! Until 0.2.5 the medium band (512 B to 16 KB: the decoder's packets
//! and frames, the sink's scratch) went to SRAM first while a floor of
//! internal memory held, because at the PSRAM's 40 MHz the same buffers
//! cost the decoder a third more of the core at 96 kHz FLAC. Two things
//! changed the sum. The PSRAM runs at 80 MHz, and the decoder's share
//! measured the same with the band in either memory (54% at 96 kHz,
//! 2026-10-31). And the floor measured the wrong pool: it counted all
//! of internal memory, which includes the C5's 16 KB of LP SRAM that DMA
//! cannot reach, so it held while the card and the SPI master could not
//! get a 512 B buffer (37 KB internal free, the largest DMA-capable
//! block 336 B, then a panic inside the SPI master). The band's 30 KB
//! of SRAM was exactly what the drivers were missing. So the band is in
//! PSRAM, and the DMA-capable SRAM is the drivers'. The heartbeat's
//! `heap:` line still reports the band's live bytes in each memory.

use core::alloc::{GlobalAlloc, Layout};
use core::ffi::c_void;
use core::sync::atomic::{
    AtomicBool, AtomicPtr, AtomicUsize, Ordering::Relaxed,
};

use esp_idf_svc::sys;

/// The allocator behind `Box`, `Vec`, `String` and the rest.
pub struct Placed;

/// Allocations from this size up to [`LARGE`] are the decoder's and the
/// sink's working buffers; the heartbeat accounts for them by memory.
const MEDIUM: usize = 512;
/// From this size up: the stream's ring, the art.
const LARGE: usize = 16 * 1024;

const SPIRAM: u32 = sys::MALLOC_CAP_SPIRAM | sys::MALLOC_CAP_8BIT;
const INTERNAL: u32 = sys::MALLOC_CAP_INTERNAL | sys::MALLOC_CAP_8BIT;
/// PSRAM first, SRAM when it is out: every size.
const PREFER: [u32; 2] = [SPIRAM, INTERNAL];

/// The medium band's live bytes and blocks in each memory, for the
/// heartbeat.
static SRAM_BYTES: AtomicUsize = AtomicUsize::new(0);
static SRAM_BLOCKS: AtomicUsize = AtomicUsize::new(0);
static PSRAM_BYTES: AtomicUsize = AtomicUsize::new(0);
static PSRAM_BLOCKS: AtomicUsize = AtomicUsize::new(0);
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
            codec_sram: CODEC_SRAM.load(Relaxed),
            codec_psram: CODEC_PSRAM.load(Relaxed),
        }
    }

    /// One line for the heartbeat.
    pub fn line(&self) -> String {
        format!(
            "medium band {} KB in {} blocks in SRAM, {} KB in {} in PSRAM; codec {} KB in SRAM, {} KB in PSRAM",
            self.sram_bytes / 1024,
            self.sram_blocks,
            self.psram_bytes / 1024,
            self.psram_blocks,
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
        for caps in PREFER {
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
// ESP-IDF means internal SRAM for anything under 16 KB (measured: MP3's
// state took 29 KB of internal). Here they go where the Rust heap goes:
// PSRAM, SRAM when it is out. These live in the firmware crate, whose
// objects the linker takes whole, so the strong definitions are present
// before the library's weak ones are met.

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
    for caps in PREFER {
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
    for caps in PREFER {
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
    for caps in PREFER {
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

/// `len` bytes of DMA-capable internal memory for the life of the
/// firmware, zeroed: for a buffer a driver reaches by DMA (the panel's
/// transfer buffer), so the driver never has to bounce it through one it
/// allocates per transfer. `None` when there is none, which at boot
/// there always is.
pub fn dma_buffer(len: usize) -> Option<&'static mut [u8]> {
    let caps =
        sys::MALLOC_CAP_DMA | sys::MALLOC_CAP_INTERNAL | sys::MALLOC_CAP_8BIT;
    // SAFETY: an allocation call with a plain size and capability set.
    let p = unsafe { sys::heap_caps_aligned_alloc(4, len, caps) };
    if p.is_null() {
        return None;
    }
    // SAFETY: `p` is `len` bytes of live, aligned allocation that nothing
    // else holds and that is never freed.
    let buffer = unsafe { core::slice::from_raw_parts_mut(p.cast(), len) };
    buffer.fill(0);
    Some(buffer)
}

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

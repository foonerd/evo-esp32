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

use core::alloc::{GlobalAlloc, Layout};
use core::ffi::c_void;

use esp_idf_svc::sys;

/// The allocator behind `Box`, `Vec`, `String` and the rest.
pub struct Placed;

/// Allocations from this size up to [`LARGE`] are the decoder's and the
/// sink's working buffers: SRAM first.
const MEDIUM: usize = 512;
/// From this size up, PSRAM first: the stream's ring, the art.
const LARGE: usize = 16 * 1024;

const SPIRAM: u32 = sys::MALLOC_CAP_SPIRAM | sys::MALLOC_CAP_8BIT;
const INTERNAL: u32 = sys::MALLOC_CAP_INTERNAL | sys::MALLOC_CAP_8BIT;

/// The capabilities tried for `size`, in order.
fn prefer(size: usize) -> [u32; 2] {
    if (MEDIUM..LARGE).contains(&size) {
        [INTERNAL, SPIRAM]
    } else {
        [SPIRAM, INTERNAL]
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
                return p.cast();
            }
        }
        core::ptr::null_mut()
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        sys::heap_caps_free(ptr.cast::<c_void>());
    }
}

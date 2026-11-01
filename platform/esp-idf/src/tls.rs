// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Thread-local destructors in POSIX order, so a thread that exits gives
//! its handle back.
//!
//! Rust's standard library keeps the current thread's handle in a
//! pthread key without a destructor and drops it from a second key's
//! destructor, deferred to a second pass (`DEFER`, then `RUN`): POSIX
//! says a destructor pass touches only keys that have destructors and
//! leaves the rest readable until the thread is gone. ESP-IDF's pass
//! (`pthread_local_storage.c`) walks the thread's values once, head to
//! tail, calling what destructors there are and freeing every entry as
//! it goes; the handle's entry is freed before the deferred pass asks
//! for it, so the handle, its parker's condition variable and the
//! kernel mutex under it — 168 bytes of internal SRAM, allocated through
//! the system allocator — outlive every thread that ever exits. On a
//! player that opens a thread or two per track that is a working day of
//! internal SRAM (measured 2026-11-01: ~350 B a track; the heap trace
//! named `Thread::new` and `pthread_cond_init` under it).
//!
//! The linker's `--wrap` puts this module between ESP-IDF's pthreads
//! and its storage: `pthread_key_create` and `pthread_key_delete` are
//! watched so the destructors are known, and the exit pass is run the
//! POSIX way first — up to four rounds over the keys with destructors,
//! each value cleared before its destructor is called, values without
//! destructors untouched — before ESP-IDF's own pass frees what is
//! left. The firmware's `build.rs` carries the three `--wrap` flags; the
//! firmware checks the effect at boot with a thread that exits.
//!
//! Both halves are upstream matters (Espressif's pass against POSIX,
//! Rust's reliance on it for `espidf`); this is the player's own fix
//! until either lands.

use core::ffi::c_void;
use core::sync::atomic::{AtomicUsize, Ordering};

use esp_idf_svc::sys;

type Destructor = unsafe extern "C" fn(*mut c_void);

/// Keys with destructors the firmware can have at once. Rust's runtime
/// makes a handful; ESP-IDF's pthread one; each `thread_local!` with a
/// `Drop` type one more.
const KEYS: usize = 32;
/// POSIX's `PTHREAD_DESTRUCTOR_ITERATIONS`.
const ROUNDS: usize = 4;

static KEY: [AtomicUsize; KEYS] = [const { AtomicUsize::new(0) }; KEYS];
static DESTRUCTOR: [AtomicUsize; KEYS] = [const { AtomicUsize::new(0) }; KEYS];

extern "C" {
    fn __real_pthread_key_create(
        key: *mut u32,
        destructor: Option<Destructor>,
    ) -> i32;
    fn __real_pthread_key_delete(key: u32) -> i32;
    fn __real_pthread_internal_local_storage_destructor_callback(
        handle: sys::TaskHandle_t,
    );
    fn pthread_getspecific(key: u32) -> *mut c_void;
    fn pthread_setspecific(key: u32, value: *const c_void) -> i32;
}

/// `pthread_key_create`, remembering the destructor.
///
/// # Safety
/// Called by the C runtime with its own arguments.
#[no_mangle]
pub unsafe extern "C" fn __wrap_pthread_key_create(
    key: *mut u32,
    destructor: Option<Destructor>,
) -> i32 {
    let r = unsafe { __real_pthread_key_create(key, destructor) };
    if r == 0 {
        if let Some(d) = destructor {
            // SAFETY: a zero return means `key` was written.
            remember(unsafe { *key }, d);
        }
    }
    r
}

/// `pthread_key_delete`, forgetting the destructor: ESP-IDF hands out
/// the newest id again.
///
/// # Safety
/// Called by the C runtime with its own arguments.
#[no_mangle]
pub unsafe extern "C" fn __wrap_pthread_key_delete(key: u32) -> i32 {
    forget(key);
    unsafe { __real_pthread_key_delete(key) }
}

/// ESP-IDF's thread-local clean-up, with the POSIX pass run first for
/// the exiting thread itself (a null handle). For another task's
/// storage (a join or detach after the thread has gone) there is
/// nothing left to read and ESP-IDF's pass runs alone, as before.
///
/// # Safety
/// Called by ESP-IDF's pthreads at thread exit, join and detach.
#[no_mangle]
pub unsafe extern "C" fn __wrap_pthread_internal_local_storage_destructor_callback(
    handle: sys::TaskHandle_t,
) {
    if handle.is_null() {
        for _ in 0..ROUNDS {
            let mut called = false;
            for i in 0..KEYS {
                let key = KEY[i].load(Ordering::Acquire) as u32;
                if key == 0 {
                    continue;
                }
                // SAFETY: plain pthread calls on the current thread.
                let value = unsafe { pthread_getspecific(key) };
                if value.is_null() {
                    continue;
                }
                let destructor = DESTRUCTOR[i].load(Ordering::Acquire);
                if destructor == 0 {
                    continue;
                }
                unsafe { pthread_setspecific(key, core::ptr::null()) };
                // SAFETY: stored from a `Destructor` in `remember`.
                let destructor: Destructor =
                    unsafe { core::mem::transmute(destructor) };
                unsafe { destructor(value) };
                called = true;
            }
            if !called {
                break;
            }
        }
    }
    unsafe { __real_pthread_internal_local_storage_destructor_callback(handle) }
}

fn remember(key: u32, destructor: Destructor) {
    for i in 0..KEYS {
        if KEY[i]
            .compare_exchange(
                0,
                key as usize,
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_ok()
        {
            DESTRUCTOR[i].store(destructor as usize, Ordering::Release);
            return;
        }
    }
    log::warn!(
        "threads: more than {KEYS} thread-local keys with destructors; key {key} runs in ESP-IDF's order"
    );
}

fn forget(key: u32) {
    for i in 0..KEYS {
        if KEY[i].load(Ordering::Acquire) as u32 == key {
            DESTRUCTOR[i].store(0, Ordering::Release);
            KEY[i].store(0, Ordering::Release);
        }
    }
}

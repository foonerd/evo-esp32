// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The key the player seals the card's passwords with, and the
//! randomness the sealing needs.
//!
//! The key is 32 bytes made once from the hardware random source and
//! kept in NVS under its own namespace: an OTA update keeps it, a flash
//! from nothing (`write-flash 0x0`) loses it along with the rest of NVS,
//! and the passwords on the card then have to be written again. That is
//! the deal the release notes state for a full flash.

use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sys;
use evo_esp32_config::seal::{KEY_LEN, NONCE_LEN};

use crate::nvs::{NvsError, NvsStore};

const NAMESPACE: &str = "seal";
const KEY: &str = "key";

/// The player's sealing key: read from NVS, or made and stored on the
/// first call after a flash from nothing.
pub fn device_key(
    partition: EspDefaultNvsPartition,
) -> Result<[u8; KEY_LEN], NvsError> {
    let mut store = NvsStore::open(partition, NAMESPACE)?;
    if let Some(hex) = store.get(KEY)? {
        if let Some(key) = parse_hex(&hex) {
            return Ok(key);
        }
        log::warn!("seal: the stored key is not {KEY_LEN} bytes of hex; a new one is made");
    }
    let key: [u8; KEY_LEN] = random();
    let mut hex = String::with_capacity(2 * KEY_LEN);
    for b in key {
        hex.push_str(&format!("{b:02x}"));
    }
    store.set(KEY, &hex)?;
    log::info!("seal: a new key made for this player");
    Ok(key)
}

/// A fresh nonce from the hardware random source.
pub fn nonce() -> [u8; NONCE_LEN] {
    random()
}

fn random<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    // SAFETY: a buffer of its own length; the radio or the bootloader's
    // entropy source is up by the time anything is sealed.
    unsafe { sys::esp_fill_random(out.as_mut_ptr().cast(), N) };
    out
}

fn parse_hex(hex: &str) -> Option<[u8; KEY_LEN]> {
    if hex.len() != 2 * KEY_LEN {
        return None;
    }
    let mut out = [0u8; KEY_LEN];
    for (i, pair) in hex.as_bytes().chunks(2).enumerate() {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out[i] = (hi * 16 + lo) as u8;
    }
    Some(out)
}

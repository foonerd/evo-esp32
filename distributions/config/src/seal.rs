// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Passwords on the card, sealed to the player.
//!
//! A player is small and its card comes out: a password written in
//! `evoconfig.txt` in plain text is read by anyone who takes it. So the
//! player seals every password it finds in plain text at its first boot
//! and writes the file back, the password replaced by
//! `sealed:1:<hex>` — AES-256-GCM under a key the player made for itself
//! and keeps in its own flash, with the setting's name as the associated
//! data so a sealed value cannot be moved from one key to another. The
//! card then holds nothing a reader can use without the player it came
//! from. A user who changes a password writes it in plain text again
//! and the player seals it again. A player flashed from nothing makes a
//! new key and cannot open the old seals; it says so and asks for the
//! passwords again. This is a lock on the card, not a vault: the key
//! lives in flash the device can read, and a determined attacker with
//! the device and its card has the means to reach it.
//!
//! The crate does the sealing; the device provides the key and the
//! randomness ([`Sealer::new`], the nonce each [`Sealer::seal`] takes).

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};

use crate::set;

/// A sealed value begins with this.
pub const SEALED_PREFIX: &str = "sealed:1:";
/// The key's length in bytes.
pub const KEY_LEN: usize = 32;
/// The nonce's length in bytes; one per sealing, never repeated under a
/// key, which random bytes achieve for a file of a few passwords.
pub const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;

/// Whether `key` (lower case) holds a password: `wifi_password`,
/// `web_password`, `share_password` and `share2_password` to
/// `share9_password`.
pub fn is_secret_key(key: &str) -> bool {
    match key {
        "wifi_password" | "web_password" => true,
        _ => key
            .strip_prefix("share")
            .and_then(|rest| rest.strip_suffix("_password"))
            .is_some_and(|n| {
                n.is_empty()
                    || (n.len() == 1 && n.as_bytes()[0].is_ascii_digit())
            }),
    }
}

/// Whether `value` is a sealed value rather than plain text.
pub fn is_sealed(value: &str) -> bool {
    value.starts_with(SEALED_PREFIX)
}

/// Why a sealed value did not open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unsealable {
    /// Not a sealed value at all.
    NotSealed,
    /// The text after the prefix is not what the player writes.
    Malformed,
    /// Sealed under another key: another player, or this one before a
    /// flash from nothing. Or moved from another setting's line.
    AnotherKey,
}

/// Seals and opens under one key.
pub struct Sealer {
    cipher: Aes256Gcm,
}

impl Sealer {
    /// A sealer under `key`.
    pub fn new(key: &[u8; KEY_LEN]) -> Self {
        Self {
            cipher: Aes256Gcm::new(key.into()),
        }
    }

    /// `plain` sealed for the setting `key_name`, with `nonce` fresh.
    pub fn seal(
        &self,
        key_name: &str,
        plain: &str,
        nonce: &[u8; NONCE_LEN],
    ) -> String {
        let payload = Payload {
            msg: plain.as_bytes(),
            aad: key_name.as_bytes(),
        };
        let sealed = self
            .cipher
            .encrypt(Nonce::from_slice(nonce), payload)
            .expect("AES-GCM encryption does not fail for these lengths");
        let mut out = String::with_capacity(
            SEALED_PREFIX.len() + 2 * (NONCE_LEN + sealed.len()),
        );
        out.push_str(SEALED_PREFIX);
        hex_push(&mut out, nonce);
        hex_push(&mut out, &sealed);
        out
    }

    /// The plain text of `sealed`, written for the setting `key_name`.
    pub fn open(
        &self,
        key_name: &str,
        sealed: &str,
    ) -> Result<String, Unsealable> {
        let hex = sealed
            .strip_prefix(SEALED_PREFIX)
            .ok_or(Unsealable::NotSealed)?;
        let bytes = hex_parse(hex).ok_or(Unsealable::Malformed)?;
        if bytes.len() < NONCE_LEN + TAG_LEN {
            return Err(Unsealable::Malformed);
        }
        let (nonce, body) = bytes.split_at(NONCE_LEN);
        let payload = Payload {
            msg: body,
            aad: key_name.as_bytes(),
        };
        let plain = self
            .cipher
            .decrypt(Nonce::from_slice(nonce), payload)
            .map_err(|_| Unsealable::AnotherKey)?;
        String::from_utf8(plain).map_err(|_| Unsealable::Malformed)
    }
}

/// The file's text with every password still in plain text sealed, and
/// how many were; `None` when nothing needed sealing. `nonce` is asked
/// for one fresh nonce per password. Empty values (an open network) are
/// not passwords and stay as written.
pub fn seal_text(
    text: &str,
    sealer: &Sealer,
    mut nonce: impl FnMut() -> [u8; NONCE_LEN],
) -> Option<(String, usize)> {
    let mut sealed = Vec::new();
    for raw in text.lines() {
        let line = raw.trim().trim_start_matches('\u{feff}');
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = crate::unquote(value.trim());
        if !is_secret_key(&key) || value.is_empty() || is_sealed(value) {
            continue;
        }
        if sealed.iter().any(|(k, _)| *k == key) {
            // `set` keeps the first line of a repeated key and drops the
            // rest; the first is the one sealed.
            continue;
        }
        sealed.push((key.clone(), sealer.seal(&key, value, &nonce())));
    }
    if sealed.is_empty() {
        return None;
    }
    let count = sealed.len();
    let mut out = text.to_string();
    for (key, value) in sealed {
        out = set(&out, &key, &value);
    }
    Some((out, count))
}

/// A password's value as the settings carry it, opened if sealed.
/// Returns the setting's name when a sealed value would not open, and
/// clears it, so a password the player cannot use is not tried.
fn open_one(
    sealer: &Sealer,
    key_name: &str,
    value: &mut Option<String>,
) -> Option<(String, Unsealable)> {
    let v = value.as_deref()?;
    if !is_sealed(v) {
        return None;
    }
    match sealer.open(key_name, v) {
        Ok(plain) => {
            *value = Some(plain);
            None
        }
        Err(why) => {
            *value = None;
            Some((key_name.to_string(), why))
        }
    }
}

impl crate::Settings {
    /// Open every sealed password in place. The settings named in the
    /// result did not open and are cleared: sealed by another player,
    /// or by this one before a flash from nothing.
    pub fn open_secrets(
        &mut self,
        sealer: &Sealer,
    ) -> Vec<(String, Unsealable)> {
        let mut failed = Vec::new();
        failed.extend(open_one(
            sealer,
            "wifi_password",
            &mut self.wifi_password,
        ));
        failed.extend(open_one(sealer, "web_password", &mut self.web_password));
        for share in &mut self.shares {
            let key_name = if share.index <= 1 {
                "share_password".to_string()
            } else {
                alloc::format!("share{}_password", share.index)
            };
            failed.extend(open_one(sealer, &key_name, &mut share.password));
        }
        failed
    }
}

fn hex_push(out: &mut String, bytes: &[u8]) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for b in bytes {
        out.push(DIGITS[usize::from(b >> 4)] as char);
        out.push(DIGITS[usize::from(b & 0xF)] as char);
    }
}

fn hex_parse(hex: &str) -> Option<Vec<u8>> {
    if hex.len() % 2 != 0 {
        return None;
    }
    hex.as_bytes()
        .chunks(2)
        .map(|pair| {
            let hi = (pair[0] as char).to_digit(16)?;
            let lo = (pair[1] as char).to_digit(16)?;
            Some((hi * 16 + lo) as u8)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; KEY_LEN] = [7; KEY_LEN];
    const OTHER: [u8; KEY_LEN] = [9; KEY_LEN];

    fn nonces() -> impl FnMut() -> [u8; NONCE_LEN] {
        let mut n = 0u8;
        move || {
            n += 1;
            [n; NONCE_LEN]
        }
    }

    #[test]
    fn a_sealed_password_opens_under_its_key_and_name() {
        let sealer = Sealer::new(&KEY);
        let s = sealer.seal("wifi_password", "hunter2 #1", &[1; NONCE_LEN]);
        assert!(is_sealed(&s));
        assert_eq!(sealer.open("wifi_password", &s).unwrap(), "hunter2 #1");
        assert_eq!(
            sealer.open("web_password", &s),
            Err(Unsealable::AnotherKey),
            "moved to another setting's line"
        );
        assert_eq!(
            Sealer::new(&OTHER).open("wifi_password", &s),
            Err(Unsealable::AnotherKey)
        );
        assert_eq!(
            sealer.open("wifi_password", "plain"),
            Err(Unsealable::NotSealed)
        );
        assert_eq!(
            sealer.open("wifi_password", "sealed:1:zz"),
            Err(Unsealable::Malformed)
        );
        assert_eq!(
            sealer.open("wifi_password", "sealed:1:0011"),
            Err(Unsealable::Malformed)
        );
    }

    #[test]
    fn the_secret_keys_are_the_three_password_families() {
        for k in [
            "wifi_password",
            "web_password",
            "share_password",
            "share2_password",
            "share9_password",
        ] {
            assert!(is_secret_key(k), "{k}");
        }
        for k in [
            "wifi_ssid",
            "share_user",
            "share10_password",
            "sharex_password",
            "password",
        ] {
            assert!(!is_secret_key(k), "{k}");
        }
    }

    #[test]
    fn seal_text_seals_plain_passwords_once_and_leaves_the_rest() {
        let sealer = Sealer::new(&KEY);
        let text = "# evo\r\nwifi_ssid = Home\r\nwifi_password = \"pass word\"\r\nweb_password =\r\nshare = smb://nas/Music\r\nshare_password = s3cret\r\nvolume = 40\r\n";
        let (out, n) = seal_text(text, &sealer, nonces()).unwrap();
        assert_eq!(n, 2);
        assert!(out.contains("wifi_ssid = Home\r\n"));
        assert!(
            out.contains("web_password =\r\n"),
            "an empty password is not sealed"
        );
        assert!(out.contains("volume = 40\r\n"));
        assert!(!out.contains("pass word") && !out.contains("s3cret"));
        let parsed = crate::parse(&out);
        assert!(parsed.is_clean(), "{parsed:?}");
        let mut settings = parsed.settings;
        assert!(settings.open_secrets(&sealer).is_empty());
        assert_eq!(settings.wifi_password.as_deref(), Some("pass word"));
        assert_eq!(settings.shares[0].password.as_deref(), Some("s3cret"));
        assert!(
            seal_text(&out, &sealer, nonces()).is_none(),
            "nothing left to seal"
        );
    }

    #[test]
    fn a_seal_from_another_player_is_cleared_and_named() {
        let text = "wifi_ssid = Home\nwifi_password = pw\n";
        let (out, _) = seal_text(text, &Sealer::new(&OTHER), nonces()).unwrap();
        let mut settings = crate::parse(&out).settings;
        let failed = settings.open_secrets(&Sealer::new(&KEY));
        assert_eq!(
            failed,
            alloc::vec![("wifi_password".to_string(), Unsealable::AnotherKey)]
        );
        assert_eq!(settings.wifi_password, None);
        assert_eq!(settings.wifi_ssid.as_deref(), Some("Home"));
    }
}

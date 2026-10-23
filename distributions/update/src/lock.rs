// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The page's lock: HTTP Digest authentication (RFC 7616, `MD5` and
//! `qop=auth`), which every browser speaks with its own name-and-
//! password prompt and which never sends the password itself: the
//! browser proves it knows the password by hashing it with a nonce the
//! player issued, and what crosses the Wi-Fi is that one-time hash.
//! Plain HTTP on a home network is what this page is; Basic auth or a
//! login form would carry the password in clear on every request.
//!
//! A nonce is the time it was issued and a hash of that time with a
//! secret the player draws once per boot; it is good for a few minutes,
//! after which the browser is asked again with `stale=true` and retries
//! by itself. What this does not do: stop a replay within a nonce's
//! life (the `nc` counter is parsed, not tracked), or hide the page's
//! contents from a sniffer — it guards the buttons against a guest on
//! the network, no more. Firmware integrity is the image's signature.

use md5::{Digest as _, Md5};

/// The realm the browser shows beside its prompt.
pub const REALM: &str = "evo-esp32";
/// How long a nonce is accepted after it was issued, seconds.
pub const NONCE_LIFE: u64 = 300;

/// The lock: who may use the page, and the secret the nonces hash.
#[derive(Clone)]
pub struct Lock {
    user: String,
    /// `MD5(user:realm:password)`, the only form the password is kept in.
    ha1: String,
    secret: [u8; 16],
}

/// Why a request was not let through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// No credentials, or wrong ones: the browser is asked.
    Ask,
    /// Right credentials with a nonce past its life: asked again, and
    /// the browser retries without troubling the user.
    Stale,
}

impl Lock {
    /// A lock for `user` with `password`; `secret` is random, drawn once
    /// per boot, and never leaves the player.
    pub fn new(user: &str, password: &str, secret: [u8; 16]) -> Self {
        Self {
            user: user.to_string(),
            ha1: hex(Md5::digest(format!("{user}:{REALM}:{password}"))),
            secret,
        }
    }

    /// A fresh nonce at `now` (seconds, any monotonic or wall clock the
    /// caller also passes to [`Lock::check`]).
    pub fn nonce(&self, now: u64) -> String {
        format!("{now:x}-{}", self.stamp(now))
    }

    fn stamp(&self, issued: u64) -> String {
        let mut h = Md5::new();
        h.update(issued.to_le_bytes());
        h.update(b":");
        h.update(self.secret);
        hex(h.finalize())
    }

    /// The `WWW-Authenticate` header for a refusal at `now`.
    pub fn challenge(&self, now: u64, refusal: Refusal) -> String {
        format!(
            "Digest realm=\"{REALM}\", qop=\"auth\", algorithm=MD5, nonce=\"{}\"{}",
            self.nonce(now),
            if refusal == Refusal::Stale {
                ", stale=true"
            } else {
                ""
            }
        )
    }

    /// Whether `authorization` (the request's header, if any) proves the
    /// password for `method` on `uri` at `now`.
    pub fn check(
        &self,
        method: &str,
        uri: &str,
        authorization: Option<&str>,
        now: u64,
    ) -> Result<(), Refusal> {
        let header = authorization.ok_or(Refusal::Ask)?;
        let fields = header
            .strip_prefix("Digest ")
            .or_else(|| header.strip_prefix("digest "))
            .ok_or(Refusal::Ask)?;
        let fields = parse_fields(fields);
        let get = |k: &str| {
            fields
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.as_str())
        };
        let (Some(user), Some(nonce), Some(response), Some(digest_uri)) =
            (get("username"), get("nonce"), get("response"), get("uri"))
        else {
            return Err(Refusal::Ask);
        };
        if user != self.user
            || !constant_eq(digest_uri, uri) && !uri.starts_with(digest_uri)
        {
            return Err(Refusal::Ask);
        }
        // The nonce must be one of ours.
        let Some((issued_hex, stamp)) = nonce.split_once('-') else {
            return Err(Refusal::Ask);
        };
        let Ok(issued) = u64::from_str_radix(issued_hex, 16) else {
            return Err(Refusal::Ask);
        };
        if !constant_eq(stamp, &self.stamp(issued)) {
            return Err(Refusal::Ask);
        }
        let ha2 = hex(Md5::digest(format!("{method}:{digest_uri}")));
        let expected = match (get("qop"), get("nc"), get("cnonce")) {
            (Some(qop), Some(nc), Some(cnonce))
                if qop.eq_ignore_ascii_case("auth") =>
            {
                hex(Md5::digest(format!(
                    "{}:{nonce}:{nc}:{cnonce}:auth:{ha2}",
                    self.ha1
                )))
            }
            (None, _, _) => {
                hex(Md5::digest(format!("{}:{nonce}:{ha2}", self.ha1)))
            }
            _ => return Err(Refusal::Ask),
        };
        if !constant_eq(&expected, &response.to_ascii_lowercase()) {
            return Err(Refusal::Ask);
        }
        if now < issued || now - issued > NONCE_LIFE {
            return Err(Refusal::Stale);
        }
        Ok(())
    }
}

/// `k=v, k="v", ...` as the header carries them.
fn parse_fields(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = s.trim();
    while !rest.is_empty() {
        let Some(eq) = rest.find('=') else { break };
        let key = rest[..eq].trim().to_ascii_lowercase();
        rest = rest[eq + 1..].trim_start();
        let value;
        if let Some(r) = rest.strip_prefix('"') {
            let Some(end) = r.find('"') else { break };
            value = r[..end].to_string();
            rest = r[end + 1..].trim_start();
        } else {
            let end = rest.find(',').unwrap_or(rest.len());
            value = rest[..end].trim().to_string();
            rest = &rest[end..];
        }
        out.push((key, value));
        rest = rest.strip_prefix(',').unwrap_or(rest).trim_start();
    }
    out
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// Equal without an early exit on the first differing byte.
fn constant_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lock() -> Lock {
        Lock::new("andrea", "s3cret", [7u8; 16])
    }

    /// What a browser would send back for the challenge.
    fn answer(
        lock: &Lock,
        nonce: &str,
        method: &str,
        uri: &str,
        password: &str,
    ) -> String {
        let ha1 = hex(Md5::digest(format!("andrea:{REALM}:{password}")));
        let ha2 = hex(Md5::digest(format!("{method}:{uri}")));
        let response = hex(Md5::digest(format!(
            "{ha1}:{nonce}:00000001:abcdef:auth:{ha2}"
        )));
        let _ = lock;
        format!(
            "Digest username=\"andrea\", realm=\"{REALM}\", nonce=\"{nonce}\", uri=\"{uri}\", \
             qop=auth, nc=00000001, cnonce=\"abcdef\", response=\"{response}\", algorithm=MD5"
        )
    }

    #[test]
    fn the_right_password_opens_and_the_wrong_one_asks() {
        let l = lock();
        let nonce = l.nonce(1000);
        let good = answer(&l, &nonce, "POST", "/update", "s3cret");
        assert_eq!(l.check("POST", "/update", Some(&good), 1010), Ok(()));
        let bad = answer(&l, &nonce, "POST", "/update", "guess");
        assert_eq!(
            l.check("POST", "/update", Some(&bad), 1010),
            Err(Refusal::Ask)
        );
        assert_eq!(l.check("GET", "/", None, 1010), Err(Refusal::Ask));
        // The right answer for another method or path is not this one.
        assert_eq!(
            l.check("GET", "/update", Some(&good), 1010),
            Err(Refusal::Ask)
        );
    }

    #[test]
    fn an_old_nonce_is_stale_and_a_forged_one_is_refused() {
        let l = lock();
        let nonce = l.nonce(1000);
        let good = answer(&l, &nonce, "GET", "/", "s3cret");
        assert_eq!(
            l.check("GET", "/", Some(&good), 1000 + NONCE_LIFE + 1),
            Err(Refusal::Stale)
        );
        let forged = answer(&l, "3e8-0000", "GET", "/", "s3cret");
        assert_eq!(l.check("GET", "/", Some(&forged), 1001), Err(Refusal::Ask));
        let other = Lock::new("andrea", "s3cret", [9u8; 16]);
        assert_eq!(
            other.check("GET", "/", Some(&good), 1001),
            Err(Refusal::Ask)
        );
    }

    #[test]
    fn the_challenge_names_the_realm_and_says_stale() {
        let l = lock();
        let c = l.challenge(42, Refusal::Ask);
        assert!(c.starts_with("Digest realm=\"evo-esp32\", qop=\"auth\", algorithm=MD5, nonce=\"2a-"));
        assert!(!c.contains("stale"));
        assert!(l.challenge(42, Refusal::Stale).ends_with(", stale=true"));
    }

    #[test]
    fn header_fields_parse_quoted_and_bare() {
        let f = parse_fields(
            "username=\"a, b\", nc=00000001, qop=auth,response=\"x\"",
        );
        assert_eq!(
            f,
            vec![
                ("username".into(), "a, b".into()),
                ("nc".into(), "00000001".into()),
                ("qop".into(), "auth".into()),
                ("response".into(), "x".into()),
            ]
        );
    }
}

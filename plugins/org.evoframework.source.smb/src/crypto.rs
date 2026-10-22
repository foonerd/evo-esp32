//! The cryptography SMB 3.1.1 adds over 2.1: the SHA-512 chain of
//! pre-authentication integrity, the SP800-108 key derivation, AES-CMAC
//! signing and AES-128-GCM sealing. All in software here (RustCrypto);
//! one place to swap for a platform's hardware when a measurement
//! calls for it.

use aes::Aes128;
use aes_gcm::aead::{AeadInPlace, KeyInit};
use aes_gcm::{Aes128Gcm, Nonce, Tag};
use cmac::{Cmac, Mac};
use hmac::Hmac;
use sha2::{Digest, Sha256, Sha512};

/// The pre-authentication integrity hash: 64 bytes, zero at the start
/// of a connection, folded over every negotiate and session-setup
/// message in turn.
pub type Preauth = [u8; 64];

/// `H = SHA-512(H || message)`.
pub fn fold(hash: &mut Preauth, message: &[u8]) {
    let mut h = Sha512::new();
    h.update(*hash);
    h.update(message);
    hash.copy_from_slice(&h.finalize());
}

/// SP800-108 in counter mode with HMAC-SHA256, one 128-bit block:
/// `K = HMAC(key, 1 || label || 0 || context || 128)`, the label with
/// its terminating zero as [MS-SMB2] gives it.
pub fn derive(key: &[u8], label: &[u8], context: &[u8]) -> [u8; 16] {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key)
        .expect("HMAC takes any key length");
    mac.update(&1u32.to_be_bytes());
    mac.update(label);
    mac.update(&[0u8]);
    mac.update(context);
    mac.update(&128u32.to_be_bytes());
    let full = mac.finalize().into_bytes();
    let mut out = [0u8; 16];
    out.copy_from_slice(&full[..16]);
    out
}

/// AES-128-CMAC over `msg`, the whole 16 bytes.
pub fn cmac(key: &[u8; 16], parts: &[&[u8]]) -> [u8; 16] {
    let mut mac = <Cmac<Aes128> as Mac>::new_from_slice(key)
        .expect("CMAC takes a 16-byte key");
    for p in parts {
        mac.update(p);
    }
    mac.finalize().into_bytes().into()
}

/// AES-128-GCM: seal `data` in place with `nonce` and `aad`; the tag.
pub fn seal(
    key: &[u8; 16],
    nonce: &[u8; 12],
    aad: &[u8],
    data: &mut [u8],
) -> [u8; 16] {
    let cipher = Aes128Gcm::new(key.into());
    let tag = cipher
        .encrypt_in_place_detached(Nonce::from_slice(nonce), aad, data)
        .expect("GCM over a message shorter than its limit");
    tag.into()
}

/// AES-128-GCM: open `data` in place; false when the tag does not
/// verify, and `data` is then not to be trusted.
pub fn open(
    key: &[u8; 16],
    nonce: &[u8; 12],
    aad: &[u8],
    data: &mut [u8],
    tag: &[u8; 16],
) -> bool {
    let cipher = Aes128Gcm::new(key.into());
    cipher
        .decrypt_in_place_detached(
            Nonce::from_slice(nonce),
            aad,
            data,
            Tag::from_slice(tag),
        )
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// RFC 4493 example 1: AES-CMAC of the empty message.
    #[test]
    fn cmac_matches_rfc_4493() {
        let key: [u8; 16] =
            hex("2b7e151628aed2a6abf7158809cf4f3c").try_into().unwrap();
        assert_eq!(
            cmac(&key, &[]).to_vec(),
            hex("bb1d6929e95937287fa37d129b756746")
        );
        // Example 2: a 16-byte message.
        let msg = hex("6bc1bee22e409f96e93d7e117393172a");
        assert_eq!(
            cmac(&key, &[&msg]).to_vec(),
            hex("070a16b46b4d4144f79bdd9dd04a287c")
        );
    }

    /// NIST GCM test case 1: zero key, zero nonce, empty everything.
    #[test]
    fn gcm_matches_nist() {
        let key = [0u8; 16];
        let nonce = [0u8; 12];
        let mut data = Vec::new();
        let tag = seal(&key, &nonce, &[], &mut data);
        assert_eq!(tag.to_vec(), hex("58e2fccefa7e3061367f1d57a4e7455a"));
        // Test case 2: 16 zero bytes of plaintext.
        let mut data = vec![0u8; 16];
        let tag = seal(&key, &nonce, &[], &mut data);
        assert_eq!(data, hex("0388dace60b6a392f328c2b971b2fe78"));
        assert_eq!(tag.to_vec(), hex("ab6e47d42cec13bdf53a67b21257bddf"));
        assert!(open(&key, &nonce, &[], &mut data, &tag));
        assert_eq!(data, vec![0u8; 16]);
        let mut bad = tag;
        bad[0] ^= 1;
        assert!(!open(&key, &nonce, &[], &mut data, &bad));
    }

    #[test]
    fn preauth_folds_and_derive_is_deterministic() {
        let mut h = [0u8; 64];
        fold(&mut h, b"negotiate");
        let after_one = h;
        fold(&mut h, b"session setup");
        assert_ne!(h, after_one);
        assert_ne!(h, [0u8; 64]);
        // The signing key differs from the cipher keys by label alone.
        let key = [7u8; 16];
        let a = derive(&key, b"SMBSigningKey\0", &h);
        let b = derive(&key, b"SMBC2SCipherKey\0", &h);
        let c = derive(&key, b"SMBS2CCipherKey\0", &h);
        assert!(a != b && b != c && a != c);
        assert_eq!(a, derive(&key, b"SMBSigningKey\0", &h));
    }
}

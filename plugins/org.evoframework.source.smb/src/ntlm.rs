//! NTLMv2 ([MS-NLMP]): the three messages and the proof the client
//! makes of its password without sending it, with the MIC over the
//! exchange. NTLMv1 and LM are not spoken.

use hmac::{Hmac, Mac};
use md4::{Digest, Md4};
use md5::Md5;

use crate::wire::{get16, get32, put16, put32, utf16le};

type HmacMd5 = Hmac<Md5>;

const SIGNATURE: &[u8; 8] = b"NTLMSSP\0";

const UNICODE: u32 = 0x0000_0001;
const REQUEST_TARGET: u32 = 0x0000_0004;
const SIGN: u32 = 0x0000_0010;
const NTLM: u32 = 0x0000_0200;
const ANONYMOUS: u32 = 0x0000_0800;
const ALWAYS_SIGN: u32 = 0x0000_8000;
const EXTENDED_SESSION_SECURITY: u32 = 0x0008_0000;
const TARGET_INFO: u32 = 0x0080_0000;
const VERSION: u32 = 0x0200_0000;
const KEY_128: u32 = 0x2000_0000;
const KEY_56: u32 = 0x8000_0000;

/// What this client asks for: Unicode, NTLMv2 with extended session
/// security, a session key for signing, the server's target info.
const OUR_FLAGS: u32 = UNICODE
    | REQUEST_TARGET
    | SIGN
    | NTLM
    | ALWAYS_SIGN
    | EXTENDED_SESSION_SECURITY
    | TARGET_INFO
    | VERSION
    | KEY_128
    | KEY_56;

/// The version field: 6.1 build 7601, NTLMSSP revision 15.
const VERSION_BYTES: [u8; 8] = [6, 1, 0xB1, 0x1D, 0, 0, 0, 0x0F];

const AV_EOL: u16 = 0x0000;
const AV_FLAGS: u16 = 0x0006;
const AV_TARGET_NAME: u16 = 0x0009;
const AV_CHANNEL_BINDINGS: u16 = 0x000A;
/// The MIC-present bit of MsvAvFlags.
const AV_FLAG_MIC: u32 = 0x0000_0002;

/// A user's name, password and domain.
#[derive(Clone)]
pub struct Credentials {
    /// The account name.
    pub user: String,
    /// Its password, which never leaves the client.
    pub password: String,
    /// The account's domain; empty for a server's own accounts.
    pub domain: String,
}

/// The NEGOTIATE message (type 1).
pub fn negotiate() -> Vec<u8> {
    let mut m = Vec::with_capacity(40);
    m.extend_from_slice(SIGNATURE);
    m.extend_from_slice(&1u32.to_le_bytes());
    m.extend_from_slice(&OUR_FLAGS.to_le_bytes());
    // Domain and workstation: empty, offsets at the payload.
    m.extend_from_slice(&[0, 0, 0, 0, 40, 0, 0, 0]);
    m.extend_from_slice(&[0, 0, 0, 0, 40, 0, 0, 0]);
    m.extend_from_slice(&VERSION_BYTES);
    m
}

/// What the CHALLENGE message (type 2) said.
pub struct Challenge {
    pub flags: u32,
    pub server_challenge: [u8; 8],
    /// The server's AV pairs, as sent (EOL included).
    pub target_info: Vec<u8>,
    /// The server's name for itself, for the log.
    pub target_name: String,
}

pub fn parse_challenge(m: &[u8]) -> Result<Challenge, String> {
    if m.len() < 32 || &m[0..8] != SIGNATURE {
        return Err("not an NTLM challenge".into());
    }
    if get32(m, 8) != 2 {
        return Err(format!(
            "NTLM message type {} where 2 was expected",
            get32(m, 8)
        ));
    }
    let field = |at: usize| -> Result<&[u8], String> {
        let len = get16(m, at) as usize;
        let off = get32(m, at + 4) as usize;
        if len == 0 {
            return Ok(&[]);
        }
        m.get(off..off + len).ok_or_else(|| {
            "NTLM challenge field beyond the message".to_string()
        })
    };
    let target_name = crate::wire::from_utf16le(field(12)?);
    let flags = get32(m, 20);
    let mut server_challenge = [0u8; 8];
    server_challenge.copy_from_slice(&m[24..32]);
    let target_info = if m.len() >= 48 && flags & TARGET_INFO != 0 {
        field(40)?.to_vec()
    } else {
        Vec::new()
    };
    Ok(Challenge {
        flags,
        server_challenge,
        target_info,
        target_name,
    })
}

/// The AUTHENTICATE message (type 3) and the session key it
/// establishes, none for an anonymous session.
pub struct Authenticated {
    pub message: Vec<u8>,
    pub session_key: Option<[u8; 16]>,
}

/// The AUTHENTICATE message for `creds` against `challenge`; anonymous
/// with none. `timestamp` is a FILETIME (the server's own, from the
/// negotiate), `client_challenge` eight random bytes, `spn_host` the
/// server as the client addressed it, bound into the target info.
#[allow(clippy::too_many_arguments)]
pub fn authenticate(
    creds: Option<&Credentials>,
    negotiate_message: &[u8],
    challenge_message: &[u8],
    challenge: &Challenge,
    timestamp: u64,
    client_challenge: [u8; 8],
    workstation: &str,
    spn_host: &str,
) -> Authenticated {
    let Some(creds) = creds else {
        return anonymous(challenge);
    };
    let flags = (OUR_FLAGS & challenge.flags) | UNICODE | NTLM | REQUEST_TARGET;

    let ntowf = ntowf_v2(&creds.user, &creds.password, &creds.domain);
    let target_info = target_info_for(&challenge.target_info, spn_host);

    // temp: versions, the time, our challenge, the server's AV pairs.
    let mut temp = Vec::with_capacity(32 + target_info.len());
    temp.extend_from_slice(&[1, 1, 0, 0, 0, 0, 0, 0]);
    temp.extend_from_slice(&timestamp.to_le_bytes());
    temp.extend_from_slice(&client_challenge);
    temp.extend_from_slice(&[0; 4]);
    temp.extend_from_slice(&target_info);
    temp.extend_from_slice(&[0; 4]);

    let nt_proof = hmac_md5(&ntowf, &[&challenge.server_challenge[..], &temp]);
    let mut nt_response = Vec::with_capacity(16 + temp.len());
    nt_response.extend_from_slice(&nt_proof);
    nt_response.extend_from_slice(&temp);

    let mut lm_response = Vec::with_capacity(24);
    lm_response.extend_from_slice(&hmac_md5(
        &ntowf,
        &[&challenge.server_challenge[..], &client_challenge],
    ));
    lm_response.extend_from_slice(&client_challenge);

    let session_key = hmac_md5(&ntowf, &[&nt_proof]);

    let domain = utf16le(&creds.domain);
    let user = utf16le(&creds.user);
    let station = utf16le(workstation);
    let mut message = build_authenticate(
        &lm_response,
        &nt_response,
        &domain,
        &user,
        &station,
        flags,
    );
    let mic = hmac_md5(
        &session_key,
        &[negotiate_message, challenge_message, &message],
    );
    message[72..88].copy_from_slice(&mic);
    Authenticated {
        message,
        session_key: Some(session_key),
    }
}

fn anonymous(challenge: &Challenge) -> Authenticated {
    let flags = (OUR_FLAGS & challenge.flags & !SIGN & !ALWAYS_SIGN)
        | UNICODE
        | NTLM
        | ANONYMOUS;
    Authenticated {
        message: build_authenticate(&[0], &[], &[], &[], &[], flags),
        session_key: None,
    }
}

/// The fixed part of the message with its payload laid out after the
/// MIC field, which is left zero for the caller to fill.
fn build_authenticate(
    lm: &[u8],
    nt: &[u8],
    domain: &[u8],
    user: &[u8],
    workstation: &[u8],
    flags: u32,
) -> Vec<u8> {
    const FIXED: usize = 88;
    let mut m = vec![0u8; FIXED];
    m[0..8].copy_from_slice(SIGNATURE);
    put32(&mut m, 8, 3);
    put32(&mut m, 60, flags);
    m[64..72].copy_from_slice(&VERSION_BYTES);
    let place = |at: usize, data: &[u8], m: &mut Vec<u8>| {
        let off = m.len() as u32;
        put16(m, at, data.len() as u16);
        put16(m, at + 2, data.len() as u16);
        put32(m, at + 4, off);
        m.extend_from_slice(data);
    };
    place(28, domain, &mut m);
    place(36, user, &mut m);
    place(44, workstation, &mut m);
    place(12, lm, &mut m);
    place(20, nt, &mut m);
    place(52, &[], &mut m); // no encrypted random session key: no KEY_EXCH
    m
}

/// NTOWFv2: HMAC-MD5 of the upper-cased user and the domain, keyed by
/// the MD4 of the password.
fn ntowf_v2(user: &str, password: &str, domain: &str) -> [u8; 16] {
    let nt_hash: [u8; 16] = Md4::digest(utf16le(password)).into();
    let mut ident = user.to_uppercase();
    ident.push_str(domain);
    hmac_md5(&nt_hash, &[&utf16le(&ident)])
}

fn hmac_md5(key: &[u8], parts: &[&[u8]]) -> [u8; 16] {
    let mut mac =
        HmacMd5::new_from_slice(key).expect("HMAC takes any key length");
    for p in parts {
        mac.update(p);
    }
    mac.finalize().into_bytes().into()
}

/// The NTLMSSP signature over `message` with sequence number 0: the
/// SPNEGO mechListMIC, made with the client-to-server signing key
/// derived from the session key (extended session security, no key
/// exchange, so the checksum is in the clear).
pub fn mech_list_mic(session_key: &[u8; 16], message: &[u8]) -> [u8; 16] {
    signature(&sign_key(session_key, CLIENT_SIGNING), message)
}

/// Whether `mic` is the server's signature over `message` with
/// sequence number 0.
pub fn server_mic_ok(
    session_key: &[u8; 16],
    message: &[u8],
    mic: &[u8],
) -> bool {
    mic == signature(&sign_key(session_key, SERVER_SIGNING), message)
}

const CLIENT_SIGNING: &[u8] =
    b"session key to client-to-server signing key magic constant\0";
const SERVER_SIGNING: &[u8] =
    b"session key to server-to-client signing key magic constant\0";

fn sign_key(session_key: &[u8; 16], magic: &[u8]) -> [u8; 16] {
    let mut h = Md5::new();
    h.update(session_key);
    h.update(magic);
    h.finalize().into()
}

fn signature(key: &[u8; 16], message: &[u8]) -> [u8; 16] {
    let seq = 0u32.to_le_bytes();
    let sum = hmac_md5(key, &[&seq, message]);
    let mut sig = [0u8; 16];
    sig[0..4].copy_from_slice(&1u32.to_le_bytes());
    sig[4..12].copy_from_slice(&sum[..8]);
    sig[12..16].copy_from_slice(&seq);
    sig
}

/// The server's AV pairs with what the client adds: the MIC flag, empty
/// channel bindings, and the target name it connected by.
fn target_info_for(server_pairs: &[u8], spn_host: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(server_pairs.len() + 64);
    let mut flags = AV_FLAG_MIC;
    let (mut had_bindings, mut had_target) = (false, false);
    let mut at = 0;
    while at + 4 <= server_pairs.len() {
        let id = get16(server_pairs, at);
        let len = get16(server_pairs, at + 2) as usize;
        let Some(value) = server_pairs.get(at + 4..at + 4 + len) else {
            break;
        };
        at += 4 + len;
        match id {
            AV_EOL => break,
            AV_FLAGS => {
                if len == 4 {
                    flags |= get32(value, 0);
                }
            }
            AV_CHANNEL_BINDINGS => had_bindings = true,
            AV_TARGET_NAME => had_target = true,
            _ => {}
        }
        if id != AV_FLAGS {
            out.extend_from_slice(&id.to_le_bytes());
            out.extend_from_slice(&(len as u16).to_le_bytes());
            out.extend_from_slice(value);
        }
    }
    out.extend_from_slice(&AV_FLAGS.to_le_bytes());
    out.extend_from_slice(&4u16.to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());
    if !had_bindings {
        out.extend_from_slice(&AV_CHANNEL_BINDINGS.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(&[0u8; 16]);
    }
    if !had_target {
        let spn = utf16le(&format!("cifs/{spn_host}"));
        out.extend_from_slice(&AV_TARGET_NAME.to_le_bytes());
        out.extend_from_slice(&(spn.len() as u16).to_le_bytes());
        out.extend_from_slice(&spn);
    }
    out.extend_from_slice(&[0u8; 4]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [MS-NLMP] 4.2.4: NTLMv2 with user "User", domain "Domain",
    /// password "Password".
    #[test]
    fn ntowf_v2_matches_the_specification_vector() {
        let key = ntowf_v2("User", "Password", "Domain");
        assert_eq!(
            key,
            [
                0x0c, 0x86, 0x8a, 0x40, 0x3b, 0xfd, 0x7a, 0x93, 0xa3, 0x00,
                0x1e, 0xf2, 0x2e, 0xf0, 0x2e, 0x3f
            ]
        );
    }

    /// [MS-NLMP] 4.2.4.2.1: the NTProofStr over the specification's
    /// server challenge, time, client challenge and target info.
    #[test]
    fn nt_proof_matches_the_specification_vector() {
        let ntowf = ntowf_v2("User", "Password", "Domain");
        let server_challenge = [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef];
        let client_challenge = [0xaa; 8];
        // The vector's target info: NetBIOS domain "Domain", server "Server".
        let mut info = Vec::new();
        for (id, s) in [(2u16, "Domain"), (1u16, "Server")] {
            let v = utf16le(s);
            info.extend_from_slice(&id.to_le_bytes());
            info.extend_from_slice(&(v.len() as u16).to_le_bytes());
            info.extend_from_slice(&v);
        }
        info.extend_from_slice(&[0; 4]);
        let mut temp = Vec::new();
        temp.extend_from_slice(&[1, 1, 0, 0, 0, 0, 0, 0]);
        temp.extend_from_slice(&0u64.to_le_bytes());
        temp.extend_from_slice(&client_challenge);
        temp.extend_from_slice(&[0; 4]);
        temp.extend_from_slice(&info);
        temp.extend_from_slice(&[0; 4]);
        let proof = hmac_md5(&ntowf, &[&server_challenge[..], &temp]);
        assert_eq!(
            proof,
            [
                0x68, 0xcd, 0x0a, 0xb8, 0x51, 0xe5, 0x1c, 0x96, 0xaa, 0xbc,
                0x92, 0x7b, 0xeb, 0xef, 0x6a, 0x1c
            ]
        );
        let session_key = hmac_md5(&ntowf, &[&proof]);
        assert_eq!(
            session_key,
            [
                0x8d, 0xe4, 0x0c, 0xca, 0xdb, 0xc1, 0x4a, 0x82, 0xf1, 0x5c,
                0xb0, 0xad, 0x0d, 0xe9, 0x5c, 0xa3
            ]
        );
    }

    #[test]
    fn challenge_parses_and_authenticate_lays_out() {
        // A type 2 with target name "SRV", flags, challenge, and two AV pairs.
        let mut m = vec![0u8; 56];
        m[0..8].copy_from_slice(SIGNATURE);
        put32(&mut m, 8, 2);
        let name = utf16le("SRV");
        put16(&mut m, 12, name.len() as u16);
        put16(&mut m, 14, name.len() as u16);
        put32(&mut m, 16, 56);
        put32(&mut m, 20, OUR_FLAGS);
        m[24..32].copy_from_slice(&[9; 8]);
        let mut info = Vec::new();
        info.extend_from_slice(&1u16.to_le_bytes());
        info.extend_from_slice(&(name.len() as u16).to_le_bytes());
        info.extend_from_slice(&name);
        info.extend_from_slice(&[0; 4]);
        put16(&mut m, 40, info.len() as u16);
        put16(&mut m, 42, info.len() as u16);
        put32(&mut m, 44, (56 + name.len()) as u32);
        m.extend_from_slice(&name);
        m.extend_from_slice(&info);
        let ch = parse_challenge(&m).unwrap();
        assert_eq!(ch.target_name, "SRV");
        assert_eq!(ch.server_challenge, [9; 8]);
        assert_eq!(ch.target_info, info);

        let creds = Credentials {
            user: "bench".into(),
            password: "correct horse".into(),
            domain: String::new(),
        };
        let t1 = negotiate();
        let a = authenticate(
            Some(&creds),
            &t1,
            &m,
            &ch,
            0x01dc_0000_0000_0000,
            [7; 8],
            "evo",
            "srv",
        );
        assert!(a.session_key.is_some());
        let t3 = &a.message;
        assert_eq!(&t3[0..8], SIGNATURE);
        assert_eq!(get32(t3, 8), 3);
        // The NT response starts with the 16-byte proof and carries the time.
        let nt_off = get32(t3, 24) as usize;
        let nt_len = get16(t3, 20) as usize;
        assert!(nt_len > 16 + 28);
        assert_eq!(&t3[nt_off + 16..nt_off + 18], &[1, 1]);
        // The user is in there, upper case not applied to the field itself.
        let u_off = get32(t3, 40) as usize;
        let u_len = get16(t3, 36) as usize;
        assert_eq!(
            crate::wire::from_utf16le(&t3[u_off..u_off + u_len]),
            "bench"
        );
        // The MIC is set.
        assert_ne!(&t3[72..88], &[0u8; 16]);
        // The target info we sent back has the MIC flag and a cifs/ SPN.
        let temp = &t3[nt_off + 16..nt_off + nt_len];
        let av = &temp[28..temp.len() - 4];
        let text =
            crate::wire::from_utf16le(&av[av.len() - 4 - 16..av.len() - 4]);
        assert!(text.contains("cifs/srv"), "{text:?}");

        let anon = authenticate(None, &t1, &m, &ch, 0, [0; 8], "evo", "srv");
        assert!(anon.session_key.is_none());
        assert_eq!(get16(&anon.message, 12), 1); // LM: one zero byte
        assert_eq!(get16(&anon.message, 20), 0); // NT: empty
        assert_ne!(get32(&anon.message, 60) & ANONYMOUS, 0);
    }
}

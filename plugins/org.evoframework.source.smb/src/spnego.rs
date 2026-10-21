//! SPNEGO ([RFC 4178]) around NTLMSSP, the little of DER it takes: the
//! initial token the client offers and the response tokens either side
//! sends after it. A server that answers with a bare NTLMSSP message is
//! taken as it is.

const OID_SPNEGO: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x02];
const OID_NTLMSSP: &[u8] =
    &[0x2b, 0x06, 0x01, 0x04, 0x01, 0x82, 0x37, 0x02, 0x02, 0x0a];

/// The DER length octets for `n`.
fn length(n: usize) -> Vec<u8> {
    if n < 0x80 {
        return vec![n as u8];
    }
    // Long form: a count byte, then the length big-endian without
    // leading zeros. Written over u64 so the count does not depend on
    // the width of usize (4 on the chip, 8 on a desktop).
    let bytes = (n as u64).to_be_bytes();
    let first = bytes.iter().position(|&b| b != 0).unwrap_or(7);
    let mut out = vec![0x80 | (bytes.len() - first) as u8];
    out.extend_from_slice(&bytes[first..]);
    out
}

/// A tag with its content.
fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(content.len() + 6);
    out.push(tag);
    out.extend_from_slice(&length(content.len()));
    out.extend_from_slice(content);
    out
}

/// The mechanism list as sent in [`init`], the bytes a mechListMIC is
/// computed over.
pub fn mech_types() -> Vec<u8> {
    tlv(0x30, &tlv(0x06, OID_NTLMSSP))
}

/// The initial token: NegTokenInit offering NTLMSSP with `ntlm` (the
/// NEGOTIATE message) as the mechanism token.
pub fn init(ntlm: &[u8]) -> Vec<u8> {
    let mech_types = tlv(0xa0, &mech_types());
    let mech_token = tlv(0xa2, &tlv(0x04, ntlm));
    let mut seq = mech_types;
    seq.extend_from_slice(&mech_token);
    let neg_init = tlv(0xa0, &tlv(0x30, &seq));
    let mut inner = tlv(0x06, OID_SPNEGO);
    inner.extend_from_slice(&neg_init);
    tlv(0x60, &inner)
}

/// A response token carrying `ntlm` (the AUTHENTICATE message) and,
/// with it, the MIC over the mechanism list; or the MIC alone when the
/// server asked for it after the mechanism had completed.
pub fn response(ntlm: Option<&[u8]>, mic: Option<&[u8]>) -> Vec<u8> {
    let mut seq = Vec::new();
    if let Some(t) = ntlm {
        seq.extend_from_slice(&tlv(0xa2, &tlv(0x04, t)));
    }
    if let Some(m) = mic {
        seq.extend_from_slice(&tlv(0xa3, &tlv(0x04, m)));
    }
    tlv(0xa1, &tlv(0x30, &seq))
}

/// What a server's NegTokenResp said.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Reply<'a> {
    /// negState: 0 accept-completed, 1 accept-incomplete, 2 reject,
    /// 3 request-mic; none when not said.
    pub state: Option<u8>,
    /// The mechanism's token, if one came.
    pub token: Option<&'a [u8]>,
    /// The server's MIC over the mechanism list, if one came.
    pub mic: Option<&'a [u8]>,
}

/// A server's reply: a NegTokenResp taken apart, or a bare NTLMSSP
/// message as the token.
pub fn parse(blob: &[u8]) -> Result<Reply<'_>, String> {
    if blob.starts_with(b"NTLMSSP\0") {
        return Ok(Reply {
            token: Some(blob),
            ..Reply::default()
        });
    }
    let (tag, body, _) = read_tlv(blob).ok_or("not a SPNEGO token")?;
    // Some servers wrap the first reply as an InitialContextToken.
    let body = if tag == 0x60 {
        let (_, _, rest) = read_tlv(body).ok_or("no OID in the token")?;
        let (tag, inner, _) = read_tlv(rest).ok_or("empty token")?;
        if tag != 0xa1 {
            return Err(format!(
                "SPNEGO tag {tag:#04x} where a response was expected"
            ));
        }
        inner
    } else if tag == 0xa1 {
        body
    } else {
        return Err(format!(
            "SPNEGO tag {tag:#04x} where a response was expected"
        ));
    };
    let (tag, mut fields, _) =
        read_tlv(body).ok_or("no sequence in the response")?;
    if tag != 0x30 {
        return Err("no sequence in the response".into());
    }
    let mut reply = Reply::default();
    while let Some((tag, value, rest)) = read_tlv(fields) {
        fields = rest;
        match tag {
            0xa0 => {
                if let Some((0x0a, v, _)) = read_tlv(value) {
                    reply.state = v.first().copied();
                }
            }
            0xa2 | 0xa3 => {
                let (t, octets, _) = read_tlv(value).ok_or("empty field")?;
                if t != 0x04 {
                    return Err("a field that is not an octet string".into());
                }
                if tag == 0xa2 {
                    reply.token = Some(octets);
                } else {
                    reply.mic = Some(octets);
                }
            }
            _ => {}
        }
    }
    Ok(reply)
}

/// One TLV: its tag, its content, and what follows it.
fn read_tlv(b: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let tag = *b.first()?;
    let first = *b.get(1)?;
    let (len, head) = if first < 0x80 {
        (first as usize, 2)
    } else {
        let n = (first & 0x7f) as usize;
        if n == 0 || n > 4 {
            return None;
        }
        let mut len = 0usize;
        for i in 0..n {
            len = (len << 8) | *b.get(2 + i)? as usize;
        }
        (len, 2 + n)
    };
    let content = b.get(head..head + len)?;
    Some((tag, content, &b[head + len..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lengths_encode_as_der_on_any_width() {
        assert_eq!(length(0), [0x00]);
        assert_eq!(length(0x7f), [0x7f]);
        assert_eq!(length(0x80), [0x81, 0x80]);
        assert_eq!(length(300), [0x82, 0x01, 0x2c]);
        assert_eq!(length(419), [0x82, 0x01, 0xa3]);
        assert_eq!(length(70_000), [0x83, 0x01, 0x11, 0x70]);
        let long = tlv(0x04, &[9u8; 419]);
        let (tag, body, rest) = read_tlv(&long).unwrap();
        assert_eq!((tag, body.len(), rest.len()), (0x04, 419, 0));
    }

    #[test]
    fn init_wraps_the_mechanism_and_token() {
        let t = init(b"NTLMSSP\0\x01");
        assert_eq!(t[0], 0x60);
        let (_, body, _) = read_tlv(&t).unwrap();
        let (tag, oid, rest) = read_tlv(body).unwrap();
        assert_eq!((tag, oid), (0x06, OID_SPNEGO));
        let (tag, _, _) = read_tlv(rest).unwrap();
        assert_eq!(tag, 0xa0);
        assert!(t.windows(OID_NTLMSSP.len()).any(|w| w == OID_NTLMSSP));
    }

    #[test]
    fn response_tokens_unwrap() {
        let ours = response(Some(b"NTLMSSP\0\x03"), Some(&[9u8; 16]));
        let r = parse(&ours).unwrap();
        assert_eq!(r.token, Some(&b"NTLMSSP\0\x03"[..]));
        assert_eq!(r.mic, Some(&[9u8; 16][..]));
        // A server's: negResult, supportedMech, responseToken.
        let mut seq = tlv(0xa0, &tlv(0x0a, &[1]));
        seq.extend_from_slice(&tlv(0xa1, &tlv(0x06, OID_NTLMSSP)));
        seq.extend_from_slice(&tlv(0xa2, &tlv(0x04, b"NTLMSSP\0\x02")));
        let theirs = tlv(0xa1, &tlv(0x30, &seq));
        let r = parse(&theirs).unwrap();
        assert_eq!(r.state, Some(1));
        assert_eq!(r.token, Some(&b"NTLMSSP\0\x02"[..]));
        assert_eq!(r.mic, None);
        assert_eq!(
            parse(b"NTLMSSP\0\x02").unwrap().token,
            Some(&b"NTLMSSP\0\x02"[..])
        );
        assert!(parse(b"\x30\x00").is_err());
        // Long-form lengths survive.
        let long = response(Some(&vec![7u8; 300]), None);
        assert_eq!(parse(&long).unwrap().token.unwrap().len(), 300);
        // The MIC alone.
        let alone = response(None, Some(&[1u8; 16]));
        let r = parse(&alone).unwrap();
        assert_eq!((r.token, r.mic.map(<[u8]>::len)), (None, Some(16)));
        assert_eq!(
            mech_types(),
            [
                0x30, 0x0c, 0x06, 0x0a, 0x2b, 0x06, 0x01, 0x04, 0x01, 0x82,
                0x37, 0x02, 0x02, 0x0a
            ]
        );
    }
}

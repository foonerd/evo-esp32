//! The bytes of SMB2: little-endian fields, the 64-byte header, the
//! NetBIOS session framing, and the status codes a listener may be
//! told about.

use std::fmt;

/// The header's length.
pub const HEADER: usize = 64;

pub const NEGOTIATE: u16 = 0x0000;
pub const SESSION_SETUP: u16 = 0x0001;
pub const LOGOFF: u16 = 0x0002;
pub const TREE_CONNECT: u16 = 0x0003;
pub const TREE_DISCONNECT: u16 = 0x0004;
pub const CREATE: u16 = 0x0005;
pub const CLOSE: u16 = 0x0006;
pub const READ: u16 = 0x0008;
pub const QUERY_DIRECTORY: u16 = 0x000E;

pub const FLAG_ASYNC_COMMAND: u32 = 0x0000_0002;
pub const FLAG_SIGNED: u32 = 0x0000_0008;

/// NT status codes.
pub mod status {
    pub const SUCCESS: u32 = 0x0000_0000;
    pub const PENDING: u32 = 0x0000_0103;
    pub const BUFFER_OVERFLOW: u32 = 0x8000_0005;
    pub const NO_MORE_FILES: u32 = 0x8000_0006;
    pub const INVALID_PARAMETER: u32 = 0xC000_000D;
    pub const END_OF_FILE: u32 = 0xC000_0011;
    pub const MORE_PROCESSING_REQUIRED: u32 = 0xC000_0016;
    pub const ACCESS_DENIED: u32 = 0xC000_0022;
    pub const OBJECT_NAME_NOT_FOUND: u32 = 0xC000_0034;
    pub const OBJECT_PATH_NOT_FOUND: u32 = 0xC000_003A;
    pub const SHARING_VIOLATION: u32 = 0xC000_0043;
    pub const LOGON_FAILURE: u32 = 0xC000_006D;
    pub const ACCOUNT_RESTRICTION: u32 = 0xC000_006E;
    pub const PASSWORD_EXPIRED: u32 = 0xC000_0071;
    pub const ACCOUNT_DISABLED: u32 = 0xC000_0072;
    pub const FILE_IS_A_DIRECTORY: u32 = 0xC000_00BA;
    pub const NOT_SUPPORTED: u32 = 0xC000_00BB;
    pub const BAD_NETWORK_NAME: u32 = 0xC000_00CC;
    pub const NOT_A_DIRECTORY: u32 = 0xC000_0103;
    pub const USER_SESSION_DELETED: u32 = 0xC000_0203;
    pub const NETWORK_SESSION_EXPIRED: u32 = 0xC000_035C;
}

/// A status in the listener's words, for the status strip.
pub fn status_name(code: u32) -> String {
    use status::*;
    let name = match code {
        SUCCESS => "ok",
        PENDING => "pending",
        BUFFER_OVERFLOW => "buffer overflow",
        NO_MORE_FILES => "no more files",
        INVALID_PARAMETER => "invalid parameter",
        END_OF_FILE => "end of file",
        MORE_PROCESSING_REQUIRED => "more processing required",
        ACCESS_DENIED => "access denied",
        OBJECT_NAME_NOT_FOUND | OBJECT_PATH_NOT_FOUND => "not found",
        SHARING_VIOLATION => "in use elsewhere",
        LOGON_FAILURE => "wrong user or password",
        ACCOUNT_RESTRICTION => "account restricted",
        PASSWORD_EXPIRED => "password expired",
        ACCOUNT_DISABLED => "account disabled",
        FILE_IS_A_DIRECTORY => "is a folder",
        NOT_SUPPORTED => "not supported by the server",
        BAD_NETWORK_NAME => "no such share",
        NOT_A_DIRECTORY => "not a folder",
        USER_SESSION_DELETED | NETWORK_SESSION_EXPIRED => "session ended",
        _ => return format!("status {code:#010x}"),
    };
    name.to_string()
}

/// What an SMB2 exchange can fail with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The transport: a socket error, a short frame, a close.
    Io(String),
    /// A frame the parser could not read.
    Malformed(&'static str),
    /// The server answered with a status other than success.
    Status(u32),
    /// Authentication did not complete.
    Auth(String),
    /// A response whose signature did not verify.
    Signature,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Malformed(what) => write!(f, "malformed {what}"),
            Error::Status(code) => f.write_str(&status_name(*code)),
            Error::Auth(e) => write!(f, "{e}"),
            Error::Signature => f.write_str("bad signature on the reply"),
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e.to_string())
    }
}

/// A request's header, as the client fills it.
pub struct Header {
    pub credit_charge: u16,
    pub command: u16,
    pub credits_requested: u16,
    pub message_id: u64,
    pub tree_id: u32,
    pub session_id: u64,
}

impl Header {
    /// The 64 bytes, signature zero.
    pub fn encode(&self) -> [u8; HEADER] {
        let mut h = [0u8; HEADER];
        h[0..4].copy_from_slice(&[0xFE, b'S', b'M', b'B']);
        put16(&mut h, 4, HEADER as u16);
        put16(&mut h, 6, self.credit_charge);
        put32(&mut h, 8, 0); // ChannelSequence / Reserved
        put16(&mut h, 12, self.command);
        put16(&mut h, 14, self.credits_requested);
        put32(&mut h, 16, 0); // Flags
        put32(&mut h, 20, 0); // NextCommand
        put64(&mut h, 24, self.message_id);
        put32(&mut h, 32, 0xFEFF); // Reserved (ProcessId)
        put32(&mut h, 36, self.tree_id);
        put64(&mut h, 40, self.session_id);
        h
    }
}

/// A response's header, as read.
#[derive(Debug, Clone, Copy)]
pub struct Reply {
    pub status: u32,
    pub credits_granted: u16,
    pub flags: u32,
    pub message_id: u64,
    pub session_id: u64,
    pub tree_id: u32,
}

impl Reply {
    pub fn parse(frame: &[u8]) -> Result<Self, Error> {
        if frame.len() < HEADER {
            return Err(Error::Malformed("header"));
        }
        if frame[0..4] != [0xFE, b'S', b'M', b'B'] {
            return Err(Error::Malformed("protocol id"));
        }
        if get16(frame, 4) != HEADER as u16 {
            return Err(Error::Malformed("header size"));
        }
        let flags = get32(frame, 16);
        Ok(Reply {
            status: get32(frame, 8),
            credits_granted: get16(frame, 14),
            flags,
            message_id: get64(frame, 24),
            // In an async reply the AsyncId sits where TreeId does.
            tree_id: if flags & FLAG_ASYNC_COMMAND != 0 {
                0
            } else {
                get32(frame, 36)
            },
            session_id: get64(frame, 40),
        })
    }
}

pub fn put16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}
pub fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}
pub fn put64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}
pub fn get16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
pub fn get32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}
pub fn get64(b: &[u8], at: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(a)
}

/// A growable little-endian writer for request bodies.
#[derive(Default)]
pub struct Body(pub Vec<u8>);

impl Body {
    pub fn with_capacity(n: usize) -> Self {
        Body(Vec::with_capacity(n))
    }
    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.0.extend_from_slice(v);
        self
    }
}

/// A string as SMB2 carries it: UTF-16 little-endian, no terminator.
pub fn utf16le(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}

/// A UTF-16LE field back to a string; a lone surrogate becomes U+FFFD.
pub fn from_utf16le(b: &[u8]) -> String {
    let units: Vec<u16> = b
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&units)
}

/// A FILETIME (100 ns ticks since 1601) to Unix milliseconds, if in
/// range.
pub fn filetime_to_unix_ms(ft: u64) -> Option<u64> {
    const EPOCH_DIFF: u64 = 116_444_736_000_000_000;
    ft.checked_sub(EPOCH_DIFF).map(|t| t / 10_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trips() {
        let h = Header {
            credit_charge: 1,
            command: READ,
            credits_requested: 32,
            message_id: 7,
            tree_id: 3,
            session_id: 0x1122_3344_5566_7788,
        };
        let bytes = h.encode();
        let r = Reply::parse(&bytes).unwrap();
        assert_eq!(get16(&bytes, 12), READ);
        assert_eq!(r.message_id, 7);
        assert_eq!(r.tree_id, 3);
        assert_eq!(r.session_id, 0x1122_3344_5566_7788);
        assert_eq!(r.credits_granted, 32);
        assert!(Reply::parse(&bytes[..60]).is_err());
    }

    #[test]
    fn strings_and_times() {
        assert_eq!(utf16le("ab"), vec![b'a', 0, b'b', 0]);
        assert_eq!(from_utf16le(&utf16le("Sweep 1.flac")), "Sweep 1.flac");
        // 2026-10-21T12:00:00Z as FILETIME.
        let ft = 116_444_736_000_000_000 + 1_792_584_000u64 * 10_000_000;
        assert_eq!(filetime_to_unix_ms(ft), Some(1_792_584_000_000));
        assert_eq!(filetime_to_unix_ms(0), None);
        assert_eq!(
            status_name(status::LOGON_FAILURE),
            "wrong user or password"
        );
        assert_eq!(status_name(0xC000_FFFF), "status 0xc000ffff");
    }
}

//! The SMB1 client ([MS-CIFS], [MS-SMB]): the NT LM 0.12 dialect for
//! the boxes that speak nothing newer. Spoken only to a share named
//! `smb1://`, never reached by falling back from `smb://`. Unicode and
//! 32-bit status throughout; NTLMv2 under SPNEGO where the server has
//! extended security, NTLMv2 in the session setup's own fields where
//! it has not; the MAC signature when the server requires it.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use md5::{Digest, Md5};

use crate::ntlm;
use crate::smb2::resolve;
use crate::spnego;
use crate::wire::{self, get16, get32, get64, status, utf16le, Body, Error};

const HEADER: usize = 32;

const COM_CLOSE: u8 = 0x04;
const COM_READ_ANDX: u8 = 0x2E;
const COM_TRANSACTION2: u8 = 0x32;
const COM_TREE_DISCONNECT: u8 = 0x71;
const COM_NEGOTIATE: u8 = 0x72;
const COM_SESSION_SETUP_ANDX: u8 = 0x73;
const COM_LOGOFF_ANDX: u8 = 0x74;
const COM_TREE_CONNECT_ANDX: u8 = 0x75;
const COM_NT_CREATE_ANDX: u8 = 0xA2;
const NO_ANDX: u8 = 0xFF;

const TRANS2_FIND_FIRST2: u16 = 0x0001;
const TRANS2_FIND_NEXT2: u16 = 0x0002;

/// Flags: case-insensitive, canonicalised paths.
const FLAGS: u8 = 0x18;
/// Flags2: Unicode, NT status, extended security, long names.
const FLAGS2: u16 = 0x8000 | 0x4000 | 0x0800 | 0x0040 | 0x0001;
const FLAGS2_NT_STATUS: u16 = 0x4000;

const SECURITY_MODE_SIGNING_REQUIRED: u8 = 0x08;
const CAP_UNICODE: u32 = 0x0000_0004;
const CAP_NT_SMBS: u32 = 0x0000_0010;
const CAP_STATUS32: u32 = 0x0000_0040;
const CAP_LARGE_READX: u32 = 0x0000_4000;
const CAP_EXTENDED_SECURITY: u32 = 0x8000_0000;
/// What this client claims: Unicode, NT SMBs, 32-bit status, large
/// files and reads, extended security.
const OUR_CAPABILITIES: u32 = CAP_UNICODE
    | CAP_NT_SMBS
    | CAP_STATUS32
    | 0x0000_0008
    | CAP_LARGE_READX
    | CAP_EXTENDED_SECURITY;

const FILE_READ_DATA: u32 = 0x0000_0001;
const FILE_READ_ATTRIBUTES: u32 = 0x0000_0080;
const SYNCHRONIZE: u32 = 0x0010_0000;
const FILE_SHARE_ALL: u32 = 0x0000_0007;
const FILE_OPEN: u32 = 0x0000_0001;
const FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;
const ATTR_HIDDEN: u32 = 0x02;
const ATTR_SYSTEM: u32 = 0x04;
const ATTR_DIRECTORY: u32 = 0x10;
const SEARCH_ATTRIBUTES: u16 =
    (ATTR_HIDDEN | ATTR_SYSTEM | ATTR_DIRECTORY) as u16;
const FIND_CLOSE_AT_EOS: u16 = 0x0002;
const FIND_CONTINUE_FROM_LAST: u16 = 0x0008;
const FIND_FILE_DIRECTORY_INFO: u16 = 0x0101;
const SEARCH_COUNT: u16 = 256;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const IO_TIMEOUT: Duration = Duration::from_secs(20);
const FRAME_LIMIT: usize = 1024 * 1024;
const OUR_BUFFER: u16 = 0xFFFF;

pub use crate::smb2::{Config, DirEntry};

/// An open file on the share.
#[derive(Debug, Clone, Copy)]
pub struct File {
    /// The server's id for the open.
    pub fid: u16,
    /// Its length in bytes.
    pub size: u64,
}

/// A connected, authenticated session on one share.
pub struct Session {
    stream: TcpStream,
    uid: u16,
    tid: u16,
    mid: u16,
    /// The MAC key and the next sequence number, when signing.
    signing: Option<(Vec<u8>, u32)>,
    max_buffer: u32,
    large_readx: bool,
    frame: Vec<u8>,
    /// The server's name for itself, from the challenge.
    pub server_name: String,
    /// Whether the server let the session in as a guest.
    pub guest: bool,
}

struct Negotiated {
    signing_required: bool,
    capabilities: u32,
    challenge: [u8; 8],
    domain: String,
    server_time: u64,
}

impl Session {
    /// Connect, negotiate NT LM 0.12, authenticate and connect to the
    /// share.
    pub fn connect(cfg: &Config) -> Result<Session, Error> {
        let addr = resolve(&cfg.host, cfg.port)?;
        let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
            .map_err(|e| {
                Error::Io(format!("connect {}:{}: {e}", cfg.host, cfg.port))
            })?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        stream.set_nodelay(true)?;
        let mut s = Session {
            stream,
            uid: 0,
            tid: 0,
            mid: 1,
            signing: None,
            max_buffer: 16644,
            large_readx: false,
            frame: Vec::with_capacity(HEADER + 1024),
            server_name: String::new(),
            guest: false,
        };
        let negotiated = s.negotiate()?;
        s.session_setup(cfg, &negotiated)?;
        s.tree_connect(&cfg.host, &cfg.share)?;
        Ok(s)
    }

    /// The entries of the folder at `path` (`a/b`, empty for the root),
    /// hidden and system ones left out.
    pub fn list(&mut self, path: &str) -> Result<Vec<DirEntry>, Error> {
        let pattern = if path.trim_matches('/').is_empty() {
            "\\*".to_string()
        } else {
            format!("\\{}\\*", path.trim_matches('/').replace('/', "\\"))
        };
        let mut entries = Vec::new();
        let mut params = Body::with_capacity(16 + pattern.len() * 2);
        params
            .u16(SEARCH_ATTRIBUTES)
            .u16(SEARCH_COUNT)
            .u16(FIND_CLOSE_AT_EOS);
        params
            .u16(FIND_FILE_DIRECTORY_INFO)
            .u32(0)
            .bytes(&utf16le(&pattern))
            .u16(0);
        let (reply_params, data) =
            self.transaction2(TRANS2_FIND_FIRST2, &params.0)?;
        if reply_params.len() < 10 {
            return Err(Error::Malformed("find reply"));
        }
        let sid = get16(&reply_params, 0);
        let mut end = get16(&reply_params, 4) != 0;
        let mut last = parse_find(&data, &mut entries)?;
        while !end {
            let Some(name) = last.take() else { break };
            let mut params = Body::with_capacity(16 + name.len() * 2);
            params
                .u16(sid)
                .u16(SEARCH_COUNT)
                .u16(FIND_FILE_DIRECTORY_INFO)
                .u32(0);
            params.u16(FIND_CLOSE_AT_EOS | FIND_CONTINUE_FROM_LAST);
            params.bytes(&utf16le(&name)).u16(0);
            let (reply_params, data) =
                self.transaction2(TRANS2_FIND_NEXT2, &params.0)?;
            if reply_params.len() < 8 {
                return Err(Error::Malformed("find next reply"));
            }
            end = get16(&reply_params, 2) != 0;
            last = parse_find(&data, &mut entries)?;
        }
        Ok(entries)
    }

    /// Open the file at `path` for reading.
    pub fn open(&mut self, path: &str) -> Result<File, Error> {
        // Share-relative, no leading backslash: as Windows sends it.
        let name = utf16le(&path.trim_matches('/').replace('/', "\\"));
        let mut b = Body::with_capacity(52 + name.len() + 4);
        b.u8(24); // WordCount
        b.u8(NO_ANDX).u8(0).u16(0);
        b.u8(0).u16(name.len() as u16).u32(0).u32(0);
        b.u32(FILE_READ_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE);
        b.u64(0).u32(0).u32(FILE_SHARE_ALL).u32(FILE_OPEN);
        b.u32(FILE_NON_DIRECTORY_FILE).u32(2).u8(0);
        // ByteCount, then the name on an even byte.
        let mut bytes = Body::with_capacity(name.len() + 3);
        bytes.u8(0); // pad: the words end on an odd byte
        bytes.bytes(&name).u16(0);
        b.u16(bytes.0.len() as u16).bytes(&bytes.0);
        self.send(COM_NT_CREATE_ANDX, &b.0)?;
        let words = self.words()?;
        if words.len() < 34 * 2 {
            return Err(Error::Malformed("create reply"));
        }
        Ok(File {
            fid: get16(words, 5),
            size: get64(words, 55),
        })
    }

    /// Read from `file` at `offset` into `buf`: the bytes read, zero at
    /// the end of the file.
    pub fn read(
        &mut self,
        file: &File,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, Error> {
        if offset >= file.size || buf.is_empty() {
            return Ok(0);
        }
        let ceiling = if self.large_readx {
            0xFFFF
        } else {
            self.max_buffer.saturating_sub(HEADER as u32 + 64).max(1024)
        };
        let want = (buf.len() as u32).min(ceiling) as u16;
        let mut b = Body::with_capacity(32);
        b.u8(12);
        b.u8(NO_ANDX).u8(0).u16(0);
        b.u16(file.fid).u32(offset as u32).u16(want).u16(want);
        b.u32(0).u16(0).u32((offset >> 32) as u32);
        b.u16(0);
        self.send(COM_READ_ANDX, &b.0)?;
        let words = self.words()?.to_vec();
        if words.len() < 24 {
            return Err(Error::Malformed("read reply"));
        }
        let len =
            get16(&words, 10) as usize | ((get16(&words, 14) as usize) << 16);
        let off = get16(&words, 12) as usize;
        let data = self
            .frame
            .get(off..off + len)
            .ok_or(Error::Malformed("read data"))?;
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        Ok(n)
    }

    /// Close `file`.
    pub fn close_file(&mut self, file: &File) -> Result<(), Error> {
        let mut b = Body::with_capacity(10);
        b.u8(3).u16(file.fid).u32(0xFFFF_FFFF).u16(0);
        self.send(COM_CLOSE, &b.0)?;
        Ok(())
    }

    fn negotiate(&mut self) -> Result<Negotiated, Error> {
        let dialect = b"\x02NT LM 0.12\0";
        let mut b = Body::with_capacity(4 + dialect.len());
        b.u8(0).u16(dialect.len() as u16).bytes(dialect);
        self.send(COM_NEGOTIATE, &b.0)?;
        let words = self.words()?.to_vec();
        if words.len() < 34 {
            return Err(Error::Auth(
                "the server does not speak NT LM 0.12".into(),
            ));
        }
        if get16(&words, 0) != 0 {
            return Err(Error::Auth("the server chose no dialect".into()));
        }
        let security_mode = words[2];
        self.max_buffer = get32(&words, 7);
        let capabilities = get32(&words, 19);
        self.large_readx = capabilities & CAP_LARGE_READX != 0;
        if capabilities & CAP_UNICODE == 0 || capabilities & CAP_NT_SMBS == 0 {
            return Err(Error::Auth(
                "the server has no Unicode or NT SMBs".into(),
            ));
        }
        let challenge_len = words[33] as usize;
        let bytes = self.bytes()?;
        let mut challenge = [0u8; 8];
        let mut domain = String::new();
        if capabilities & CAP_EXTENDED_SECURITY == 0 {
            if bytes.len() < challenge_len || challenge_len != 8 {
                return Err(Error::Malformed("negotiate challenge"));
            }
            challenge.copy_from_slice(&bytes[..8]);
            let rest = &bytes[8..];
            // The domain, Unicode, on an even byte.
            let rest = if (HEADER + 1 + 34 + 2 + 8) % 2 == 1 {
                rest.get(1..).unwrap_or(&[])
            } else {
                rest
            };
            domain =
                wire::from_utf16le(rest).trim_end_matches('\0').to_string();
        }
        Ok(Negotiated {
            signing_required: security_mode & SECURITY_MODE_SIGNING_REQUIRED
                != 0,
            capabilities,
            challenge,
            domain,
            server_time: get64(&words, 23),
        })
    }

    fn session_setup(
        &mut self,
        cfg: &Config,
        n: &Negotiated,
    ) -> Result<(), Error> {
        if n.capabilities & CAP_EXTENDED_SECURITY != 0 {
            self.session_setup_extended(cfg, n)
        } else {
            self.session_setup_classic(cfg, n)
        }
    }

    fn session_setup_extended(
        &mut self,
        cfg: &Config,
        n: &Negotiated,
    ) -> Result<(), Error> {
        let type1 = ntlm::negotiate();
        let status = self.send_setup_blob(&spnego::init(&type1))?;
        if status != status::MORE_PROCESSING_REQUIRED {
            return Err(Error::Auth("server skipped the challenge".into()));
        }
        self.uid = get16(&self.frame, 28);
        let blob = self.setup_blob()?.to_vec();
        let type2 = spnego::parse(&blob)
            .map_err(Error::Auth)?
            .token
            .ok_or_else(|| Error::Auth("no challenge in the reply".into()))?
            .to_vec();
        let challenge = ntlm::parse_challenge(&type2).map_err(Error::Auth)?;
        self.server_name = challenge.target_name.clone();
        let mut client_challenge = [0u8; 8];
        getrandom::fill(&mut client_challenge)
            .map_err(|e| Error::Io(format!("random: {e}")))?;
        let auth = ntlm::authenticate(
            cfg.credentials.as_ref(),
            &type1,
            &type2,
            &challenge,
            n.server_time,
            client_challenge,
            &cfg.workstation,
            &cfg.host,
        );
        let mech_types = spnego::mech_types();
        let our_mic = auth
            .session_key
            .map(|k| ntlm::mech_list_mic(&k, &mech_types));
        let mut status = self.send_setup_blob(&spnego::response(
            Some(&auth.message),
            our_mic.as_ref().map(|m| &m[..]),
        ))?;
        if status == status::MORE_PROCESSING_REQUIRED {
            status = self.send_setup_blob(&spnego::response(
                None,
                our_mic.as_ref().map(|m| &m[..]),
            ))?;
        }
        self.finish_setup(status, cfg, n, auth.session_key.map(|k| k.to_vec()))
    }

    fn session_setup_classic(
        &mut self,
        cfg: &Config,
        n: &Negotiated,
    ) -> Result<(), Error> {
        // NTLMv2 in the message's own fields, against the negotiate's
        // challenge, with a target info of the server's domain.
        let (lm, nt, key, user, domain) = match &cfg.credentials {
            Some(c) => {
                let mut client_challenge = [0u8; 8];
                getrandom::fill(&mut client_challenge)
                    .map_err(|e| Error::Io(format!("random: {e}")))?;
                let r = ntlm::classic_v2(
                    c,
                    &n.challenge,
                    &n.domain,
                    n.server_time,
                    client_challenge,
                );
                let mut key = r.session_key.to_vec();
                key.extend_from_slice(&r.nt_response);
                (
                    r.lm_response,
                    r.nt_response,
                    Some(key),
                    c.user.clone(),
                    c.domain.clone(),
                )
            }
            None => {
                (Vec::new(), Vec::new(), None, String::new(), String::new())
            }
        };
        let mut b = Body::with_capacity(64 + lm.len() + nt.len());
        b.u8(13);
        b.u8(NO_ANDX).u8(0).u16(0);
        b.u16(OUR_BUFFER).u16(2).u16(0).u32(0);
        b.u16(lm.len() as u16)
            .u16(nt.len() as u16)
            .u32(0)
            .u32(OUR_CAPABILITIES & !CAP_EXTENDED_SECURITY);
        let mut bytes = Body::with_capacity(lm.len() + nt.len() + 64);
        bytes.bytes(&lm).bytes(&nt);
        if (HEADER + 1 + 26 + 2 + bytes.0.len()) % 2 == 1 {
            bytes.u8(0);
        }
        bytes.bytes(&utf16le(&user)).u16(0);
        bytes.bytes(&utf16le(&domain)).u16(0);
        bytes.bytes(&utf16le("evo")).u16(0);
        bytes.bytes(&utf16le("evo-esp32")).u16(0);
        b.u16(bytes.0.len() as u16).bytes(&bytes.0);
        let status = match self.send(COM_SESSION_SETUP_ANDX, &b.0) {
            Ok(s) => s,
            Err(Error::Status(s)) => s,
            Err(e) => return Err(e),
        };
        self.uid = get16(&self.frame, 28);
        self.finish_setup(status, cfg, n, key)
    }

    fn finish_setup(
        &mut self,
        status: u32,
        cfg: &Config,
        n: &Negotiated,
        key: Option<Vec<u8>>,
    ) -> Result<(), Error> {
        match status {
            status::SUCCESS => {}
            status::LOGON_FAILURE if cfg.credentials.is_none() => {
                return Err(Error::Auth(
                    "guest access refused; set a user and password".into(),
                ));
            }
            // A Samba 3 with the 2016 security updates refuses NTLMv2
            // outside NTLMSSP, which is all the old session setup can
            // carry; NTLMv1 is not spoken here.
            status::INVALID_PARAMETER
                if cfg.credentials.is_some()
                    && n.capabilities & CAP_EXTENDED_SECURITY == 0 =>
            {
                return Err(Error::Auth(
                    "the server refuses NTLMv2 in the old session setup; a guest share, or SMB 2 if it has it".into(),
                ));
            }
            other => return Err(Error::Status(other)),
        }
        let words = self.words()?;
        self.guest = words.len() >= 6 && get16(words, 4) & 1 != 0;
        if let (Some(key), false, true) = (key, self.guest, n.signing_required)
        {
            // Requests from here on carry the MAC; the sequence starts
            // at two, the setup having been zero and one.
            self.signing = Some((key, 2));
        }
        Ok(())
    }

    fn send_setup_blob(&mut self, blob: &[u8]) -> Result<u32, Error> {
        let mut b = Body::with_capacity(32 + blob.len());
        b.u8(12);
        b.u8(NO_ANDX).u8(0).u16(0);
        b.u16(OUR_BUFFER).u16(2).u16(0).u32(0);
        b.u16(blob.len() as u16).u32(0).u32(OUR_CAPABILITIES);
        let mut bytes = Body::with_capacity(blob.len() + 32);
        bytes.bytes(blob);
        if (HEADER + 1 + 24 + 2 + bytes.0.len()) % 2 == 1 {
            bytes.u8(0);
        }
        bytes.bytes(&utf16le("evo")).u16(0);
        bytes.bytes(&utf16le("evo-esp32")).u16(0);
        b.u16(bytes.0.len() as u16).bytes(&bytes.0);
        // A refusal is a status for the caller to word, not an error
        // here.
        match self.send(COM_SESSION_SETUP_ANDX, &b.0) {
            Ok(s) => Ok(s),
            Err(Error::Status(s)) => Ok(s),
            Err(e) => Err(e),
        }
    }

    /// The security blob of a session setup reply, in the frame.
    fn setup_blob(&self) -> Result<&[u8], Error> {
        let words = self.words()?;
        if words.len() < 8 {
            return Err(Error::Malformed("session setup reply"));
        }
        let len = get16(words, 6) as usize;
        let bytes = self.bytes()?;
        bytes.get(..len).ok_or(Error::Malformed("security blob"))
    }

    fn tree_connect(&mut self, host: &str, share: &str) -> Result<(), Error> {
        let path = utf16le(&format!("\\\\{host}\\{share}"));
        let mut b = Body::with_capacity(16 + path.len() + 8);
        b.u8(4);
        b.u8(NO_ANDX).u8(0).u16(0);
        b.u16(0).u16(1);
        let mut bytes = Body::with_capacity(path.len() + 10);
        bytes.u8(0); // the one-byte password
                     // Words 8 bytes + count byte + bytecount 2 → the path lands on an
                     // odd byte after the password: pad.
        if (HEADER + 1 + 8 + 2 + 1) % 2 == 1 {
            bytes.u8(0);
        }
        bytes.bytes(&path).u16(0).bytes(b"?????\0");
        b.u16(bytes.0.len() as u16).bytes(&bytes.0);
        self.send(COM_TREE_CONNECT_ANDX, &b.0)?;
        self.tid = get16(&self.frame, 24);
        Ok(())
    }

    /// One TRANSACTION2 with `setup` and `params`, its reply's
    /// parameters and data gathered across however many messages the
    /// server split them into.
    fn transaction2(
        &mut self,
        setup: u16,
        params: &[u8],
    ) -> Result<(Vec<u8>, Vec<u8>), Error> {
        let max_data = (self.max_buffer.saturating_sub(HEADER as u32 + 96))
            .clamp(1024, 0xFFFF) as u16;
        let mut b = Body::with_capacity(40 + params.len() + 4);
        b.u8(15);
        b.u16(params.len() as u16)
            .u16(0)
            .u16(16)
            .u16(max_data)
            .u8(0)
            .u8(0);
        b.u16(0).u32(0).u16(0);
        // Parameters start after the words, the count and a pad to 4.
        let words_end = HEADER + 1 + 30 + 2;
        let pad = (4 - words_end % 4) % 4;
        let param_offset = words_end + pad;
        b.u16(params.len() as u16)
            .u16(param_offset as u16)
            .u16(0)
            .u16(0);
        b.u8(1).u8(0).u16(setup);
        let mut bytes = Body::with_capacity(pad + params.len());
        for _ in 0..pad {
            bytes.u8(0);
        }
        bytes.bytes(params);
        b.u16(bytes.0.len() as u16).bytes(&bytes.0);
        self.send(COM_TRANSACTION2, &b.0)?;
        let mut out_params = Vec::new();
        let mut out_data = Vec::new();
        loop {
            let words = self.words()?.to_vec();
            if words.len() < 20 {
                return Err(Error::Malformed("transaction reply"));
            }
            let total_params = get16(&words, 0) as usize;
            let total_data = get16(&words, 2) as usize;
            let (pc, po, pd) = (
                get16(&words, 6) as usize,
                get16(&words, 8) as usize,
                get16(&words, 10) as usize,
            );
            let (dc, dof, dd) = (
                get16(&words, 12) as usize,
                get16(&words, 14) as usize,
                get16(&words, 16) as usize,
            );
            if pc > 0 {
                let p = self
                    .frame
                    .get(po..po + pc)
                    .ok_or(Error::Malformed("transaction parameters"))?;
                if out_params.len() < pd {
                    out_params.resize(pd, 0);
                }
                out_params.truncate(pd);
                out_params.extend_from_slice(p);
            }
            if dc > 0 {
                let d = self
                    .frame
                    .get(dof..dof + dc)
                    .ok_or(Error::Malformed("transaction data"))?;
                if out_data.len() < dd {
                    out_data.resize(dd, 0);
                }
                out_data.truncate(dd);
                out_data.extend_from_slice(d);
            }
            if out_params.len() >= total_params && out_data.len() >= total_data
            {
                return Ok((out_params, out_data));
            }
            // More of the same reply follows, under the same MID.
            self.recv()?;
            self.check()?;
        }
    }

    /// One request (its body: the word count byte, the words, the byte
    /// count and the bytes) and its reply, left in `self.frame`; the
    /// reply's status.
    fn send(&mut self, command: u8, body: &[u8]) -> Result<u32, Error> {
        let mut msg = Vec::with_capacity(HEADER + body.len());
        msg.extend_from_slice(&[0xFF, b'S', b'M', b'B', command]);
        msg.extend_from_slice(&0u32.to_le_bytes());
        msg.push(FLAGS);
        msg.extend_from_slice(&FLAGS2.to_le_bytes());
        msg.extend_from_slice(&[0, 0]); // PIDHigh
        msg.extend_from_slice(&[0u8; 8]); // SecurityFeatures
        msg.extend_from_slice(&[0, 0]); // Reserved
        msg.extend_from_slice(&self.tid.to_le_bytes());
        msg.extend_from_slice(&0xFEFFu16.to_le_bytes());
        msg.extend_from_slice(&self.uid.to_le_bytes());
        msg.extend_from_slice(&self.mid.to_le_bytes());
        msg.extend_from_slice(body);
        self.mid = self.mid.wrapping_add(1).max(1);
        if let Some((key, seq)) = &mut self.signing {
            msg[14..18].copy_from_slice(&seq.to_le_bytes());
            msg[18..22].fill(0);
            let mac = mac(key, &msg);
            msg[14..22].copy_from_slice(&mac);
            *seq += 2;
        }
        let len = msg.len();
        self.stream.write_all(&[
            0,
            (len >> 16) as u8,
            (len >> 8) as u8,
            len as u8,
        ])?;
        self.stream.write_all(&msg)?;
        self.recv()?;
        if self.frame[4] != command {
            return Err(Error::Malformed("reply to another command"));
        }
        self.check()
    }

    fn recv(&mut self) -> Result<(), Error> {
        let mut nb = [0u8; 4];
        self.stream.read_exact(&mut nb).map_err(|e| {
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                Error::Io("the server closed the connection".into())
            } else {
                Error::Io(e.to_string())
            }
        })?;
        let len =
            ((nb[1] as usize) << 16) | ((nb[2] as usize) << 8) | nb[3] as usize;
        if nb[0] != 0 || !(HEADER + 3..=FRAME_LIMIT).contains(&len) {
            return Err(Error::Malformed("frame length"));
        }
        self.frame.resize(len, 0);
        self.stream.read_exact(&mut self.frame)?;
        if self.frame[0..4] != [0xFF, b'S', b'M', b'B'] {
            return Err(Error::Malformed("protocol id"));
        }
        Ok(())
    }

    /// The reply's status; a DOS-class error from a server without
    /// 32-bit status is reported as one.
    fn check(&self) -> Result<u32, Error> {
        let flags2 = get16(&self.frame, 10);
        let status = get32(&self.frame, 5);
        if flags2 & FLAGS2_NT_STATUS == 0 && status != 0 {
            return Err(Error::Auth(format!(
                "DOS error class {} code {}",
                status & 0xFF,
                status >> 16
            )));
        }
        match status {
            status::SUCCESS | status::MORE_PROCESSING_REQUIRED => Ok(status),
            other => Err(Error::Status(other)),
        }
    }

    /// The reply's words.
    fn words(&self) -> Result<&[u8], Error> {
        let count = *self
            .frame
            .get(HEADER)
            .ok_or(Error::Malformed("word count"))?
            as usize;
        self.frame
            .get(HEADER + 1..HEADER + 1 + count * 2)
            .ok_or(Error::Malformed("words"))
    }

    /// The reply's bytes.
    fn bytes(&self) -> Result<&[u8], Error> {
        let count = *self
            .frame
            .get(HEADER)
            .ok_or(Error::Malformed("word count"))?
            as usize;
        let at = HEADER + 1 + count * 2;
        let n = get16(
            self.frame
                .get(at..at + 2)
                .ok_or(Error::Malformed("byte count"))?,
            0,
        ) as usize;
        self.frame
            .get(at + 2..at + 2 + n)
            .ok_or(Error::Malformed("bytes"))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.tid != 0 {
            let mut b = Body::with_capacity(3);
            b.u8(0).u16(0);
            let _ = self.send(COM_TREE_DISCONNECT, &b.0);
        }
        if self.uid != 0 {
            let mut b = Body::with_capacity(8);
            b.u8(2).u8(NO_ANDX).u8(0).u16(0).u16(0);
            let _ = self.send(COM_LOGOFF_ANDX, &b.0);
        }
    }
}

/// The MAC over `msg`, whose SecurityFeatures already carry the
/// sequence number: MD5 of the key and the message, the first eight.
fn mac(key: &[u8], msg: &[u8]) -> [u8; 8] {
    let mut h = Md5::new();
    h.update(key);
    h.update(msg);
    let d = h.finalize();
    let mut out = [0u8; 8];
    out.copy_from_slice(&d[..8]);
    out
}

/// The entries in a FIND reply's data, appended; the last name, for
/// the next call.
fn parse_find(
    mut data: &[u8],
    out: &mut Vec<DirEntry>,
) -> Result<Option<String>, Error> {
    let mut last = None;
    while data.len() >= 64 {
        let next = get32(data, 0) as usize;
        let size = get64(data, 40);
        let attrs = get32(data, 56);
        let name_len = get32(data, 60) as usize;
        let name = data
            .get(64..64 + name_len)
            .ok_or(Error::Malformed("find entry name"))?;
        let name = wire::from_utf16le(name).trim_end_matches('\0').to_string();
        let hidden = attrs & (ATTR_HIDDEN | ATTR_SYSTEM) != 0;
        if name != "." && name != ".." && !hidden {
            out.push(DirEntry {
                name: name.clone(),
                is_dir: attrs & ATTR_DIRECTORY != 0,
                size,
                modified_ms: wire::filetime_to_unix_ms(get64(data, 24)),
            });
        }
        last = Some(name);
        if next == 0 {
            break;
        }
        data = data
            .get(next..)
            .ok_or(Error::Malformed("find entry chain"))?;
    }
    Ok(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_entries_parse() {
        fn entry(name: &str, attrs: u32, size: u64, next: u32) -> Vec<u8> {
            let n = utf16le(name);
            let mut e = vec![0u8; 64];
            wire::put32(&mut e, 0, next);
            wire::put64(&mut e, 40, size);
            wire::put32(&mut e, 56, attrs);
            wire::put32(&mut e, 60, n.len() as u32);
            e.extend_from_slice(&n);
            e
        }
        let mut data = entry("..", ATTR_DIRECTORY, 0, 0);
        let pad = (8 - data.len() % 8) % 8;
        data.resize(data.len() + pad, 0);
        let first_len = data.len() as u32;
        wire::put32(&mut data, 0, first_len);
        data.extend_from_slice(&entry("Song.mp3", 0x20, 1_081_596, 0));
        let mut out = Vec::new();
        let last = parse_find(&data, &mut out).unwrap();
        assert_eq!(last.as_deref(), Some("Song.mp3"));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].size, 1_081_596);
        assert!(!out[0].is_dir);
    }

    #[test]
    fn the_mac_is_the_first_eight_of_md5() {
        let msg = [1u8; 40];
        let m = mac(b"key", &msg);
        let mut h = Md5::new();
        h.update(b"key");
        h.update(msg);
        assert_eq!(&h.finalize()[..8], &m);
    }
}

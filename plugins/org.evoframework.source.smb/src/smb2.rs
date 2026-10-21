//! The SMB2 client: one connection, one session, one tree, spoken in
//! dialects 2.0.2 and 2.1 ([MS-SMB2]). Signing is HMAC-SHA256 when the
//! server asks for it; dialect 3 is not offered, so a server that
//! insists on it is told apart from one that is down.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::ntlm::{self, Credentials};
use crate::spnego;
use crate::wire::{
    self, get16, get32, get64, status, utf16le, Body, Error, Header, Reply,
    FLAG_ASYNC_COMMAND, FLAG_SIGNED, HEADER,
};

const DIALECT_2_0_2: u16 = 0x0202;
const DIALECT_2_1: u16 = 0x0210;
const SECURITY_MODE_SIGNING_ENABLED: u16 = 0x0001;
const SECURITY_MODE_SIGNING_REQUIRED: u16 = 0x0002;
const CAP_LARGE_MTU: u32 = 0x0000_0004;
const SESSION_FLAG_IS_GUEST: u16 = 0x0001;
const SESSION_FLAG_IS_NULL: u16 = 0x0002;

const FILE_READ_DATA: u32 = 0x0000_0001;
const FILE_LIST_DIRECTORY: u32 = 0x0000_0001;
const FILE_READ_ATTRIBUTES: u32 = 0x0000_0080;
const SYNCHRONIZE: u32 = 0x0010_0000;
const FILE_SHARE_ALL: u32 = 0x0000_0007;
const FILE_OPEN: u32 = 0x0000_0001;
const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
const FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;
const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x0000_0020;
const IMPERSONATION: u32 = 0x0000_0002;
const FILE_ATTRIBUTE_HIDDEN: u32 = 0x0000_0002;
const FILE_ATTRIBUTE_SYSTEM: u32 = 0x0000_0004;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
const FILE_DIRECTORY_INFORMATION: u8 = 0x01;
const RESTART_SCANS: u8 = 0x01;

/// The largest read asked for at once: one credit's worth.
pub const READ_CHUNK: u32 = 64 * 1024;
/// How many credits to keep in hand.
const CREDITS_WANTED: u16 = 32;
/// The longest a frame may be before it is refused.
const FRAME_LIMIT: usize = 16 * 1024 * 1024;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// A NAS waking its disks can take a while over the first read.
const IO_TIMEOUT: Duration = Duration::from_secs(20);

/// Where and as whom to connect.
#[derive(Clone)]
pub struct Config {
    /// The server, by name or address.
    pub host: String,
    /// Its port.
    pub port: u16,
    /// The share's name.
    pub share: String,
    /// As whom; none is a guest.
    pub credentials: Option<Credentials>,
    /// The name this client gives for itself.
    pub workstation: String,
}

/// An entry of a listed folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// The file or folder's name.
    pub name: String,
    /// A folder.
    pub is_dir: bool,
    /// A file's length in bytes.
    pub size: u64,
    /// When it was last written, in Unix milliseconds, if the server
    /// said a time in range.
    pub modified_ms: Option<u64>,
}

/// An open file on the share.
#[derive(Debug, Clone, Copy)]
pub struct File {
    /// The server's id for the open.
    pub id: [u8; 16],
    /// Its length in bytes.
    pub size: u64,
}

/// A connected, authenticated session on one share.
pub struct Session {
    stream: TcpStream,
    multi_credit: bool,
    credits: u16,
    next_id: u64,
    session_id: u64,
    tree_id: u32,
    signing_key: Option<[u8; 16]>,
    max_read: u32,
    max_transact: u32,
    frame: Vec<u8>,
    /// The dialect the server chose: 0x0202 or 0x0210.
    pub dialect: u16,
    /// The server's name for itself, from the challenge.
    pub server_name: String,
    /// Whether the server let the session in as a guest.
    pub guest: bool,
}

impl Session {
    /// Connect, negotiate, authenticate and connect to the share.
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
            multi_credit: false,
            credits: 1,
            next_id: 0,
            session_id: 0,
            tree_id: 0,
            signing_key: None,
            max_read: READ_CHUNK,
            max_transact: 64 * 1024,
            frame: Vec::with_capacity(HEADER + 1024),
            dialect: 0,
            server_name: String::new(),
            guest: false,
        };
        let negotiated = s.negotiate().map_err(|e| match e {
            // An SMB1-only server drops the connection on an SMB2
            // negotiate rather than answering it.
            Error::Io(detail)
                if detail.contains("closed")
                    || detail.contains("fill whole buffer")
                    || detail.contains("reset") =>
            {
                Error::Auth(
                    "no answer to SMB2: an SMB1-only box? name it smb1://"
                        .into(),
                )
            }
            other => other,
        })?;
        s.session_setup(cfg, &negotiated)?;
        s.tree_connect(&cfg.host, &cfg.share)?;
        Ok(s)
    }

    /// The entries of the folder at `path` (`a/b`, empty for the root),
    /// hidden and system ones left out.
    pub fn list(&mut self, path: &str) -> Result<Vec<DirEntry>, Error> {
        let id = self.create(
            path,
            FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            FILE_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT,
        )?;
        let listed = self.query_directory(id);
        let _ = self.close(id);
        let (_, entries) = listed?;
        Ok(entries)
    }

    /// Open the file at `path` for reading.
    pub fn open(&mut self, path: &str) -> Result<File, Error> {
        let (id, size) = self.create_full(
            path,
            FILE_READ_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            FILE_NON_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT,
        )?;
        Ok(File { id, size })
    }

    /// Read from `file` at `offset` into `buf`: the bytes read, zero at
    /// the end of the file. At most [`READ_CHUNK`] at once.
    pub fn read(
        &mut self,
        file: &File,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, Error> {
        let want = (buf.len() as u32).min(READ_CHUNK).min(self.max_read);
        if want == 0 || offset >= file.size {
            return Ok(0);
        }
        let mut b = Body::with_capacity(49);
        b.u16(49)
            .u8(0x50)
            .u8(0)
            .u32(want)
            .u64(offset)
            .bytes(&file.id);
        b.u32(0).u32(0).u32(0).u16(0).u16(0).u8(0);
        let reply = self.send(wire::READ, &b.0)?;
        if reply.status == status::END_OF_FILE {
            return Ok(0);
        }
        self.ok(&reply)?;
        let body = &self.frame[HEADER..];
        if body.len() < 16 || get16(body, 0) != 17 {
            return Err(Error::Malformed("read reply"));
        }
        let off = body[2] as usize;
        let len = get32(body, 4) as usize;
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
        self.close(file.id)
    }

    fn negotiate(&mut self) -> Result<Negotiated, Error> {
        let mut guid = [0u8; 16];
        getrandom::fill(&mut guid)
            .map_err(|e| Error::Io(format!("random: {e}")))?;
        let mut b = Body::with_capacity(40);
        b.u16(36)
            .u16(2)
            .u16(SECURITY_MODE_SIGNING_ENABLED)
            .u16(0)
            .u32(0);
        b.bytes(&guid).u64(0).u16(DIALECT_2_0_2).u16(DIALECT_2_1);
        let reply = self.send(wire::NEGOTIATE, &b.0)?;
        self.ok(&reply)?;
        let body = &self.frame[HEADER..];
        if body.len() < 64 || get16(body, 0) != 65 {
            return Err(Error::Malformed("negotiate reply"));
        }
        let dialect = get16(body, 4);
        if dialect != DIALECT_2_0_2 && dialect != DIALECT_2_1 {
            return Err(Error::Auth(format!(
                "server chose dialect {dialect:#06x}"
            )));
        }
        let security_mode = get16(body, 2);
        let capabilities = get32(body, 24);
        self.dialect = dialect;
        self.multi_credit =
            dialect >= DIALECT_2_1 && capabilities & CAP_LARGE_MTU != 0;
        self.max_transact = get32(body, 28).max(1024);
        self.max_read = get32(body, 32).max(1024);
        log::debug!(
            "smb2: dialect {dialect:#06x}, capabilities {capabilities:#x}, security mode {security_mode:#x}, max read {}, max transact {}",
            self.max_read,
            self.max_transact
        );
        Ok(Negotiated {
            signing_required: security_mode & SECURITY_MODE_SIGNING_REQUIRED
                != 0,
            server_time: get64(body, 40),
        })
    }

    fn session_setup(
        &mut self,
        cfg: &Config,
        negotiated: &Negotiated,
    ) -> Result<(), Error> {
        let type1 = ntlm::negotiate();
        let reply =
            self.send(wire::SESSION_SETUP, &setup_body(&spnego::init(&type1)))?;
        if reply.status != status::MORE_PROCESSING_REQUIRED {
            self.ok(&reply)?;
            return Err(Error::Auth("server skipped the challenge".into()));
        }
        self.session_id = reply.session_id;
        let blob = setup_blob(&self.frame)?;
        let type2 = spnego::parse(blob)
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
            negotiated.server_time,
            client_challenge,
            &cfg.workstation,
            &cfg.host,
        );
        // With a session key, the MIC over the mechanism list goes with
        // the answer; a server may still ask for it alone once more.
        let mech_types = spnego::mech_types();
        let our_mic = auth
            .session_key
            .map(|k| ntlm::mech_list_mic(&k, &mech_types));
        let mut reply = self.send(
            wire::SESSION_SETUP,
            &setup_body(&spnego::response(
                Some(&auth.message),
                our_mic.as_ref().map(|m| &m[..]),
            )),
        )?;
        if reply.status == status::MORE_PROCESSING_REQUIRED {
            let theirs =
                spnego::parse(setup_blob(&self.frame)?).map_err(Error::Auth)?;
            if let (Some(key), Some(mic)) = (auth.session_key, theirs.mic) {
                if !ntlm::server_mic_ok(&key, &mech_types, mic) {
                    return Err(Error::Auth(
                        "the server's MIC did not verify".into(),
                    ));
                }
            }
            reply = self.send(
                wire::SESSION_SETUP,
                &setup_body(&spnego::response(
                    None,
                    our_mic.as_ref().map(|m| &m[..]),
                )),
            )?;
        }
        match reply.status {
            status::SUCCESS => {}
            status::LOGON_FAILURE if cfg.credentials.is_none() => {
                return Err(Error::Auth(
                    "guest access refused; set a user and password".into(),
                ));
            }
            other => return Err(Error::Status(other)),
        }
        let body = &self.frame[HEADER..];
        if body.len() < 8 || get16(body, 0) != 9 {
            return Err(Error::Malformed("session setup reply"));
        }
        let flags = get16(body, 2);
        self.guest =
            flags & (SESSION_FLAG_IS_GUEST | SESSION_FLAG_IS_NULL) != 0;
        if let (Some(key), Some(mic)) = (
            auth.session_key,
            spnego::parse(setup_blob(&self.frame)?)
                .ok()
                .and_then(|r| r.mic),
        ) {
            if !ntlm::server_mic_ok(&key, &mech_types, mic) {
                return Err(Error::Auth(
                    "the server's MIC did not verify".into(),
                ));
            }
        }
        // A guest or null session has no key the server shares; signing
        // is for a session that proved a password.
        if let (Some(key), false) = (auth.session_key, self.guest) {
            if negotiated.signing_required || reply.flags & FLAG_SIGNED != 0 {
                // The server signed its acceptance with the key we hold:
                // verifying it proves the exchange before the tree.
                if reply.flags & FLAG_SIGNED != 0 && !verify(&key, &self.frame)
                {
                    return Err(Error::Signature);
                }
                self.signing_key = Some(key);
            }
        } else if negotiated.signing_required {
            log::debug!(
                "smb: the server requires signing but the session is a guest's"
            );
        }
        Ok(())
    }

    fn tree_connect(&mut self, host: &str, share: &str) -> Result<(), Error> {
        let path = utf16le(&format!("\\\\{host}\\{share}"));
        let mut b = Body::with_capacity(8 + path.len());
        b.u16(9)
            .u16(0)
            .u16((HEADER + 8) as u16)
            .u16(path.len() as u16)
            .bytes(&path);
        let reply = self.send(wire::TREE_CONNECT, &b.0)?;
        self.ok(&reply)?;
        self.tree_id = reply.tree_id;
        Ok(())
    }

    fn create(
        &mut self,
        path: &str,
        access: u32,
        options: u32,
    ) -> Result<[u8; 16], Error> {
        self.create_full(path, access, options).map(|(id, _)| id)
    }

    fn create_full(
        &mut self,
        path: &str,
        access: u32,
        options: u32,
    ) -> Result<([u8; 16], u64), Error> {
        let name = utf16le(&path.trim_matches('/').replace('/', "\\"));
        let mut b = Body::with_capacity(56 + name.len().max(1));
        b.u16(57).u8(0).u8(0).u32(IMPERSONATION).u64(0).u64(0);
        b.u32(access)
            .u32(0)
            .u32(FILE_SHARE_ALL)
            .u32(FILE_OPEN)
            .u32(options);
        b.u16((HEADER + 56) as u16)
            .u16(name.len() as u16)
            .u32(0)
            .u32(0);
        if name.is_empty() {
            b.u8(0);
        } else {
            b.bytes(&name);
        }
        let reply = self.send(wire::CREATE, &b.0)?;
        self.ok(&reply)?;
        let body = &self.frame[HEADER..];
        if body.len() < 88 || get16(body, 0) != 89 {
            return Err(Error::Malformed("create reply"));
        }
        let mut id = [0u8; 16];
        id.copy_from_slice(&body[64..80]);
        Ok((id, get64(body, 48)))
    }

    fn close(&mut self, id: [u8; 16]) -> Result<(), Error> {
        let mut b = Body::with_capacity(24);
        b.u16(24).u16(0).u32(0).bytes(&id);
        let reply = self.send(wire::CLOSE, &b.0)?;
        self.ok(&reply)
    }

    fn query_directory(
        &mut self,
        id: [u8; 16],
    ) -> Result<(usize, Vec<DirEntry>), Error> {
        let pattern = utf16le("*");
        let out_len = self.max_transact.min(64 * 1024);
        let mut entries = Vec::new();
        let mut flags = RESTART_SCANS;
        let mut rounds = 0usize;
        loop {
            let mut b = Body::with_capacity(32 + pattern.len());
            b.u16(33)
                .u8(FILE_DIRECTORY_INFORMATION)
                .u8(flags)
                .u32(0)
                .bytes(&id);
            b.u16((HEADER + 32) as u16)
                .u16(pattern.len() as u16)
                .u32(out_len)
                .bytes(&pattern);
            let reply = self.send(wire::QUERY_DIRECTORY, &b.0)?;
            rounds += 1;
            if reply.status == status::NO_MORE_FILES {
                break;
            }
            self.ok(&reply)?;
            let body = &self.frame[HEADER..];
            if body.len() < 8 || get16(body, 0) != 9 {
                return Err(Error::Malformed("query directory reply"));
            }
            let off = get16(body, 2) as usize;
            let len = get32(body, 4) as usize;
            let data = self
                .frame
                .get(off..off + len)
                .ok_or(Error::Malformed("directory data"))?;
            parse_directory(data, &mut entries)?;
            flags = 0;
        }
        Ok((rounds, entries))
    }

    /// One request and its reply; the reply's frame is left in
    /// `self.frame` for the caller to read the body from.
    fn send(&mut self, command: u16, body: &[u8]) -> Result<Reply, Error> {
        if self.credits == 0 {
            return Err(Error::Io("the server granted no credits".into()));
        }
        let charge = u16::from(self.multi_credit);
        let header = Header {
            credit_charge: charge,
            command,
            credits_requested: CREDITS_WANTED
                .saturating_sub(self.credits)
                .max(1),
            message_id: self.next_id,
            tree_id: self.tree_id,
            session_id: self.session_id,
        };
        let expected_id = self.next_id;
        self.next_id += u64::from(charge.max(1));
        self.credits -= 1;
        let mut msg = Vec::with_capacity(HEADER + body.len());
        msg.extend_from_slice(&header.encode());
        msg.extend_from_slice(body);
        if let Some(key) = &self.signing_key {
            wire::put32(&mut msg, 16, FLAG_SIGNED);
            let sig = sign(key, &msg);
            msg[48..64].copy_from_slice(&sig);
        }
        let len = msg.len();
        let nb = [0u8, (len >> 16) as u8, (len >> 8) as u8, len as u8];
        self.stream.write_all(&nb)?;
        self.stream.write_all(&msg)?;
        loop {
            self.recv()?;
            let reply = Reply::parse(&self.frame)?;
            if reply.message_id != expected_id {
                return Err(Error::Malformed("reply to another message"));
            }
            if reply.status == status::PENDING
                && reply.flags & FLAG_ASYNC_COMMAND != 0
            {
                // An interim notice: the real reply follows.
                self.credits =
                    self.credits.saturating_add(reply.credits_granted);
                continue;
            }
            self.credits = self.credits.saturating_add(reply.credits_granted);
            if let Some(key) = &self.signing_key {
                if reply.flags & FLAG_SIGNED != 0 && !verify(key, &self.frame) {
                    return Err(Error::Signature);
                }
            }
            // The statuses a caller expects are not worth a line.
            if !matches!(
                reply.status,
                status::SUCCESS
                    | status::MORE_PROCESSING_REQUIRED
                    | status::END_OF_FILE
                    | status::NO_MORE_FILES
            ) {
                log::warn!(
                    "smb2: {} answered {} ({:#010x})",
                    command_name(command),
                    wire::status_name(reply.status),
                    reply.status
                );
            }
            return Ok(reply);
        }
    }

    fn recv(&mut self) -> Result<(), Error> {
        let mut nb = [0u8; 4];
        self.stream.read_exact(&mut nb)?;
        let len =
            ((nb[1] as usize) << 16) | ((nb[2] as usize) << 8) | nb[3] as usize;
        if nb[0] != 0 || !(HEADER..=FRAME_LIMIT).contains(&len) {
            return Err(Error::Malformed("frame length"));
        }
        self.frame.resize(len, 0);
        self.stream.read_exact(&mut self.frame)?;
        Ok(())
    }

    fn ok(&self, reply: &Reply) -> Result<(), Error> {
        match reply.status {
            status::SUCCESS => Ok(()),
            other => Err(Error::Status(other)),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Best effort: the server tidies up on the close anyway.
        if self.tree_id != 0 {
            let mut b = Body::with_capacity(4);
            b.u16(4).u16(0);
            let _ = self.send(wire::TREE_DISCONNECT, &b.0);
        }
        if self.session_id != 0 {
            let mut b = Body::with_capacity(4);
            b.u16(4).u16(0);
            let _ = self.send(wire::LOGOFF, &b.0);
        }
    }
}

struct Negotiated {
    signing_required: bool,
    server_time: u64,
}

fn command_name(command: u16) -> &'static str {
    match command {
        wire::NEGOTIATE => "negotiate",
        wire::SESSION_SETUP => "session setup",
        wire::LOGOFF => "logoff",
        wire::TREE_CONNECT => "tree connect",
        wire::TREE_DISCONNECT => "tree disconnect",
        wire::CREATE => "create",
        wire::CLOSE => "close",
        wire::READ => "read",
        wire::QUERY_DIRECTORY => "query directory",
        _ => "command",
    }
}

pub(crate) fn resolve(host: &str, port: u16) -> Result<SocketAddr, Error> {
    let addrs: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|e| Error::Io(format!("{host}: {e}")))?
        .collect();
    addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or(addrs.first())
        .copied()
        .ok_or_else(|| Error::Io(format!("{host}: no address")))
}

fn setup_body(blob: &[u8]) -> Vec<u8> {
    let mut b = Body::with_capacity(24 + blob.len());
    b.u16(25)
        .u8(0)
        .u8(SECURITY_MODE_SIGNING_ENABLED as u8)
        .u32(0)
        .u32(0);
    b.u16((HEADER + 24) as u16)
        .u16(blob.len() as u16)
        .u64(0)
        .bytes(blob);
    b.0
}

/// The security blob of a session setup reply, in the frame.
fn setup_blob(frame: &[u8]) -> Result<&[u8], Error> {
    let body = &frame[HEADER..];
    if body.len() < 8 || get16(body, 0) != 9 {
        return Err(Error::Malformed("session setup reply"));
    }
    let off = get16(body, 4) as usize;
    let len = get16(body, 6) as usize;
    frame
        .get(off..off + len)
        .ok_or(Error::Malformed("security blob"))
}

fn parse_directory(
    mut data: &[u8],
    out: &mut Vec<DirEntry>,
) -> Result<(), Error> {
    loop {
        if data.len() < 64 {
            return Err(Error::Malformed("directory entry"));
        }
        let next = get32(data, 0) as usize;
        let size = get64(data, 40);
        let attrs = get32(data, 56);
        let name_len = get32(data, 60) as usize;
        let name = data
            .get(64..64 + name_len)
            .ok_or(Error::Malformed("directory entry name"))?;
        let name = wire::from_utf16le(name);
        let hidden =
            attrs & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM) != 0;
        if name != "." && name != ".." && !hidden {
            out.push(DirEntry {
                name,
                is_dir: attrs & FILE_ATTRIBUTE_DIRECTORY != 0,
                size,
                modified_ms: wire::filetime_to_unix_ms(get64(data, 24)),
            });
        }
        if next == 0 {
            return Ok(());
        }
        data = data
            .get(next..)
            .ok_or(Error::Malformed("directory entry chain"))?;
    }
}

fn sign(key: &[u8; 16], msg: &[u8]) -> [u8; 16] {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(&msg[..48]);
    mac.update(&[0u8; 16]);
    mac.update(&msg[64..]);
    let full = mac.finalize().into_bytes();
    let mut sig = [0u8; 16];
    sig.copy_from_slice(&full[..16]);
    sig
}

fn verify(key: &[u8; 16], msg: &[u8]) -> bool {
    msg.len() >= HEADER && sign(key, msg) == msg[48..64]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signing_round_trips_and_detects_change() {
        let key = [3u8; 16];
        let mut msg = vec![0u8; HEADER + 10];
        msg[0..4].copy_from_slice(&[0xFE, b'S', b'M', b'B']);
        let sig = sign(&key, &msg);
        msg[48..64].copy_from_slice(&sig);
        assert!(verify(&key, &msg));
        msg[70] ^= 1;
        assert!(!verify(&key, &msg));
    }

    #[test]
    fn directory_entries_parse_and_skip_dots_and_hidden() {
        fn entry(name: &str, attrs: u32, size: u64, next: u32) -> Vec<u8> {
            let n = utf16le(name);
            let mut e = vec![0u8; 64];
            wire::put32(&mut e, 0, next);
            wire::put64(&mut e, 24, 116_444_736_000_000_000 + 10_000_000);
            wire::put64(&mut e, 40, size);
            wire::put32(&mut e, 56, attrs);
            wire::put32(&mut e, 60, n.len() as u32);
            e.extend_from_slice(&n);
            e
        }
        let mut data = Vec::new();
        let a = entry(".", FILE_ATTRIBUTE_DIRECTORY, 0, 0);
        let b = entry("Albums", FILE_ATTRIBUTE_DIRECTORY, 0, 0);
        let c = entry(".hidden", FILE_ATTRIBUTE_HIDDEN, 1, 0);
        let d = entry("Sweep 1.flac", 0x80, 532_957, 0);
        for (i, mut e) in [a, b, c, d].into_iter().enumerate() {
            let last = i == 3;
            if !last {
                let len = (e.len() + 7) & !7;
                e.resize(len, 0);
                wire::put32(&mut e, 0, len as u32);
            }
            data.extend_from_slice(&e);
        }
        let mut out = Vec::new();
        parse_directory(&data, &mut out).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].name, "Albums");
        assert!(out[0].is_dir);
        assert_eq!(out[1].name, "Sweep 1.flac");
        assert_eq!(out[1].size, 532_957);
        assert_eq!(out[1].modified_ms, Some(1000));
        assert!(parse_directory(&data[..10], &mut out).is_err());
    }

    #[test]
    fn create_name_is_share_relative_with_backslashes() {
        let name =
            utf16le(&"/Albums/Sweeps/".trim_matches('/').replace('/', "\\"));
        assert_eq!(wire::from_utf16le(&name), "Albums\\Sweeps");
    }

    /// A server of one share with one folder and one file, speaking
    /// just enough SMB 2.1 for an anonymous session: the exchange the
    /// client makes, end to end, with no server on the bench.
    fn serve(file: Vec<u8>) -> u16 {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut setups = 0;
            loop {
                let mut nb = [0u8; 4];
                if s.read_exact(&mut nb).is_err() {
                    return;
                }
                let len = ((nb[1] as usize) << 16)
                    | ((nb[2] as usize) << 8)
                    | nb[3] as usize;
                let mut req = vec![0u8; len];
                s.read_exact(&mut req).unwrap();
                let command = get16(&req, 12);
                let message_id = get64(&req, 24);
                let body = &req[HEADER..];
                let (status, out): (u32, Vec<u8>) = match command {
                    wire::NEGOTIATE => {
                        assert_eq!(get16(body, 0), 36);
                        assert_eq!(get16(body, 2), 2, "two dialects offered");
                        let mut b = Body::with_capacity(65);
                        b.u16(65).u16(0).u16(DIALECT_2_1).u16(0);
                        b.bytes(&[7u8; 16]).u32(CAP_LARGE_MTU);
                        b.u32(65536).u32(65536).u32(65536);
                        b.u64(133_000_000_000_000_000).u64(0);
                        b.u16(0).u16(0).u32(0).u8(0);
                        (status::SUCCESS, b.0)
                    }
                    wire::SESSION_SETUP => {
                        setups += 1;
                        let blob_off = get16(body, 12) as usize;
                        let blob_len = get16(body, 14) as usize;
                        let blob = &req[blob_off..blob_off + blob_len];
                        if setups == 1 {
                            // The NEGOTIATE rides inside a NegTokenInit.
                            let at = blob
                                .windows(8)
                                .position(|w| w == b"NTLMSSP\0")
                                .expect("an NTLMSSP token in the init");
                            let t1 = &blob[at..];
                            assert_eq!(get32(t1, 8), 1, "a NEGOTIATE first");
                            // A challenge with no target info.
                            let mut t2 = vec![0u8; 56];
                            t2[0..8].copy_from_slice(b"NTLMSSP\0");
                            wire::put32(&mut t2, 8, 2);
                            wire::put32(&mut t2, 20, 0x0008_0201);
                            t2[24..32]
                                .copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
                            let token = spnego::response(Some(&t2), None);
                            (
                                status::MORE_PROCESSING_REQUIRED,
                                setup_reply(0, &token),
                            )
                        } else {
                            let t3 =
                                spnego::parse(blob).unwrap().token.unwrap();
                            assert_eq!(
                                get32(t3, 8),
                                3,
                                "an AUTHENTICATE second"
                            );
                            assert_eq!(
                                get16(t3, 20),
                                0,
                                "anonymous: no NT response"
                            );
                            (
                                status::SUCCESS,
                                setup_reply(SESSION_FLAG_IS_NULL, &[]),
                            )
                        }
                    }
                    wire::TREE_CONNECT => {
                        let off = get16(body, 4) as usize;
                        let len = get16(body, 6) as usize;
                        let path = wire::from_utf16le(&req[off..off + len]);
                        assert_eq!(path, "\\\\127.0.0.1\\music");
                        let mut b = Body::with_capacity(16);
                        b.u16(16).u8(1).u8(0).u32(0).u32(0).u32(0x001f_01ff);
                        (status::SUCCESS, b.0)
                    }
                    wire::CREATE => {
                        let off = get16(body, 44) as usize;
                        let len = get16(body, 46) as usize;
                        let name = wire::from_utf16le(&req[off..off + len]);
                        let (id, size, attrs) = match name.as_str() {
                            "" => (1u8, 0u64, FILE_ATTRIBUTE_DIRECTORY),
                            "Sweep 1.flac" => (2, file.len() as u64, 0x80),
                            _ => (0, 0, 0),
                        };
                        if id == 0 {
                            (status::OBJECT_NAME_NOT_FOUND, Vec::new())
                        } else {
                            let mut b = Body::with_capacity(89);
                            b.u16(89).u8(0).u8(0).u32(1);
                            b.u64(0).u64(0).u64(0).u64(0);
                            b.u64(size).u64(size).u32(attrs).u32(0);
                            b.bytes(&[id; 16]).u32(0).u32(0).u8(0);
                            (status::SUCCESS, b.0)
                        }
                    }
                    wire::QUERY_DIRECTORY => {
                        if body[3] & RESTART_SCANS == 0 {
                            (status::NO_MORE_FILES, Vec::new())
                        } else {
                            let mut data = Vec::new();
                            for (i, (name, attrs, size)) in [
                                (".", FILE_ATTRIBUTE_DIRECTORY, 0u64),
                                ("Albums", FILE_ATTRIBUTE_DIRECTORY, 0),
                                ("Sweep 1.flac", 0x80, file.len() as u64),
                            ]
                            .into_iter()
                            .enumerate()
                            {
                                let n = utf16le(name);
                                let mut e = vec![0u8; 64];
                                wire::put64(&mut e, 40, size);
                                wire::put32(&mut e, 56, attrs);
                                wire::put32(&mut e, 60, n.len() as u32);
                                e.extend_from_slice(&n);
                                if i < 2 {
                                    let len = (e.len() + 7) & !7;
                                    e.resize(len, 0);
                                    wire::put32(&mut e, 0, len as u32);
                                }
                                data.extend_from_slice(&e);
                            }
                            let mut b = Body::with_capacity(8 + data.len());
                            b.u16(9)
                                .u16((HEADER + 8) as u16)
                                .u32(data.len() as u32);
                            b.bytes(&data);
                            (status::SUCCESS, b.0)
                        }
                    }
                    wire::READ => {
                        let want = get32(body, 4) as usize;
                        let offset = get64(body, 8) as usize;
                        assert_eq!(body[16], 2, "the file's id");
                        if offset >= file.len() {
                            (status::END_OF_FILE, Vec::new())
                        } else {
                            let end = (offset + want).min(file.len());
                            let mut b = Body::with_capacity(16 + end - offset);
                            b.u16(17).u8((HEADER + 16) as u8).u8(0);
                            b.u32((end - offset) as u32).u32(0).u32(0);
                            b.bytes(&file[offset..end]);
                            (status::SUCCESS, b.0)
                        }
                    }
                    wire::CLOSE => {
                        let mut b = Body::with_capacity(60);
                        b.u16(60).bytes(&[0u8; 58]);
                        (status::SUCCESS, b.0)
                    }
                    wire::TREE_DISCONNECT | wire::LOGOFF => {
                        let mut b = Body::with_capacity(4);
                        b.u16(4).u16(0);
                        (status::SUCCESS, b.0)
                    }
                    other => panic!("command {other:#06x} not served"),
                };
                let mut msg = Vec::with_capacity(HEADER + out.len());
                msg.extend_from_slice(
                    &Header {
                        credit_charge: 0,
                        command,
                        credits_requested: 16,
                        message_id,
                        tree_id: 5,
                        session_id: 0x1234,
                    }
                    .encode(),
                );
                wire::put32(&mut msg, 8, status);
                wire::put32(&mut msg, 16, 0x0000_0001); // SERVER_TO_REDIR
                msg.extend_from_slice(&out);
                let len = msg.len();
                s.write_all(&[
                    0,
                    (len >> 16) as u8,
                    (len >> 8) as u8,
                    len as u8,
                ])
                .unwrap();
                s.write_all(&msg).unwrap();
            }
        });
        port
    }

    fn setup_reply(flags: u16, blob: &[u8]) -> Vec<u8> {
        let mut b = Body::with_capacity(8 + blob.len());
        b.u16(9)
            .u16(flags)
            .u16((HEADER + 8) as u16)
            .u16(blob.len() as u16);
        b.bytes(blob);
        b.0
    }

    #[test]
    fn a_session_lists_opens_and_reads() {
        let file: Vec<u8> = (0..150_000u32).map(|i| (i % 251) as u8).collect();
        let port = serve(file.clone());
        let cfg = Config {
            host: "127.0.0.1".into(),
            port,
            share: "music".into(),
            credentials: None,
            workstation: "test".into(),
        };
        let mut s = Session::connect(&cfg).unwrap();
        assert_eq!(s.dialect, DIALECT_2_1);
        assert!(s.guest);
        assert!(s.multi_credit);
        let entries = s.list("").unwrap();
        assert_eq!(
            entries,
            vec![
                DirEntry {
                    name: "Albums".into(),
                    is_dir: true,
                    size: 0,
                    modified_ms: None
                },
                DirEntry {
                    name: "Sweep 1.flac".into(),
                    is_dir: false,
                    size: 150_000,
                    modified_ms: None
                },
            ]
        );
        let f = s.open("Sweep 1.flac").unwrap();
        assert_eq!(f.size, 150_000);
        let mut buf = vec![0u8; 70_000];
        let n = s.read(&f, 0, &mut buf).unwrap();
        assert_eq!(n, 65_536, "one credit's worth at a time");
        assert_eq!(&buf[..n], &file[..n]);
        let n = s.read(&f, 100_000, &mut buf).unwrap();
        assert_eq!(n, 50_000);
        assert_eq!(&buf[..n], &file[100_000..]);
        assert_eq!(s.read(&f, 150_000, &mut buf).unwrap(), 0);
        s.close_file(&f).unwrap();
        assert_eq!(
            s.open("Missing.flac").unwrap_err(),
            Error::Status(status::OBJECT_NAME_NOT_FOUND)
        );
        assert_eq!(
            Error::Status(status::OBJECT_NAME_NOT_FOUND).to_string(),
            "not found"
        );
    }
}

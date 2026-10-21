// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! A source over SMB shares on the network.
//!
//! `org.evoframework.source.smb` stocks `source/smb`. The shares are
//! named by the distribution (from the card's settings: `share =
//! smb://host/share/folder`, with `share_user`, `share_password` and
//! `share_domain` beside it), each a folder at the root of the listing,
//! and below it the share's own folders and audio files, read through
//! the client in [`smb2`]: SMB 2.0.2 and 2.1 in plain `std::net`, NTLMv2
//! under SPNEGO, signing when the server requires it. A file's first
//! bytes are peeked for its rate and length, as the files source peeks
//! the card's, so the listing says what a player with a rate ceiling
//! cannot play. Tracks are announced as `track` subjects.
//!
//! [`SmbOpener`] is the other half: the playback plugin's way to read a
//! track the listing produced, by position, so a seek costs no reopen.
//! A `smb://` URI that names no configured share is opened as a guest.

#![forbid(unsafe_code)]

mod ntlm;
pub mod smb1;
pub mod smb2;
mod spnego;
mod wire;

use std::fmt::Write as _;

use evo_esp32_sdk::context::Context;
use evo_esp32_sdk::contract::audio::playback::{ByteFile, FileOpener};
use evo_esp32_sdk::contract::audio::source::{Entry, ListArgs, Listing, LIST};
use evo_esp32_sdk::contract::audio::{
    from_value, shelf, to_value, Track, TRACK_KIND,
};
use evo_esp32_sdk::plugin::{
    InstanceShape, InteractionShape, LoadError, Plugin, PluginDescription,
    Stocking,
};
use evo_esp32_sdk::respondent::{Fault, Request, Respondent, Response};
use evo_esp32_sdk::subject::{
    ExternalAddressing, SubjectAnnouncement, SubjectId,
};
use log::{info, warn};
use org_evoframework_source_files::{facts_from_header, is_audio, title_of};

pub use ntlm::Credentials;
pub use smb2::Session;
pub use wire::Error;

/// The plugin's name, as the catalogue and the steward know it.
pub const NAME: &str = "org.evoframework.source.smb";
/// The scheme of a share's URI.
pub const SCHEME: &str = "smb";
/// The scheme of a share that speaks only SMB1, named so on purpose.
pub const SCHEME_SMB1: &str = "smb1";
/// SMB's port.
pub const PORT: u16 = 445;
/// The name this client gives the server for itself.
pub const WORKSTATION: &str = "evo-esp32";
/// How many bytes of a file are peeked for its header.
const PEEK: usize = 64;

static STOCKS: [Stocking; 1] = [Stocking {
    rack: shelf::SOURCE,
    shelf: shelf::SMB,
    shape: shelf::SHAPE,
}];

/// Which family of the protocol a share is spoken in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// SMB 2.0.2 or 2.1: `smb://`.
    Smb2,
    /// SMB1, NT LM 0.12: `smb1://`, never reached from `smb://`.
    Smb1,
}

impl Protocol {
    fn scheme(self) -> &'static str {
        match self {
            Protocol::Smb2 => SCHEME,
            Protocol::Smb1 => SCHEME_SMB1,
        }
    }
}

/// One configured share: where it is, as whom, and what it is called.
#[derive(Clone)]
pub struct Share {
    /// The protocol family its URL named.
    pub protocol: Protocol,
    /// The server, by name or address.
    pub host: String,
    /// Its port; 445 unless the URL says otherwise.
    pub port: u16,
    /// The share's name on the server.
    pub share: String,
    /// The folder within the share the listing starts at, `a/b` or empty.
    pub folder: String,
    /// As whom; none is a guest.
    pub credentials: Option<Credentials>,
    /// What the Network page calls it.
    pub name: String,
}

impl Share {
    /// A share from `smb://host[:port]/share[/folder]` and its
    /// credentials; a user with no password is a user with an empty one.
    pub fn parse(
        url: &str,
        user: Option<&str>,
        password: Option<&str>,
        domain: Option<&str>,
    ) -> Result<Share, String> {
        let (protocol, rest) = if let Some(r) = url.strip_prefix("smb://") {
            (Protocol::Smb2, r)
        } else if let Some(r) = url.strip_prefix("smb1://") {
            (Protocol::Smb1, r)
        } else {
            return Err(format!(
                "{url}: a share starts with smb:// or smb1://"
            ));
        };
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        if authority.is_empty() {
            return Err(format!("{url}: no server named"));
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) if !h.contains(']') || h.ends_with(']') => (
                h.trim_matches(['[', ']']),
                p.parse::<u16>().map_err(|_| {
                    format!("{url}: port {p:?} is not a number")
                })?,
            ),
            _ => (authority, PORT),
        };
        let path = decode_path(path.trim_matches('/'));
        let (share, folder) = match path.split_once('/') {
            Some((s, f)) => (s.to_string(), f.trim_matches('/').to_string()),
            None => (path.clone(), String::new()),
        };
        if share.is_empty() {
            return Err(format!("{url}: no share named after the server"));
        }
        let credentials =
            user.map(str::trim).filter(|u| !u.is_empty()).map(|u| {
                Credentials {
                    user: u.to_string(),
                    password: password.unwrap_or("").to_string(),
                    domain: domain.unwrap_or("").trim().to_string(),
                }
            });
        let name = match folder.rsplit('/').next().filter(|s| !s.is_empty()) {
            Some(last) => format!("{last} on {host}"),
            None => format!("{share} on {host}"),
        };
        Ok(Share {
            protocol,
            host: host.to_string(),
            port,
            share,
            folder,
            credentials,
            name,
        })
    }

    /// The URI of `path` within the share (`a/b/file.flac`, or empty
    /// for the share's root).
    pub fn uri(&self, path: &str) -> String {
        let mut s = format!("{}://{}", self.protocol.scheme(), self.host);
        if self.port != PORT {
            let _ = write!(s, ":{}", self.port);
        }
        let _ = write!(s, "/{}", encode_path(&self.share));
        let path = path.trim_matches('/');
        if !path.is_empty() {
            let _ = write!(s, "/{}", encode_path(path));
        }
        s
    }

    fn config(&self) -> smb2::Config {
        smb2::Config {
            host: self.host.clone(),
            port: self.port,
            share: self.share.clone(),
            credentials: self.credentials.clone(),
            workstation: WORKSTATION.into(),
        }
    }

    /// Connect in the share's protocol.
    fn connect(&self) -> Result<Link, Error> {
        Ok(match self.protocol {
            Protocol::Smb2 => Link::V2(Session::connect(&self.config())?),
            Protocol::Smb1 => Link::V1(smb1::Session::connect(&self.config())?),
        })
    }

    /// Whether `uri` is on this share: same protocol, server, port and
    /// share.
    fn holds(&self, uri: &str) -> Option<String> {
        let other = Share::parse(uri, None, None, None).ok()?;
        (other.protocol == self.protocol
            && other.host.eq_ignore_ascii_case(&self.host)
            && other.port == self.port
            && other.share.eq_ignore_ascii_case(&self.share))
        .then_some(other.folder)
    }
}

/// A session in either family, behind the four calls a listing and a
/// reader make.
pub enum Link {
    /// SMB 2.x.
    V2(Session),
    /// SMB1.
    V1(smb1::Session),
}

/// An open file on either.
#[derive(Debug, Clone, Copy)]
pub enum LinkFile {
    /// On SMB 2.x.
    V2(smb2::File),
    /// On SMB1.
    V1(smb1::File),
}

impl LinkFile {
    /// The file's length.
    pub fn size(&self) -> u64 {
        match self {
            LinkFile::V2(f) => f.size,
            LinkFile::V1(f) => f.size,
        }
    }
}

impl Link {
    /// The dialect for the log: `0x0210`, or `NT LM 0.12`.
    pub fn dialect(&self) -> String {
        match self {
            Link::V2(s) => format!("{:#06x}", s.dialect),
            Link::V1(_) => "NT LM 0.12".into(),
        }
    }

    /// Whether the session is a guest's.
    pub fn guest(&self) -> bool {
        match self {
            Link::V2(s) => s.guest,
            Link::V1(s) => s.guest,
        }
    }

    /// The entries of the folder at `path`.
    pub fn list(&mut self, path: &str) -> Result<Vec<smb2::DirEntry>, Error> {
        match self {
            Link::V2(s) => s.list(path),
            Link::V1(s) => s.list(path),
        }
    }

    /// Open the file at `path`.
    pub fn open(&mut self, path: &str) -> Result<LinkFile, Error> {
        match self {
            Link::V2(s) => s.open(path).map(LinkFile::V2),
            Link::V1(s) => s.open(path).map(LinkFile::V1),
        }
    }

    /// Read from `file` at `offset`.
    pub fn read(
        &mut self,
        file: &LinkFile,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, Error> {
        match (self, file) {
            (Link::V2(s), LinkFile::V2(f)) => s.read(f, offset, buf),
            (Link::V1(s), LinkFile::V1(f)) => s.read(f, offset, buf),
            _ => Err(Error::Malformed("a file of the other protocol")),
        }
    }

    /// Close `file`.
    pub fn close_file(&mut self, file: &LinkFile) -> Result<(), Error> {
        match (self, file) {
            (Link::V2(s), LinkFile::V2(f)) => s.close_file(f),
            (Link::V1(s), LinkFile::V1(f)) => s.close_file(f),
            _ => Err(Error::Malformed("a file of the other protocol")),
        }
    }
}

/// A path percent-encoded for a URI: `/` kept, spaces and the reserved
/// characters escaped.
pub fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for b in path.bytes() {
        match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'.'
            | b'_'
            | b'~'
            | b'/' => out.push(b as char),
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

/// The reverse of [`encode_path`]; a stray `%` is kept as it is.
pub fn decode_path(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&path[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The SMB source.
pub struct SourceSmb {
    shares: Vec<Share>,
    sessions: Vec<Option<Link>>,
    announced: Vec<SubjectId>,
}

impl SourceSmb {
    /// A source over `shares`; nothing is connected until a listing
    /// asks for it.
    pub fn new(shares: Vec<Share>) -> Self {
        let sessions = shares.iter().map(|_| None).collect();
        Self {
            shares,
            sessions,
            announced: Vec::new(),
        }
    }

    /// The shares configured.
    pub fn shares(&self) -> &[Share] {
        &self.shares
    }

    fn list(
        &mut self,
        args: &ListArgs,
        ctx: &mut dyn Context,
    ) -> Result<Listing, Fault> {
        let path = args.path.trim_matches('/');
        if path.is_empty() {
            let entries = self
                .shares
                .iter()
                .map(|s| Entry::Folder {
                    path: s.uri(&s.folder),
                    name: s.name.clone(),
                })
                .collect();
            return Ok(Listing {
                path: String::new(),
                entries,
            });
        }
        let (index, folder) = self
            .shares
            .iter()
            .enumerate()
            .find_map(|(i, s)| s.holds(path).map(|f| (i, f)))
            .ok_or_else(|| Fault::Failed {
                detail: format!("{path}: no such share is set up"),
            })?;
        let share = self.shares[index].clone();
        let listed = self.with_session(index, |session| {
            let mut entries = session.list(&folder)?;
            entries.sort_by(|a, b| {
                b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name))
            });
            let mut out = Vec::with_capacity(entries.len());
            for e in entries {
                let rel = if folder.is_empty() {
                    e.name.clone()
                } else {
                    format!("{folder}/{}", e.name)
                };
                if e.is_dir {
                    out.push(Entry::Folder {
                        path: share.uri(&rel),
                        name: e.name,
                    });
                } else if is_audio(&e.name) {
                    let (sample_rate, duration_ms) =
                        peek(session, &rel, &e.name).unwrap_or_default();
                    out.push(Entry::Track(Track {
                        uri: share.uri(&rel),
                        title: title_of(&e.name),
                        artist: None,
                        album: None,
                        duration_ms,
                        sample_rate,
                        art: None,
                        live: false,
                    }));
                }
            }
            Ok(out)
        });
        let entries = listed.map_err(|e| Fault::Failed {
            detail: format!("{}: {e}", share.name),
        })?;
        for e in &entries {
            if let Entry::Track(t) = e {
                if let Ok(id) = ctx.announce(SubjectAnnouncement {
                    addressing: ExternalAddressing::new(SCHEME, t.uri.clone()),
                    kind: TRACK_KIND.into(),
                    state: to_value(t),
                }) {
                    if !self.announced.contains(&id) {
                        self.announced.push(id);
                    }
                }
            }
        }
        Ok(Listing {
            path: path.to_string(),
            entries,
        })
    }

    /// Run `f` on the share's session, connecting if there is none; a
    /// session that fails is dropped and the call tried once more on a
    /// fresh one, since a server forgets an idle session.
    fn with_session<T>(
        &mut self,
        index: usize,
        mut f: impl FnMut(&mut Link) -> Result<T, Error>,
    ) -> Result<T, Error> {
        for attempt in 0..2 {
            if self.sessions[index].is_none() {
                let share = &self.shares[index];
                let session = share.connect()?;
                info!(
                    "smb: {} on {} as {}: dialect {}{}",
                    share.share,
                    share.host,
                    share
                        .credentials
                        .as_ref()
                        .map(|c| c.user.as_str())
                        .unwrap_or("guest"),
                    session.dialect(),
                    if session.guest() {
                        ", a guest session"
                    } else {
                        ""
                    }
                );
                self.sessions[index] = Some(session);
            }
            let session =
                self.sessions[index].as_mut().expect("just connected");
            match f(session) {
                Ok(v) => return Ok(v),
                Err(Error::Status(code))
                    if code != wire::status::USER_SESSION_DELETED
                        && code != wire::status::NETWORK_SESSION_EXPIRED =>
                {
                    // The share answered: the path is the matter, not
                    // the session.
                    return Err(Error::Status(code));
                }
                Err(e) => {
                    warn!(
                        "smb: {}: {e}; reconnecting",
                        self.shares[index].name
                    );
                    self.sessions[index] = None;
                    if attempt == 1 {
                        return Err(e);
                    }
                }
            }
        }
        unreachable!("two attempts return")
    }
}

/// The rate and length a file's header states, from its first bytes.
fn peek(
    session: &mut Link,
    path: &str,
    name: &str,
) -> Result<(Option<u32>, Option<u64>), Error> {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    if ext != "flac" && ext != "wav" {
        return Ok((None, None));
    }
    let file = session.open(path)?;
    let mut head = [0u8; PEEK];
    let n = session.read(&file, 0, &mut head)?;
    let _ = session.close_file(&file);
    Ok(facts_from_header(name, &head[..n]).unwrap_or_default())
}

impl Plugin for SourceSmb {
    fn describe(&self) -> PluginDescription {
        PluginDescription {
            name: NAME,
            version: env!("CARGO_PKG_VERSION"),
            instance: InstanceShape::Factory,
            interaction: InteractionShape::Respondent,
            stocks: &STOCKS,
        }
    }

    fn load(&mut self, _ctx: &mut dyn Context) -> Result<(), LoadError> {
        Ok(())
    }

    fn unload(&mut self, ctx: &mut dyn Context) {
        for id in self.announced.drain(..) {
            let _ = ctx.retract(&id);
        }
        for s in &mut self.sessions {
            *s = None;
        }
    }
}

impl Respondent for SourceSmb {
    fn handle(&mut self, req: &Request, ctx: &mut dyn Context) -> Response {
        if req.verb != LIST {
            return Response::fault(req, Fault::UnknownVerb);
        }
        let args: ListArgs = match from_value(&req.args) {
            Ok(a) => a,
            Err(detail) => {
                return Response::fault(req, Fault::BadArgs { detail })
            }
        };
        match self.list(&args, ctx) {
            Ok(listing) => Response::ok(req, to_value(&listing)),
            Err(f) => Response::fault(req, f),
        }
    }
}

/// Opens `smb://` (or, as [`SmbOpener::smb1`], `smb1://`) files for
/// the playback plugin, with the shares' credentials where a URI is on
/// a configured share, as a guest otherwise. One session per open
/// file, on the reader's own thread.
pub struct SmbOpener {
    shares: Vec<Share>,
    protocol: Protocol,
}

impl SmbOpener {
    /// The `smb://` opener, with the shares' credentials to hand.
    pub fn new(shares: Vec<Share>) -> Self {
        Self {
            shares,
            protocol: Protocol::Smb2,
        }
    }

    /// The `smb1://` opener, for the shares named so.
    pub fn smb1(shares: Vec<Share>) -> Self {
        Self {
            shares,
            protocol: Protocol::Smb1,
        }
    }
}

impl FileOpener for SmbOpener {
    fn scheme(&self) -> &str {
        self.protocol.scheme()
    }

    fn open(&self, uri: &str) -> Result<Box<dyn ByteFile>, String> {
        let (share, path) = match self
            .shares
            .iter()
            .find_map(|s| s.holds(uri).map(|p| (s.clone(), p)))
        {
            Some(found) => found,
            None => {
                let s = Share::parse(uri, None, None, None)?;
                let path = s.folder.clone();
                (s, path)
            }
        };
        if share.protocol != self.protocol {
            return Err(format!("{uri}: not this opener's protocol"));
        }
        let mut session = share
            .connect()
            .map_err(|e| format!("{}: {e}", share.host))?;
        let file = session.open(&path).map_err(|e| {
            format!("{}: {e}", path.rsplit('/').next().unwrap_or(&path))
        })?;
        Ok(Box::new(SmbFile { session, file }))
    }
}

/// An open file on a share.
struct SmbFile {
    session: Link,
    file: LinkFile,
}

impl ByteFile for SmbFile {
    fn length(&self) -> u64 {
        self.file.size()
    }

    fn read_at(
        &mut self,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, String> {
        self.session
            .read(&self.file, offset, buf)
            .map_err(|e| e.to_string())
    }
}

impl Drop for SmbFile {
    fn drop(&mut self) {
        let _ = self.session.close_file(&self.file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shares_parse() {
        let s = Share::parse(
            "smb://nas.local/Music",
            Some("bench"),
            Some("pw"),
            None,
        )
        .unwrap();
        assert_eq!(
            (s.host.as_str(), s.port, s.share.as_str(), s.folder.as_str()),
            ("nas.local", 445, "Music", "")
        );
        assert_eq!(s.name, "Music on nas.local");
        assert_eq!(
            s.credentials.as_ref().map(|c| c.user.as_str()),
            Some("bench")
        );
        let s = Share::parse(
            "smb://192.0.2.10:1445/music/Albums/Live%20Set/",
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            (s.port, s.share.as_str(), s.folder.as_str()),
            (1445, "music", "Albums/Live Set")
        );
        assert_eq!(s.name, "Live Set on 192.0.2.10");
        assert!(s.credentials.is_none());
        assert_eq!(
            s.uri("Live Set/01 - Sweep 1.flac"),
            "smb://192.0.2.10:1445/music/Live%20Set/01%20-%20Sweep%201.flac"
        );
        assert_eq!(s.uri(""), "smb://192.0.2.10:1445/music");
        assert!(Share::parse("nfs://nas/export", None, None, None).is_err());
        assert!(Share::parse("smb://nas", None, None, None).is_err());
        assert!(Share::parse("smb:///music", None, None, None).is_err());
        // An empty user is no user.
        assert!(Share::parse("smb://nas/m", Some(" "), Some("x"), None)
            .unwrap()
            .credentials
            .is_none());
    }

    #[test]
    fn a_share_knows_its_uris() {
        let s =
            Share::parse("smb://nas/Music/Albums", None, None, None).unwrap();
        assert_eq!(
            s.holds("smb://NAS/music/Albums/x.flac"),
            Some("Albums/x.flac".into())
        );
        assert_eq!(s.holds("smb://nas/Other/x.flac"), None);
        assert_eq!(s.holds("smb://nas:1445/Music/x.flac"), None);
        assert_eq!(s.holds("http://nas/Music/x.flac"), None);
    }

    #[test]
    fn paths_encode_both_ways() {
        let raw = "Björk/Début/01 - Human Behaviour (#1).flac";
        let enc = encode_path(raw);
        assert!(!enc.contains(' ') && !enc.contains('#') && enc.contains('/'));
        assert_eq!(decode_path(&enc), raw);
        assert_eq!(decode_path("100%25"), "100%");
        assert_eq!(decode_path("a%zz"), "a%zz");
        assert_eq!(decode_path("a%"), "a%");
    }

    /// Against a real server named in `EVO_SMB_URL` (with
    /// `EVO_SMB_USER` / `EVO_SMB_PASSWORD`): list the root, open the
    /// first audio file, read its head and a slice from the middle.
    /// Skipped when the variable is not set, so CI needs no server.
    #[test]
    fn against_a_server_when_named() {
        let Ok(url) = std::env::var("EVO_SMB_URL") else {
            return;
        };
        let user = std::env::var("EVO_SMB_USER").ok();
        let password = std::env::var("EVO_SMB_PASSWORD").ok();
        let share =
            Share::parse(&url, user.as_deref(), password.as_deref(), None)
                .unwrap();
        let mut session = share.connect().unwrap();
        eprintln!(
            "connected: dialect {}, guest {}",
            session.dialect(),
            session.guest()
        );
        let entries = session.list(&share.folder).unwrap();
        for e in &entries {
            eprintln!(
                "  {}{} {}",
                e.name,
                if e.is_dir { "/" } else { "" },
                e.size
            );
        }
        let first = entries
            .iter()
            .find(|e| !e.is_dir && is_audio(&e.name))
            .expect("an audio file at the root");
        let path = if share.folder.is_empty() {
            first.name.clone()
        } else {
            format!("{}/{}", share.folder, first.name)
        };
        let file = session.open(&path).unwrap();
        assert_eq!(file.size(), first.size);
        let mut head = [0u8; 64];
        let n = session.read(&file, 0, &mut head).unwrap();
        assert_eq!(n, 64);
        let facts = facts_from_header(&first.name, &head);
        eprintln!(
            "  {}: {} bytes, header says {:?}",
            first.name,
            file.size(),
            facts
        );
        let mut mid = vec![0u8; 70_000];
        let n = session.read(&file, file.size() / 2, &mut mid).unwrap();
        assert!(n >= 16_000, "a chunk's worth, got {n}");
        let n = session.read(&file, file.size() - 10, &mut mid).unwrap();
        assert_eq!(n, 10);
        let n = session.read(&file, file.size(), &mut mid).unwrap();
        assert_eq!(n, 0);
        session.close_file(&file).unwrap();
    }
}

// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! A source over NFS exports on the network.
//!
//! `org.evoframework.source.nfs` stocks `source/nfs`. The exports are
//! named by the distribution (from the card's settings: `share =
//! nfs://host/export/folder`), each a folder at the root of the
//! listing, and below it the export's own folders and audio files,
//! read through the clients in [`nfs4`] and [`nfs3`]: NFS version 4.1
//! where the server speaks it (one connection, a session, no
//! portmapper), version 3 where it does not; over TCP in plain
//! `std::net`, AUTH_SYS as uid and gid 1000. A file's first bytes are
//! peeked for its rate and length, as the files source peeks the
//! card's. Tracks are announced as `track` subjects.
//!
//! [`NfsOpener`] is the other half: the playback plugin's way to read a
//! track the listing produced, by offset, so a seek costs no reopen.

#![forbid(unsafe_code)]

pub mod nfs3;
pub mod nfs4;
mod rpc;

use std::fmt::Write as _;
use std::sync::Arc;

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
use org_evoframework_source_smb::{decode_path, encode_path};

pub use nfs3::{Attr, DirEntry, Handle};
pub use rpc::{Dialer, Error, Identity, StdDialer};

/// A mounted export, by whichever version the server speaks: 4.1 is
/// asked for first; a server with no version 4, or with 4.0 only, is
/// mounted by version 3.
pub enum Export {
    /// NFS 4.1: a session on one connection.
    V4(nfs4::Export),
    /// NFS 3: MOUNT, then NFS.
    V3(nfs3::Export),
}

impl Export {
    /// Mount `cfg.path` on its server.
    pub fn mount(cfg: &nfs3::Config) -> Result<Export, Error> {
        match nfs4::Export::mount(cfg) {
            Ok(e) => Ok(Export::V4(e)),
            // No version 4 at the port (refused, or the program
            // version not there), or 4.0 only: version 3, which also
            // asks the portmapper where the server is.
            Err(Error::Rejected(_))
            | Err(Error::Io(_))
            | Err(Error::Status(nfs4::status::MINOR_VERS_MISMATCH)) => {
                nfs3::Export::mount(cfg).map(Export::V3)
            }
            Err(e) => Err(e),
        }
    }

    /// The version spoken, as text.
    pub fn version(&self) -> &'static str {
        match self {
            Export::V4(_) => "4.1",
            Export::V3(_) => "3",
        }
    }

    /// The handle of the folder the share names.
    pub fn root(&self) -> &Handle {
        match self {
            Export::V4(e) => &e.root,
            Export::V3(e) => &e.root,
        }
    }

    /// The server's largest read.
    pub fn read_max(&self) -> u32 {
        match self {
            Export::V4(e) => e.read_max,
            Export::V3(e) => e.read_max,
        }
    }

    /// The handle and attributes of `name` in `dir`.
    pub fn lookup(
        &mut self,
        dir: &Handle,
        name: &str,
    ) -> Result<(Handle, Attr), Error> {
        match self {
            Export::V4(e) => e.lookup(dir, name),
            Export::V3(e) => e.lookup(dir, name),
        }
    }

    /// Walk `path` (`a/b`) down from the root.
    pub fn walk(&mut self, path: &str) -> Result<(Handle, Attr), Error> {
        match self {
            Export::V4(e) => e.walk(path),
            Export::V3(e) => e.walk(path),
        }
    }

    /// The entries of the folder `dir`.
    pub fn list(&mut self, dir: &Handle) -> Result<Vec<DirEntry>, Error> {
        match self {
            Export::V4(e) => e.list(dir),
            Export::V3(e) => e.list(dir),
        }
    }

    /// Read from `fh` at `offset` into `buf`: the bytes read, zero at
    /// the end.
    pub fn read(
        &mut self,
        fh: &Handle,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, Error> {
        match self {
            Export::V4(e) => e.read(fh, offset, buf),
            Export::V3(e) => e.read(fh, offset, buf),
        }
    }
}

/// The plugin's name, as the catalogue and the steward know it.
pub const NAME: &str = "org.evoframework.source.nfs";
/// The scheme of an export's URI.
pub const SCHEME: &str = "nfs";
/// The identity the client presents: an ordinary user.
pub const UID: u32 = 1000;
/// Its group.
pub const GID: u32 = 1000;
/// The name this client gives the server for itself.
pub const MACHINE: &str = "evo-esp32";
/// How many bytes of a file are peeked for its header.
const PEEK: usize = 64;

static STOCKS: [Stocking; 1] = [Stocking {
    rack: shelf::SOURCE,
    shelf: shelf::NFS,
    shape: shelf::SHAPE,
}];

/// One configured export: where it is and what it is called.
#[derive(Clone)]
pub struct Share {
    /// The server, by name or address.
    pub host: String,
    /// Its NFS port when the URL named one.
    pub port: Option<u16>,
    /// The export and any folder below it, `/volume1/music/albums`.
    pub path: String,
    /// What the Network page calls it.
    pub name: String,
}

impl Share {
    /// A share from `nfs://host[:port]/export[/folder]`.
    pub fn parse(url: &str) -> Result<Share, String> {
        let rest = url
            .strip_prefix("nfs://")
            .ok_or_else(|| format!("{url}: a share starts with nfs://"))?;
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        if authority.is_empty() {
            return Err(format!("{url}: no server named"));
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) if !h.contains(']') || h.ends_with(']') => (
                h.trim_matches(['[', ']']),
                Some(p.parse::<u16>().map_err(|_| {
                    format!("{url}: port {p:?} is not a number")
                })?),
            ),
            _ => (authority, None),
        };
        let path = decode_path(path.trim_matches('/'));
        if path.is_empty() {
            return Err(format!("{url}: no export named after the server"));
        }
        let name =
            format!("{} on {host}", path.rsplit('/').next().unwrap_or(&path));
        Ok(Share {
            host: host.to_string(),
            port,
            path: format!("/{path}"),
            name,
        })
    }

    /// The URI of `rel` within the export (`a/b/file.flac`, or empty
    /// for its root).
    pub fn uri(&self, rel: &str) -> String {
        let mut s = format!("nfs://{}", self.host);
        if let Some(p) = self.port {
            let _ = write!(s, ":{p}");
        }
        let _ = write!(s, "{}", encode_path(&self.path));
        let rel = rel.trim_matches('/');
        if !rel.is_empty() {
            let _ = write!(s, "/{}", encode_path(rel));
        }
        s
    }

    fn config(&self, dialer: &Arc<dyn Dialer>) -> nfs3::Config {
        nfs3::Config {
            host: self.host.clone(),
            port: self.port,
            path: self.path.clone(),
            identity: Identity {
                uid: UID,
                gid: GID,
                machine: MACHINE.into(),
            },
            dialer: Arc::clone(dialer),
        }
    }

    /// Whether `uri` is under this export: the relative path if so.
    fn holds(&self, uri: &str) -> Option<String> {
        let other = Share::parse(uri).ok()?;
        if !other.host.eq_ignore_ascii_case(&self.host)
            || other.port != self.port
        {
            return None;
        }
        let rel = other.path.strip_prefix(&self.path)?;
        if !rel.is_empty() && !rel.starts_with('/') {
            return None;
        }
        Some(rel.trim_matches('/').to_string())
    }
}

/// The NFS source.
pub struct SourceNfs {
    shares: Vec<Share>,
    exports: Vec<Option<Export>>,
    announced: Vec<SubjectId>,
    dialer: Arc<dyn Dialer>,
}

impl SourceNfs {
    /// A source over `shares`; nothing is mounted until a listing asks.
    pub fn new(shares: Vec<Share>) -> Self {
        let exports = shares.iter().map(|_| None).collect();
        Self {
            shares,
            exports,
            announced: Vec::new(),
            dialer: Arc::new(StdDialer),
        }
    }

    /// With the platform's way of connecting: one that binds a reserved
    /// port reaches exports marked `secure`.
    pub fn with_dialer(mut self, dialer: Arc<dyn Dialer>) -> Self {
        self.dialer = dialer;
        self
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
                    path: s.uri(""),
                    name: s.name.clone(),
                })
                .collect();
            return Ok(Listing {
                path: String::new(),
                entries,
            });
        }
        let (index, rel) = self
            .shares
            .iter()
            .enumerate()
            .find_map(|(i, s)| s.holds(path).map(|r| (i, r)))
            .ok_or_else(|| Fault::Failed {
                detail: format!("{path}: no such share is set up"),
            })?;
        let share = self.shares[index].clone();
        let listed = self.with_export(index, |export| {
            let (dir, _) = export.walk(&rel)?;
            let mut entries = export.list(&dir)?;
            entries.sort_by(|a, b| {
                b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name))
            });
            let mut out = Vec::with_capacity(entries.len());
            for e in entries {
                let child = if rel.is_empty() {
                    e.name.clone()
                } else {
                    format!("{rel}/{}", e.name)
                };
                if e.is_dir {
                    out.push(Entry::Folder {
                        path: share.uri(&child),
                        name: e.name,
                    });
                } else if is_audio(&e.name) {
                    let (sample_rate, duration_ms) =
                        peek(export, &dir, &e).unwrap_or_default();
                    out.push(Entry::Track(Track {
                        uri: share.uri(&child),
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

    /// Run `f` on the share's export, mounting if there is none; a
    /// mount that fails on the transport is dropped and tried once
    /// more fresh.
    fn with_export<T>(
        &mut self,
        index: usize,
        mut f: impl FnMut(&mut Export) -> Result<T, Error>,
    ) -> Result<T, Error> {
        for attempt in 0..2 {
            if self.exports[index].is_none() {
                let share = &self.shares[index];
                let export = Export::mount(&share.config(&self.dialer))?;
                info!(
                    "nfs: {} on {}: mounted by NFS {}, reads of {} bytes",
                    share.path,
                    share.host,
                    export.version(),
                    export.read_max()
                );
                self.exports[index] = Some(export);
            }
            let export = self.exports[index].as_mut().expect("just mounted");
            match f(export) {
                Ok(v) => return Ok(v),
                // A stale handle, or a session or lease the server let
                // go while the player was quiet: mount again. Any other
                // answer is the answer.
                Err(Error::Status(code))
                    if !matches!(
                        code,
                        nfs3::status::STALE
                            | nfs4::status::BADSESSION
                            | nfs4::status::EXPIRED
                            | nfs4::status::STALE_CLIENTID
                    ) =>
                {
                    return Err(Error::Status(code));
                }
                Err(e) => {
                    warn!(
                        "nfs: {}: {e}; mounting again",
                        self.shares[index].name
                    );
                    self.exports[index] = None;
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
    export: &mut Export,
    dir: &Handle,
    entry: &DirEntry,
) -> Result<(Option<u32>, Option<u64>), Error> {
    let ext = entry
        .name
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext != "flac" && ext != "wav" {
        return Ok((None, None));
    }
    let fh = match &entry.handle {
        Some(h) => h.clone(),
        None => export.lookup(dir, &entry.name)?.0,
    };
    let mut head = [0u8; PEEK];
    let n = export.read(&fh, 0, &mut head)?;
    Ok(facts_from_header(&entry.name, &head[..n]).unwrap_or_default())
}

impl Plugin for SourceNfs {
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
        for e in &mut self.exports {
            *e = None;
        }
    }
}

impl Respondent for SourceNfs {
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

/// Opens `nfs://` files for the playback plugin. One mount per open
/// file, on the reader's own thread.
pub struct NfsOpener {
    shares: Vec<Share>,
    dialer: Arc<dyn Dialer>,
}

impl NfsOpener {
    /// An opener over the configured shares; a URI on none of them is
    /// mounted as named.
    pub fn new(shares: Vec<Share>) -> Self {
        Self {
            shares,
            dialer: Arc::new(StdDialer),
        }
    }

    /// With the platform's way of connecting.
    pub fn with_dialer(mut self, dialer: Arc<dyn Dialer>) -> Self {
        self.dialer = dialer;
        self
    }
}

impl FileOpener for NfsOpener {
    fn scheme(&self) -> &str {
        SCHEME
    }

    fn open(&self, uri: &str) -> Result<Box<dyn ByteFile>, String> {
        let (share, rel) = match self
            .shares
            .iter()
            .find_map(|s| s.holds(uri).map(|r| (s.clone(), r)))
        {
            Some(found) => found,
            None => {
                // Mount the file's own folder, the file the remainder.
                let mut s = Share::parse(uri)?;
                let (dir, file) =
                    s.path.rsplit_once('/').unwrap_or(("", &s.path));
                let file = file.to_string();
                s.path = if dir.is_empty() {
                    "/".into()
                } else {
                    dir.to_string()
                };
                (s, file)
            }
        };
        let mut export = Export::mount(&share.config(&self.dialer))
            .map_err(|e| format!("{}: {e}", share.host))?;
        let (fh, attr) = export.walk(&rel).map_err(|e| {
            format!("{}: {e}", rel.rsplit('/').next().unwrap_or(&rel))
        })?;
        if !attr.is_file {
            return Err(format!("{rel}: not a file"));
        }
        Ok(Box::new(NfsFile {
            export,
            fh,
            size: attr.size,
        }))
    }
}

/// An open file on an export.
struct NfsFile {
    export: Export,
    fh: Handle,
    size: u64,
}

impl ByteFile for NfsFile {
    fn length(&self) -> u64 {
        self.size
    }

    fn read_at(
        &mut self,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, String> {
        if offset >= self.size {
            return Ok(0);
        }
        self.export
            .read(&self.fh, offset, buf)
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shares_parse_and_name_their_uris() {
        let s = Share::parse("nfs://nas.local/volume1/music/Albums").unwrap();
        assert_eq!(
            (s.host.as_str(), s.port, s.path.as_str()),
            ("nas.local", None, "/volume1/music/Albums")
        );
        assert_eq!(s.name, "Albums on nas.local");
        assert_eq!(s.uri(""), "nfs://nas.local/volume1/music/Albums");
        assert_eq!(s.uri("Live Set/01 - Sweep 1.flac"), "nfs://nas.local/volume1/music/Albums/Live%20Set/01%20-%20Sweep%201.flac");
        assert_eq!(
            s.holds("nfs://NAS.local/volume1/music/Albums/Live%20Set/x.flac"),
            Some("Live Set/x.flac".into())
        );
        assert_eq!(
            s.holds("nfs://nas.local/volume1/music/AlbumsX/x.flac"),
            None
        );
        assert_eq!(
            s.holds("nfs://nas.local:2050/volume1/music/Albums/x.flac"),
            None
        );
        let s = Share::parse("nfs://192.0.2.5:2049/export").unwrap();
        assert_eq!(
            (s.port, s.path.as_str(), s.name.as_str()),
            (Some(2049), "/export", "export on 192.0.2.5")
        );
        assert!(Share::parse("nfs://nas").is_err());
        assert!(Share::parse("smb://nas/x").is_err());
    }

    /// Against a real server named in `EVO_NFS_URL`: mount, list the
    /// root, read the first audio file's head and a slice from the
    /// middle. Skipped when the variable is not set.
    #[test]
    fn against_a_server_when_named() {
        let Ok(url) = std::env::var("EVO_NFS_URL") else {
            return;
        };
        let share = Share::parse(&url).unwrap();
        let dialer: Arc<dyn Dialer> = Arc::new(StdDialer);
        let mut export = Export::mount(&share.config(&dialer)).unwrap();
        eprintln!(
            "mounted {} by NFS {}: reads of {} bytes",
            share.path,
            export.version(),
            export.read_max()
        );
        let root = export.root().clone();
        let entries = export.list(&root).unwrap();
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
        let (fh, attr) = export.lookup(&root, &first.name).unwrap();
        assert_eq!(attr.size, first.size);
        let mut head = [0u8; 64];
        assert_eq!(export.read(&fh, 0, &mut head).unwrap(), 64);
        eprintln!(
            "  {}: header says {:?}",
            first.name,
            facts_from_header(&first.name, &head)
        );
        let mut mid = vec![0u8; 70_000];
        let n = export.read(&fh, attr.size / 2, &mut mid).unwrap();
        assert!(n >= 4096, "got {n}");
        assert_eq!(export.read(&fh, attr.size - 10, &mut mid).unwrap(), 10);
        assert_eq!(export.read(&fh, attr.size, &mut mid).unwrap(), 0);
    }
}

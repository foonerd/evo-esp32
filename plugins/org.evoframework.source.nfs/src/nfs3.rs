//! NFS version 3 ([RFC 1813]): MOUNT3 for the root handle, then
//! LOOKUP, READDIRPLUS (or READDIR and LOOKUP where a server has no
//! PLUS), FSINFO and READ. Reads are by offset, so the player seeks by
//! asking for the next bytes.

use std::sync::Arc;

use crate::rpc::{self, Client, Dialer, Error, Identity, Reader, Writer};

const MOUNT_PROG: u32 = 100_005;
const MOUNT_VERS: u32 = 3;
const MOUNTPROC3_MNT: u32 = 1;
const MOUNTPROC3_UMNT: u32 = 3;

const NFS_PROG: u32 = 100_003;
const NFS_VERS: u32 = 3;
/// The NFS port.
pub const NFS_PORT: u16 = 2049;
const NFSPROC3_GETATTR: u32 = 1;
const NFSPROC3_LOOKUP: u32 = 3;
const NFSPROC3_READ: u32 = 6;
const NFSPROC3_READDIR: u32 = 16;
const NFSPROC3_READDIRPLUS: u32 = 17;
const NFSPROC3_FSINFO: u32 = 19;

const NF3REG: u32 = 1;
const NF3DIR: u32 = 2;

/// NFS3 status codes.
pub mod status {
    /// Success.
    pub const OK: u32 = 0;
    /// Not the owner.
    pub const PERM: u32 = 1;
    /// No such file or folder.
    pub const NOENT: u32 = 2;
    /// A read failed on the server.
    pub const IO: u32 = 5;
    /// Permission denied.
    pub const ACCES: u32 = 13;
    /// Not a folder.
    pub const NOTDIR: u32 = 20;
    /// A folder where a file was expected.
    pub const ISDIR: u32 = 21;
    /// An invalid argument.
    pub const INVAL: u32 = 22;
    /// A name too long.
    pub const NAMETOOLONG: u32 = 63;
    /// A handle the server no longer knows.
    pub const STALE: u32 = 70;
    /// A handle the server never gave.
    pub const BADHANDLE: u32 = 10001;
    /// A procedure the server does not do.
    pub const NOTSUPP: u32 = 10004;
    /// A fault in the server.
    pub const SERVERFAULT: u32 = 10006;
    /// Try again later.
    pub const JUKEBOX: u32 = 10008;
}

/// A status in the listener's words.
pub fn status_name(code: u32) -> String {
    use status::*;
    match code {
        OK => "ok",
        PERM => "not permitted",
        NOENT => "not found",
        IO => "server read error",
        ACCES => "access denied",
        NOTDIR => "not a folder",
        ISDIR => "is a folder",
        INVAL => "invalid argument",
        NAMETOOLONG => "name too long",
        STALE => "stale handle",
        BADHANDLE => "bad handle",
        NOTSUPP => "not supported by the server",
        SERVERFAULT => "server fault",
        JUKEBOX => "server busy, try again",
        _ => return format!("status {code}"),
    }
    .to_string()
}

/// The largest read asked for at once, within what the server allows.
pub const READ_CHUNK: u32 = 64 * 1024;
/// How much a directory page may carry.
const DIR_COUNT: u32 = 16 * 1024;
const DIR_MAX: u32 = 64 * 1024;

/// A file handle: up to 64 bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handle(pub Vec<u8>);

/// An entry of a listed folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// The file or folder's name.
    pub name: String,
    /// A folder.
    pub is_dir: bool,
    /// A file's length in bytes.
    pub size: u64,
    /// Its handle, when the listing carried one.
    pub handle: Option<Handle>,
}

/// What an export is reached by.
#[derive(Clone)]
pub struct Config {
    /// The server, by name or address.
    pub host: String,
    /// The NFS port when the URL named one; otherwise the portmapper
    /// is asked, and 2049 assumed when it does not answer.
    pub port: Option<u16>,
    /// The export, and any folders below it, `/volume1/music/albums`.
    pub path: String,
    /// Who the client says it is.
    pub identity: Identity,
    /// How connections are made.
    pub dialer: Arc<dyn Dialer>,
}

/// A mounted export: its root handle and the connection to NFS.
pub struct Export {
    nfs: Client,
    /// The handle of the folder the share names.
    pub root: Handle,
    /// The server's largest read, capped to [`READ_CHUNK`].
    pub read_max: u32,
}

/// What a server said once and need not be asked again this boot: the
/// ports its portmapper gave, and the root handle MOUNT gave for a
/// path — a version 3 handle outlives the connection, so a mount made
/// again after the server dropped an idle connection is one connection
/// and a GETATTR, not two portmapper calls, MOUNT and a connection.
#[derive(Clone, Default)]
pub struct Remembered {
    /// `(mountd, nfsd)` ports.
    pub ports: Option<(u16, u16)>,
    /// The root handle for `Config::path`, and the read size.
    pub root: Option<(Handle, u32)>,
}

impl Export {
    /// Find the server's ports, mount `cfg.path` (walking up to the
    /// export the server will give out and looking the rest up), and
    /// open the NFS connection. With a remembered root, only the
    /// connection is made and the root checked; a stale one falls
    /// back to the full mount. What was learnt goes into `memo`.
    pub fn mount(cfg: &Config, memo: &mut Remembered) -> Result<Export, Error> {
        let (mount_port, nfs_port) = match memo.ports {
            Some(p) => p,
            None => ports(cfg)?,
        };
        memo.ports = Some((mount_port, nfs_port));
        if let Some((root, read_max)) = memo.root.clone() {
            let nfs = Client::connect(
                &cfg.host,
                nfs_port,
                NFS_PROG,
                NFS_VERS,
                cfg.identity.clone(),
                cfg.dialer.as_ref(),
            )?;
            let mut export = Export {
                nfs,
                root,
                read_max,
            };
            let root = export.root.clone();
            match export.getattr(&root) {
                Ok(_) => return Ok(export),
                // The server forgot the handle (a restart): mount anew.
                Err(Error::Status(status::STALE | status::BADHANDLE)) => {
                    memo.root = None;
                }
                Err(e) => return Err(e),
            }
        }
        let mut mountd = Client::connect(
            &cfg.host,
            mount_port,
            MOUNT_PROG,
            MOUNT_VERS,
            cfg.identity.clone(),
            cfg.dialer.as_ref(),
        )?;
        // The server may export only a parent of the path named: mount
        // the longest it accepts and look the remainder up.
        let mut parts: Vec<&str> =
            cfg.path.split('/').filter(|s| !s.is_empty()).collect();
        let mut remainder: Vec<&str> = Vec::new();
        let root = loop {
            let dirpath = format!("/{}", parts.join("/"));
            let mut w = Writer::with_capacity(8 + dirpath.len());
            w.string(&dirpath);
            let mut r = mountd.call(MOUNTPROC3_MNT, &w.0)?;
            let st = r.u32()?;
            if st == status::OK {
                let fh = Handle(r.opaque()?.to_vec());
                break fh;
            }
            match (st, parts.pop()) {
                (
                    status::NOENT
                    | status::ACCES
                    | status::NOTDIR
                    | status::INVAL,
                    Some(last),
                ) => {
                    remainder.push(last);
                }
                _ => return Err(Error::Status(st)),
            }
        };
        // Tell mountd the mount is not one to keep in its list.
        let mut w = Writer::with_capacity(8 + cfg.path.len());
        w.string(&format!("/{}", parts.join("/")));
        let _ = mountd.call(MOUNTPROC3_UMNT, &w.0);
        drop(mountd);
        let nfs = Client::connect(
            &cfg.host,
            nfs_port,
            NFS_PROG,
            NFS_VERS,
            cfg.identity.clone(),
            cfg.dialer.as_ref(),
        )?;
        let mut export = Export {
            nfs,
            root,
            read_max: READ_CHUNK,
        };
        // Below the mounted export, down to the folder named.
        for name in remainder.into_iter().rev() {
            let root = export.root.clone();
            let (fh, _) = export.lookup(&root, name)?;
            export.root = fh;
        }
        export.read_max = export
            .fsinfo()
            .unwrap_or(READ_CHUNK)
            .clamp(4096, READ_CHUNK);
        memo.root = Some((export.root.clone(), export.read_max));
        Ok(export)
    }

    /// The handle and attributes of `name` in `dir`.
    pub fn lookup(
        &mut self,
        dir: &Handle,
        name: &str,
    ) -> Result<(Handle, Attr), Error> {
        let mut w = Writer::with_capacity(8 + dir.0.len() + name.len());
        w.opaque(&dir.0).string(name);
        let mut r = self.nfs.call(NFSPROC3_LOOKUP, &w.0)?;
        let st = r.u32()?;
        if st != status::OK {
            return Err(Error::Status(st));
        }
        let fh = Handle(r.opaque()?.to_vec());
        let attr = match r.bool()? {
            true => read_attr(&mut r)?,
            false => self.getattr(&fh)?,
        };
        Ok((fh, attr))
    }

    /// The attributes of `fh`.
    pub fn getattr(&mut self, fh: &Handle) -> Result<Attr, Error> {
        let mut w = Writer::with_capacity(8 + fh.0.len());
        w.opaque(&fh.0);
        let mut r = self.nfs.call(NFSPROC3_GETATTR, &w.0)?;
        let st = r.u32()?;
        if st != status::OK {
            return Err(Error::Status(st));
        }
        read_attr(&mut r)
    }

    /// Walk `path` (`a/b`) down from the root.
    pub fn walk(&mut self, path: &str) -> Result<(Handle, Attr), Error> {
        let mut fh = self.root.clone();
        let mut attr = self.getattr(&fh)?;
        for name in path.split('/').filter(|s| !s.is_empty()) {
            let (next, a) = self.lookup(&fh, name)?;
            fh = next;
            attr = a;
        }
        Ok((fh, attr))
    }

    /// The entries of the folder `dir`, with `.` and `..` left out.
    pub fn list(&mut self, dir: &Handle) -> Result<Vec<DirEntry>, Error> {
        match self.readdirplus(dir) {
            Err(Error::Status(status::NOTSUPP)) | Err(Error::Rejected(_)) => {
                self.readdir(dir)
            }
            other => other,
        }
    }

    fn readdirplus(&mut self, dir: &Handle) -> Result<Vec<DirEntry>, Error> {
        // (name, attributes if sent, handle if sent): the reply is read
        // whole before any attribute missing is asked for.
        let mut raw: Vec<(String, Option<Attr>, Option<Handle>)> = Vec::new();
        let mut cookie = 0u64;
        let mut verf = [0u8; 8];
        loop {
            let mut w = Writer::with_capacity(32 + dir.0.len());
            w.opaque(&dir.0)
                .u64(cookie)
                .bytes(&verf)
                .u32(DIR_COUNT)
                .u32(DIR_MAX);
            let mut r = self.nfs.call(NFSPROC3_READDIRPLUS, &w.0)?;
            let st = r.u32()?;
            if st != status::OK {
                return Err(Error::Status(st));
            }
            skip_post_op_attr(&mut r)?;
            verf.copy_from_slice(r.bytes(8)?);
            while r.bool()? {
                let _fileid = r.u64()?;
                let name = r.string()?;
                cookie = r.u64()?;
                let attr = if r.bool()? {
                    Some(read_attr(&mut r)?)
                } else {
                    None
                };
                let handle = if r.bool()? {
                    Some(Handle(r.opaque()?.to_vec()))
                } else {
                    None
                };
                if name != "." && name != ".." && !name.starts_with('.') {
                    raw.push((name, attr, handle));
                }
            }
            if r.bool()? {
                break;
            }
        }
        let mut out = Vec::with_capacity(raw.len());
        for (name, attr, handle) in raw {
            let attr = match (attr, &handle) {
                (Some(a), _) => a,
                (None, Some(h)) => self.getattr(h)?,
                (None, None) => self.lookup(dir, &name)?.1,
            };
            out.push(DirEntry {
                name,
                is_dir: attr.is_dir,
                size: attr.size,
                handle,
            });
        }
        Ok(out)
    }

    fn readdir(&mut self, dir: &Handle) -> Result<Vec<DirEntry>, Error> {
        let mut names = Vec::new();
        let mut cookie = 0u64;
        let mut verf = [0u8; 8];
        loop {
            let mut w = Writer::with_capacity(32 + dir.0.len());
            w.opaque(&dir.0).u64(cookie).bytes(&verf).u32(DIR_COUNT);
            let mut r = self.nfs.call(NFSPROC3_READDIR, &w.0)?;
            let st = r.u32()?;
            if st != status::OK {
                return Err(Error::Status(st));
            }
            skip_post_op_attr(&mut r)?;
            verf.copy_from_slice(r.bytes(8)?);
            while r.bool()? {
                let _fileid = r.u64()?;
                let name = r.string()?;
                cookie = r.u64()?;
                if name != "." && name != ".." && !name.starts_with('.') {
                    names.push(name);
                }
            }
            if r.bool()? {
                break;
            }
        }
        let mut out = Vec::with_capacity(names.len());
        for name in names {
            let (fh, attr) = self.lookup(dir, &name)?;
            out.push(DirEntry {
                name,
                is_dir: attr.is_dir,
                size: attr.size,
                handle: Some(fh),
            });
        }
        Ok(out)
    }

    /// Read from `fh` at `offset` into `buf`: the bytes read, zero at
    /// the end.
    pub fn read(
        &mut self,
        fh: &Handle,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, Error> {
        let want = (buf.len() as u32).min(self.read_max);
        if want == 0 {
            return Ok(0);
        }
        let mut w = Writer::with_capacity(24 + fh.0.len());
        w.opaque(&fh.0).u64(offset).u32(want);
        let mut r = self.nfs.call(NFSPROC3_READ, &w.0)?;
        let st = r.u32()?;
        if st != status::OK {
            return Err(Error::Status(st));
        }
        skip_post_op_attr(&mut r)?;
        let count = r.u32()? as usize;
        let _eof = r.bool()?;
        let data = r.opaque()?;
        let n = data.len().min(count).min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        Ok(n)
    }

    fn fsinfo(&mut self) -> Result<u32, Error> {
        let root = self.root.clone();
        let mut w = Writer::with_capacity(8 + root.0.len());
        w.opaque(&root.0);
        let mut r = self.nfs.call(NFSPROC3_FSINFO, &w.0)?;
        let st = r.u32()?;
        if st != status::OK {
            return Err(Error::Status(st));
        }
        skip_post_op_attr(&mut r)?;
        let rtmax = r.u32()?;
        let rtpref = r.u32()?;
        Ok(rtpref.max(4096).min(rtmax.max(4096)))
    }
}

/// The ports mountd and nfsd listen on: the URL's for both when given,
/// else the portmapper's answer, else the places mountd is found by
/// convention. A NAS pins mountd to 892 (DSM, QNAP, most distributions'
/// `nfs.conf`), newer ones to 20048; a userspace server (unfs3) runs it
/// on the NFS port itself. Each guess is a MOUNT NULL call away, so a
/// wrong one is not mistaken for a server fault. The log says which
/// way the ports were found.
fn ports(cfg: &Config) -> Result<(u16, u16), Error> {
    if let Some(p) = cfg.port {
        return Ok((p, p));
    }
    let dialer = cfg.dialer.as_ref();
    let why = match rpc::getport(
        &cfg.host,
        MOUNT_PROG,
        MOUNT_VERS,
        &cfg.identity,
        dialer,
    ) {
        Ok(Some(mount_port)) => {
            let nfs_port = rpc::getport(
                &cfg.host,
                NFS_PROG,
                NFS_VERS,
                &cfg.identity,
                dialer,
            )?
            .unwrap_or(NFS_PORT);
            log::info!(
                "nfs: {}: the portmapper has mountd at {mount_port}, nfsd at {nfs_port}",
                cfg.host
            );
            return Ok((mount_port, nfs_port));
        }
        Ok(None) => "the portmapper lists no MOUNT service".to_string(),
        Err(Error::Io(e)) => {
            format!("the portmapper could not be reached ({e})")
        }
        Err(e) => return Err(e),
    };
    for port in [892, 20048, NFS_PORT] {
        if mountd_answers(cfg, port) {
            log::info!(
                "nfs: {}: {why}; mountd answers at {port}, nfsd assumed at {NFS_PORT}",
                cfg.host
            );
            return Ok((port, NFS_PORT));
        }
    }
    Err(Error::Rejected(format!(
        "{why}, and no mountd at 892, 20048 or {NFS_PORT}; is NFS enabled on the server?"
    )))
}

/// Whether MOUNT version 3 answers its NULL procedure at `port`.
fn mountd_answers(cfg: &Config, port: u16) -> bool {
    Client::connect(
        &cfg.host,
        port,
        MOUNT_PROG,
        MOUNT_VERS,
        cfg.identity.clone(),
        cfg.dialer.as_ref(),
    )
    .and_then(|mut c| c.call(0, &[]).map(|_| ()))
    .is_ok()
}

/// What a listing needs of fattr3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attr {
    /// A folder.
    pub is_dir: bool,
    /// A regular file.
    pub is_file: bool,
    /// The length in bytes.
    pub size: u64,
}

/// fattr3: 84 bytes.
fn read_attr(r: &mut Reader<'_>) -> Result<Attr, Error> {
    let ftype = r.u32()?;
    r.skip(16)?; // mode, nlink, uid, gid
    let size = r.u64()?;
    r.skip(8 + 8 + 8 + 8 + 24)?; // used, rdev, fsid, fileid, three times
    Ok(Attr {
        is_dir: ftype == NF3DIR,
        is_file: ftype == NF3REG,
        size,
    })
}

fn skip_post_op_attr(r: &mut Reader<'_>) -> Result<(), Error> {
    if r.bool()? {
        read_attr(r)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attributes_read_type_and_size() {
        let mut w = Writer::with_capacity(84);
        w.u32(NF3REG)
            .u32(0o644)
            .u32(1)
            .u32(1000)
            .u32(1000)
            .u64(532_957)
            .u64(540_672);
        w.u32(0)
            .u32(0)
            .u64(7)
            .u64(99)
            .u32(1)
            .u32(2)
            .u32(3)
            .u32(4)
            .u32(5)
            .u32(6);
        assert_eq!(w.0.len(), 84);
        let mut r = Reader::new(&w.0);
        let a = read_attr(&mut r).unwrap();
        assert_eq!((a.is_file, a.is_dir, a.size), (true, false, 532_957));
        assert_eq!(status_name(status::NOENT), "not found");
    }
}

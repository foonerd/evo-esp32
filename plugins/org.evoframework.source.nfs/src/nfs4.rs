// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! NFS version 4.1 ([RFC 8881]): one TCP connection, no portmapper and
//! no MOUNT, a session. EXCHANGE_ID names this client, CREATE_SESSION
//! opens a session with one slot, and every COMPOUND after begins with
//! SEQUENCE on that slot. The export's root is reached from the
//! server's pseudo file system by PUTROOTFH and LOOKUP; a folder is
//! listed by READDIR asking for type, size and file handle per entry;
//! a file is read by READ with the anonymous stateid, which a reader
//! that opens nothing may use ([RFC 8881] §8.2.3). The session is
//! destroyed when the export is dropped, the client id with it.
//!
//! [RFC 8881]: https://www.rfc-editor.org/rfc/rfc8881

use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

use crate::nfs3::{Attr, DirEntry, Handle, NFS_PORT, READ_CHUNK};
use crate::rpc::{Client, Error, Reader, Writer};

const NFS_PROG: u32 = 100_003;
const NFS_VERS: u32 = 4;
const NFSPROC4_COMPOUND: u32 = 1;
const MINOR_VERSION: u32 = 1;

const OP_GETATTR: u32 = 9;
const OP_GETFH: u32 = 10;
const OP_LOOKUP: u32 = 15;
const OP_PUTFH: u32 = 22;
const OP_PUTROOTFH: u32 = 24;
const OP_READ: u32 = 25;
const OP_READDIR: u32 = 26;
const OP_EXCHANGE_ID: u32 = 42;
const OP_CREATE_SESSION: u32 = 43;
const OP_DESTROY_SESSION: u32 = 44;
const OP_SEQUENCE: u32 = 53;
const OP_DESTROY_CLIENTID: u32 = 57;
const OP_RECLAIM_COMPLETE: u32 = 58;

const NF4REG: u32 = 1;
const NF4DIR: u32 = 2;

/// Attribute numbers asked for, word 0 of the bitmap.
const FATTR4_TYPE: u32 = 1;
const FATTR4_SIZE: u32 = 4;
const FATTR4_FILEHANDLE: u32 = 19;

/// The bitmap word 0 asking for type and size; and with the handle.
const WANT_ATTR: u32 = (1 << FATTR4_TYPE) | (1 << FATTR4_SIZE);
const WANT_ATTR_FH: u32 = WANT_ATTR | (1 << FATTR4_FILEHANDLE);

const EXCHGID4_FLAG_SUPP_MOVED_REFER: u32 = 0x0000_0001;
const EXCHGID4_FLAG_USE_NON_PNFS: u32 = 0x0001_0000;
const SP4_NONE: u32 = 0;
const AUTH_SYS: u32 = 1;
const CB_PROGRAM: u32 = 0x4000_0000;

/// What a reply may carry at most, and what is asked of a listing.
const MAX_RESPONSE: u32 = READ_CHUNK + 4096;
const MAX_REQUEST: u32 = 16 * 1024;
const DIR_COUNT: u32 = 16 * 1024;
const DIR_MAX: u32 = 60 * 1024;

/// NFS4 status codes a music player meets.
pub mod status {
    /// Success.
    pub const OK: u32 = 0;
    /// A minor version the server does not speak: 4.0 only.
    pub const MINOR_VERS_MISMATCH: u32 = 10021;
    /// The session is gone (the lease ran out, the server restarted).
    pub const BADSESSION: u32 = 10052;
    /// The client id is gone.
    pub const STALE_CLIENTID: u32 = 10022;
    /// The lease ran out.
    pub const EXPIRED: u32 = 10011;
    /// A stateid the server does not take.
    pub const BAD_STATEID: u32 = 10025;
    /// The server is in its grace period after a restart.
    pub const GRACE: u32 = 10013;
    /// Try again later.
    pub const DELAY: u32 = 10008;
    /// A security flavour the server wants other than AUTH_SYS.
    pub const WRONGSEC: u32 = 10016;
}

/// The name of a status code: the common ones share NFS3's numbers.
pub fn status_name(code: u32) -> String {
    match code {
        status::OK => "ok".into(),
        status::MINOR_VERS_MISMATCH => {
            "the server speaks NFS 4.0 only, not 4.1".into()
        }
        status::BADSESSION => "the session is gone".into(),
        status::STALE_CLIENTID | status::EXPIRED => "the lease ran out".into(),
        status::BAD_STATEID => "the server refuses an anonymous read".into(),
        status::GRACE => "the server is recovering; try again shortly".into(),
        status::DELAY => "the server asks to try again later".into(),
        status::WRONGSEC => {
            "the export wants a security the player has not (Kerberos)".into()
        }
        10036 => "the server could not read the request".into(),
        10044 | 10071 | 10064 | 10063 | 10053 => {
            format!("session protocol error {code}")
        }
        other => crate::nfs3::status_name(other),
    }
}

/// Counts the exports mounted since boot: each is its own client to
/// the server, so two alive at once (a listing while a track plays)
/// do not share a session's slot.
static MOUNTS: AtomicU32 = AtomicU32::new(0);

/// A mounted export: its root handle, the session, the connection.
pub struct Export {
    nfs: Client,
    clientid: u64,
    sessionid: [u8; 16],
    /// The next sequence on slot 0.
    sequence: u32,
    /// The handle of the folder the share names.
    pub root: Handle,
    /// The server's largest read, capped to [`READ_CHUNK`].
    pub read_max: u32,
}

impl Export {
    /// Connect to the NFS port, establish the client and a session, and
    /// walk the pseudo file system down to `cfg.path`. A server that
    /// does not speak version 4 answers the first call with a program
    /// version mismatch ([`Error::Rejected`]); one that speaks 4.0 only
    /// with [`status::MINOR_VERS_MISMATCH`].
    pub fn mount(cfg: &crate::nfs3::Config) -> Result<Export, Error> {
        let nfs = Client::connect(
            &cfg.host,
            cfg.port.unwrap_or(NFS_PORT),
            NFS_PROG,
            NFS_VERS,
            cfg.identity.clone(),
            cfg.dialer.as_ref(),
        )?;
        let mut export = Export {
            nfs,
            clientid: 0,
            sessionid: [0; 16],
            sequence: 1,
            root: Handle(Vec::new()),
            read_max: READ_CHUNK,
        };
        let nth = MOUNTS.fetch_add(1, Relaxed);
        let sequence = export.exchange_id(&cfg.identity.machine, nth)?;
        export.create_session(sequence, &cfg.identity)?;
        export.reclaim_complete()?;
        // The pseudo file system: the root, then the path's names.
        let mut c = export.compound();
        c.op(OP_PUTROOTFH);
        let names: Vec<&str> =
            cfg.path.split('/').filter(|s| !s.is_empty()).collect();
        for name in &names {
            c.op(OP_LOOKUP).string(name);
        }
        c.op(OP_GETFH);
        let mut r = export.call(c)?;
        r.result(OP_PUTROOTFH)?;
        for _ in &names {
            r.result(OP_LOOKUP)?;
        }
        r.result(OP_GETFH)?;
        let root = Handle(r.r.opaque()?.to_vec());
        export.root = root;
        Ok(export)
    }

    fn exchange_id(&mut self, machine: &str, nth: u32) -> Result<u32, Error> {
        let mut c = self.bare();
        let owner = format!("evo-esp32/{machine}/{nth}");
        c.op(OP_EXCHANGE_ID)
            .bytes(&verifier())
            .opaque(owner.as_bytes())
            .u32(EXCHGID4_FLAG_SUPP_MOVED_REFER | EXCHGID4_FLAG_USE_NON_PNFS)
            .u32(SP4_NONE)
            .u32(1)
            .string("evoframework.org")
            .string(concat!("evo-esp32 ", env!("CARGO_PKG_VERSION")))
            .u64(0)
            .u32(0);
        let mut r = self.call(c)?;
        r.result(OP_EXCHANGE_ID)?;
        let clientid = r.r.u64()?;
        let sequence = r.r.u32()?;
        self.clientid = clientid;
        Ok(sequence)
    }

    fn create_session(
        &mut self,
        sequence: u32,
        identity: &crate::rpc::Identity,
    ) -> Result<(), Error> {
        let mut c = self.bare();
        c.op(OP_CREATE_SESSION)
            .u64(self.clientid)
            .u32(sequence)
            .u32(0);
        // Fore channel: one slot, replies up to a read.
        c.u32(0)
            .u32(MAX_REQUEST)
            .u32(MAX_RESPONSE)
            .u32(4096)
            .u32(8)
            .u32(1)
            .u32(0);
        // Back channel: nothing is expected on it.
        c.u32(0).u32(4096).u32(4096).u32(4096).u32(2).u32(1).u32(0);
        c.u32(CB_PROGRAM);
        c.u32(1).u32(AUTH_SYS);
        c.u32(0)
            .string(&identity.machine)
            .u32(identity.uid)
            .u32(identity.gid)
            .u32(0);
        let mut r = self.call(c)?;
        r.result(OP_CREATE_SESSION)?;
        let mut sessionid = [0u8; 16];
        sessionid.copy_from_slice(r.r.bytes(16)?);
        let _sequence = r.r.u32()?;
        let _flags = r.r.u32()?;
        // The fore channel the server settled on: the response size
        // bounds a read.
        let _headerpad = r.r.u32()?;
        let _maxreq = r.r.u32()?;
        let maxresp = r.r.u32()?;
        self.sessionid = sessionid;
        self.read_max = maxresp.saturating_sub(1024).clamp(4096, READ_CHUNK);
        self.sequence = 1;
        Ok(())
    }

    fn reclaim_complete(&mut self) -> Result<(), Error> {
        let mut c = self.compound();
        c.op(OP_RECLAIM_COMPLETE).u32(0);
        let mut r = self.call(c)?;
        r.result(OP_RECLAIM_COMPLETE)?;
        Ok(())
    }

    /// The handle and attributes of `name` in `dir`.
    pub fn lookup(
        &mut self,
        dir: &Handle,
        name: &str,
    ) -> Result<(Handle, Attr), Error> {
        let mut c = self.compound();
        c.op(OP_PUTFH).opaque(&dir.0);
        c.op(OP_LOOKUP).string(name);
        c.op(OP_GETFH);
        c.op(OP_GETATTR).u32(1).u32(WANT_ATTR);
        let mut r = self.call(c)?;
        r.result(OP_PUTFH)?;
        r.result(OP_LOOKUP)?;
        r.result(OP_GETFH)?;
        let fh = Handle(r.r.opaque()?.to_vec());
        r.result(OP_GETATTR)?;
        let (attr, _) = read_fattr(&mut r.r)?;
        Ok((fh, attr))
    }

    /// The attributes of `fh`.
    pub fn getattr(&mut self, fh: &Handle) -> Result<Attr, Error> {
        let mut c = self.compound();
        c.op(OP_PUTFH).opaque(&fh.0);
        c.op(OP_GETATTR).u32(1).u32(WANT_ATTR);
        let mut r = self.call(c)?;
        r.result(OP_PUTFH)?;
        r.result(OP_GETATTR)?;
        Ok(read_fattr(&mut r.r)?.0)
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

    /// The entries of the folder `dir`, hidden names left out, each
    /// with its handle.
    pub fn list(&mut self, dir: &Handle) -> Result<Vec<DirEntry>, Error> {
        let mut out = Vec::new();
        let mut cookie = 0u64;
        let mut verf = [0u8; 8];
        loop {
            let mut c = self.compound();
            c.op(OP_PUTFH).opaque(&dir.0);
            c.op(OP_READDIR)
                .u64(cookie)
                .bytes(&verf)
                .u32(DIR_COUNT)
                .u32(DIR_MAX)
                .u32(1)
                .u32(WANT_ATTR_FH);
            let mut r = self.call(c)?;
            r.result(OP_PUTFH)?;
            r.result(OP_READDIR)?;
            verf.copy_from_slice(r.r.bytes(8)?);
            while r.r.bool()? {
                cookie = r.r.u64()?;
                let name = r.r.string()?;
                let (attr, handle) = read_fattr(&mut r.r)?;
                if !name.starts_with('.') {
                    out.push(DirEntry {
                        name,
                        is_dir: attr.is_dir,
                        size: attr.size,
                        handle,
                    });
                }
            }
            if r.r.bool()? {
                break;
            }
        }
        // A server that left the handle out of a listing: look it up.
        for e in out.iter_mut().filter(|e| e.handle.is_none()) {
            e.handle = Some(self.lookup(dir, &e.name)?.0);
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
        let mut c = self.compound();
        c.op(OP_PUTFH).opaque(&fh.0);
        // The anonymous stateid: seqid 0, other all zero.
        c.op(OP_READ).u32(0).bytes(&[0u8; 12]).u64(offset).u32(want);
        let mut r = self.call(c)?;
        r.result(OP_PUTFH)?;
        r.result(OP_READ)?;
        let _eof = r.r.bool()?;
        let data = r.r.opaque()?;
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        Ok(n)
    }

    /// A compound that begins with SEQUENCE on the session's one slot.
    fn compound(&mut self) -> Compound {
        let mut c = Compound::new();
        c.op(OP_SEQUENCE)
            .bytes(&self.sessionid)
            .u32(self.sequence)
            .u32(0)
            .u32(0)
            .u32(0);
        c.sequenced = true;
        c
    }

    /// A compound outside any session: EXCHANGE_ID, CREATE_SESSION.
    fn bare(&self) -> Compound {
        Compound::new()
    }

    /// Send `c`; the reply positioned at its first result (after the
    /// SEQUENCE result when there was one). A failed status is the
    /// error, with the operation that failed read off first.
    fn call(&mut self, c: Compound) -> Result<Results<'_>, Error> {
        let sequenced = c.sequenced;
        let bytes = c.finish();
        if sequenced {
            self.sequence = self.sequence.wrapping_add(1);
        }
        let mut r = self.nfs.call(NFSPROC4_COMPOUND, &bytes)?;
        let st = r.u32()?;
        let _tag = r.opaque()?;
        let count = r.u32()?;
        let mut results = Results { r, left: count };
        if sequenced {
            results.result(OP_SEQUENCE)?;
            // sessionid, sequenceid, slotid, highest, target, flags
            results.r.skip(16 + 4 * 5)?;
        }
        if st != status::OK && results.left == 0 {
            return Err(Error::Status(st));
        }
        Ok(results)
    }
}

impl Drop for Export {
    fn drop(&mut self) {
        if self.sessionid == [0; 16] {
            return;
        }
        let mut c = Compound::new();
        c.op(OP_DESTROY_SESSION).bytes(&self.sessionid);
        let _ = self.nfs.call(NFSPROC4_COMPOUND, &c.finish());
        let mut c = Compound::new();
        c.op(OP_DESTROY_CLIENTID).u64(self.clientid);
        let _ = self.nfs.call(NFSPROC4_COMPOUND, &c.finish());
    }
}

/// An eight-byte verifier for the client owner: one per boot, so a
/// restart of the player is a new client to the server.
fn verifier() -> [u8; 8] {
    use std::sync::OnceLock;
    static V: OnceLock<[u8; 8]> = OnceLock::new();
    *V.get_or_init(|| {
        let mut v = [0u8; 8];
        let _ = getrandom::fill(&mut v);
        v
    })
}

/// A COMPOUND under construction: tag, minor version, the operations.
struct Compound {
    w: Writer,
    ops: u32,
    sequenced: bool,
}

impl Compound {
    fn new() -> Self {
        let mut w = Writer::with_capacity(256);
        w.u32(0).u32(MINOR_VERSION).u32(0);
        Compound {
            w,
            ops: 0,
            sequenced: false,
        }
    }

    fn op(&mut self, code: u32) -> &mut Writer {
        self.ops += 1;
        self.w.u32(code)
    }

    fn u32(&mut self, v: u32) -> &mut Writer {
        self.w.u32(v)
    }

    fn finish(mut self) -> Vec<u8> {
        // The operation count sits after the empty tag and the minor
        // version.
        self.w.0[8..12].copy_from_slice(&self.ops.to_be_bytes());
        self.w.0
    }
}

/// A COMPOUND's results, read in order.
struct Results<'a> {
    r: Reader<'a>,
    left: u32,
}

impl Results<'_> {
    /// The next result must be `op`'s and succeed; the reader is then
    /// at its body.
    fn result(&mut self, op: u32) -> Result<(), Error> {
        if self.left == 0 {
            return Err(Error::Malformed("compound result missing"));
        }
        self.left -= 1;
        let code = self.r.u32()?;
        if code != op {
            return Err(Error::Malformed("compound result out of order"));
        }
        let st = self.r.u32()?;
        if st != status::OK {
            return Err(Error::Status(st));
        }
        Ok(())
    }
}

/// fattr4: the attribute mask, then the values of the attributes set,
/// in attribute order. Type and size are read; a file handle too when
/// the listing carried one.
fn read_fattr(r: &mut Reader<'_>) -> Result<(Attr, Option<Handle>), Error> {
    let words = r.u32()? as usize;
    let mut mask = [0u32; 4];
    for (i, slot) in mask.iter_mut().enumerate() {
        if i < words {
            *slot = r.u32()?;
        }
    }
    for _ in 4..words {
        r.u32()?;
    }
    let vals = r.opaque()?;
    let mut v = Reader::new(vals);
    let set = |bit: u32| mask[0] & (1 << bit) != 0;
    let mut attr = Attr {
        is_dir: false,
        is_file: false,
        size: 0,
    };
    let mut handle = None;
    // Word 0 attributes in order: supported_attrs(0), type(1),
    // fh_expire_type(2), change(3), size(4), link_support(5),
    // symlink_support(6), named_attr(7), fsid(8), unique_handles(9),
    // lease_time(10), rdattr_error(11), acl(12), aclsupport(13),
    // archive(14), cansettime(15), case_insensitive(16),
    // case_preserving(17), chown_restricted(18), filehandle(19).
    // Only what was asked for comes back, so only those are read.
    if set(FATTR4_TYPE) {
        let t = v.u32()?;
        attr.is_dir = t == NF4DIR;
        attr.is_file = t == NF4REG;
    }
    if set(FATTR4_SIZE) {
        attr.size = v.u64()?;
    }
    if set(FATTR4_FILEHANDLE) {
        handle = Some(Handle(v.opaque()?.to_vec()));
    }
    Ok((attr, handle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_compound_counts_its_operations_and_a_fattr_reads_in_order() {
        let mut c = Compound::new();
        c.op(OP_PUTROOTFH);
        c.op(OP_LOOKUP).string("music");
        c.op(OP_GETFH);
        let bytes = c.finish();
        // tag (empty), minor 1, three ops; PUTROOTFH first.
        assert_eq!(
            &bytes[..16],
            &[0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 3, 0, 0, 0, 24]
        );

        let mut w = Writer::with_capacity(64);
        w.u32(1).u32(WANT_ATTR_FH);
        let mut vals = Writer::with_capacity(32);
        vals.u32(NF4REG).u64(123_456).opaque(&[9, 8, 7]);
        w.opaque(&vals.0);
        let mut r = Reader::new(&w.0);
        let (a, h) = read_fattr(&mut r).unwrap();
        assert_eq!((a.is_file, a.is_dir, a.size), (true, false, 123_456));
        assert_eq!(h, Some(Handle(vec![9, 8, 7])));
        assert_eq!(status_name(2), "not found");
        assert!(status_name(10052).contains("session"));
    }
}

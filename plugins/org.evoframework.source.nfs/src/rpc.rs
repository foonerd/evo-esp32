//! SunRPC over TCP ([RFC 5531]) with record marking ([RFC 5531] §11),
//! AUTH_SYS credentials, and the XDR a client needs ([RFC 4506]).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

/// The portmapper: program 100000, version 2, on port 111.
pub const PORTMAP_PROG: u32 = 100_000;
pub const PORTMAP_VERS: u32 = 2;
pub const PORTMAP_PORT: u16 = 111;
const PMAPPROC_GETPORT: u32 = 3;
const IPPROTO_TCP: u32 = 6;

const CALL: u32 = 0;
const REPLY: u32 = 1;
const RPC_VERSION: u32 = 2;
const MSG_ACCEPTED: u32 = 0;
const AUTH_NONE: u32 = 0;
const AUTH_SYS: u32 = 1;

/// A fragment longer than this is refused.
const FRAGMENT_LIMIT: usize = 4 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const IO_TIMEOUT: Duration = Duration::from_secs(20);

/// What a call can fail with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The transport.
    Io(String),
    /// A reply the parser could not read.
    Malformed(&'static str),
    /// The server did not accept the call: program or version not
    /// there, or the credentials refused.
    Rejected(String),
    /// The procedure answered with a status other than success.
    Status(u32),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => f.write_str(e),
            Error::Malformed(what) => write!(f, "malformed {what}"),
            Error::Rejected(why) => f.write_str(why),
            Error::Status(code) => {
                f.write_str(&crate::nfs3::status_name(*code))
            }
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e.to_string())
    }
}

/// The identity AUTH_SYS carries: a uid, a gid and a machine name.
#[derive(Clone)]
pub struct Identity {
    /// The user id.
    pub uid: u32,
    /// The group id.
    pub gid: u32,
    /// The client's name for itself.
    pub machine: String,
}

/// How the client reaches a server: a distribution's way of making a
/// TCP connection. A knfsd export marked `secure` (the usual default)
/// takes mounts only from ports below 1024, which the ordinary
/// connect cannot bind; a platform that can gives a dialer that does.
pub trait Dialer: Send + Sync {
    /// Connect to `addr` within `timeout`.
    fn dial(
        &self,
        addr: SocketAddr,
        timeout: Duration,
    ) -> std::io::Result<TcpStream>;
}

/// The ordinary connect, from whatever port the system picks.
pub struct StdDialer;

impl Dialer for StdDialer {
    fn dial(
        &self,
        addr: SocketAddr,
        timeout: Duration,
    ) -> std::io::Result<TcpStream> {
        TcpStream::connect_timeout(&addr, timeout)
    }
}

/// One TCP connection to one RPC program.
pub struct Client {
    stream: TcpStream,
    prog: u32,
    vers: u32,
    xid: u32,
    identity: Identity,
    reply: Vec<u8>,
}

impl Client {
    /// Connect to `prog` version `vers` at `host:port`.
    pub fn connect(
        host: &str,
        port: u16,
        prog: u32,
        vers: u32,
        identity: Identity,
        dialer: &dyn Dialer,
    ) -> Result<Client, Error> {
        let addr = resolve(host, port)?;
        let stream = dialer
            .dial(addr, CONNECT_TIMEOUT)
            .map_err(|e| Error::Io(format!("connect {host}:{port}: {e}")))?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        stream.set_nodelay(true)?;
        let mut seed = [0u8; 4];
        getrandom::fill(&mut seed)
            .map_err(|e| Error::Io(format!("random: {e}")))?;
        Ok(Client {
            stream,
            prog,
            vers,
            xid: u32::from_le_bytes(seed),
            identity,
            reply: Vec::with_capacity(1024),
        })
    }

    /// One call: `proc` with `args`; the reply's results, in `self.reply`
    /// from the offset returned.
    pub fn call(
        &mut self,
        proc_: u32,
        args: &[u8],
    ) -> Result<Reader<'_>, Error> {
        self.xid = self.xid.wrapping_add(1);
        let mut w = Writer::with_capacity(64 + args.len());
        w.u32(self.xid)
            .u32(CALL)
            .u32(RPC_VERSION)
            .u32(self.prog)
            .u32(self.vers)
            .u32(proc_);
        // AUTH_SYS credentials, AUTH_NONE verifier.
        let mut cred = Writer::with_capacity(32 + self.identity.machine.len());
        cred.u32(0)
            .string(&self.identity.machine)
            .u32(self.identity.uid)
            .u32(self.identity.gid)
            .u32(0);
        w.u32(AUTH_SYS).opaque(&cred.0);
        w.u32(AUTH_NONE).u32(0);
        w.bytes(args);
        let record = w.0;
        let marker = (record.len() as u32) | 0x8000_0000;
        self.stream.write_all(&marker.to_be_bytes())?;
        self.stream.write_all(&record)?;
        loop {
            self.recv()?;
            let mut r = Reader::new(&self.reply);
            let xid = r.u32()?;
            if xid != self.xid {
                // A stale reply from a call that timed out: skip it.
                continue;
            }
            if r.u32()? != REPLY {
                return Err(Error::Malformed("not a reply"));
            }
            if r.u32()? != MSG_ACCEPTED {
                let reject = r.u32()?;
                return Err(Error::Rejected(match reject {
                    0 => "RPC version refused".to_string(),
                    _ => {
                        let auth = r.u32().unwrap_or(0);
                        format!("credentials refused (auth status {auth})")
                    }
                }));
            }
            // The verifier, then the acceptance.
            let _flavor = r.u32()?;
            let _verf = r.opaque()?;
            let accepted = r.u32()?;
            let at = r.at;
            return match accepted {
                0 => Ok(Reader {
                    data: &self.reply,
                    at,
                }),
                1 => Err(Error::Rejected(
                    "program not available on the server".into(),
                )),
                2 => Err(Error::Rejected(
                    "program version not available on the server".into(),
                )),
                3 => Err(Error::Rejected(
                    "procedure not available on the server".into(),
                )),
                4 => Err(Error::Rejected(
                    "the server could not read the call".into(),
                )),
                _ => Err(Error::Rejected("server error".into())),
            };
        }
    }

    /// The reply's record: fragments gathered until the last.
    fn recv(&mut self) -> Result<(), Error> {
        self.reply.clear();
        loop {
            let mut mark = [0u8; 4];
            self.stream.read_exact(&mut mark).map_err(|e| {
                if e.kind() == std::io::ErrorKind::UnexpectedEof {
                    Error::Io("the server closed the connection".into())
                } else {
                    Error::Io(e.to_string())
                }
            })?;
            let marker = u32::from_be_bytes(mark);
            let len = (marker & 0x7fff_ffff) as usize;
            if len > FRAGMENT_LIMIT || self.reply.len() + len > FRAGMENT_LIMIT {
                return Err(Error::Malformed("fragment length"));
            }
            let start = self.reply.len();
            self.reply.resize(start + len, 0);
            self.stream.read_exact(&mut self.reply[start..])?;
            if marker & 0x8000_0000 != 0 {
                return Ok(());
            }
        }
    }
}

/// Ask the portmapper at `host` where `prog` version `vers` listens
/// over TCP; none when it is not registered.
pub fn getport(
    host: &str,
    prog: u32,
    vers: u32,
    identity: &Identity,
    dialer: &dyn Dialer,
) -> Result<Option<u16>, Error> {
    let mut pm = Client::connect(
        host,
        PORTMAP_PORT,
        PORTMAP_PROG,
        PORTMAP_VERS,
        identity.clone(),
        dialer,
    )?;
    let mut w = Writer::with_capacity(16);
    w.u32(prog).u32(vers).u32(IPPROTO_TCP).u32(0);
    let mut r = pm.call(PMAPPROC_GETPORT, &w.0)?;
    let port = r.u32()?;
    Ok(u16::try_from(port).ok().filter(|p| *p != 0))
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

/// An XDR writer.
pub struct Writer(pub Vec<u8>);

impl Writer {
    pub fn with_capacity(n: usize) -> Self {
        Writer(Vec::with_capacity(n))
    }
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    /// Variable-length opaque: the length, the bytes, padding to four.
    pub fn opaque(&mut self, b: &[u8]) -> &mut Self {
        self.u32(b.len() as u32);
        self.bytes(b)
    }
    /// Fixed-length opaque: the bytes, padding to four.
    pub fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.0.extend_from_slice(b);
        for _ in 0..((4 - b.len() % 4) % 4) {
            self.0.push(0);
        }
        self
    }
    pub fn string(&mut self, s: &str) -> &mut Self {
        self.opaque(s.as_bytes())
    }
}

/// An XDR reader over a reply.
pub struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Reader { data, at: 0 }
    }
    pub fn u32(&mut self) -> Result<u32, Error> {
        let b = self
            .data
            .get(self.at..self.at + 4)
            .ok_or(Error::Malformed("short reply"))?;
        self.at += 4;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    pub fn u64(&mut self) -> Result<u64, Error> {
        let hi = self.u32()? as u64;
        let lo = self.u32()? as u64;
        Ok((hi << 32) | lo)
    }
    pub fn bool(&mut self) -> Result<bool, Error> {
        Ok(self.u32()? != 0)
    }
    /// Fixed-length opaque, padding skipped.
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let b = self
            .data
            .get(self.at..self.at + n)
            .ok_or(Error::Malformed("short reply"))?;
        self.at += n + (4 - n % 4) % 4;
        Ok(b)
    }
    /// Variable-length opaque.
    pub fn opaque(&mut self) -> Result<&'a [u8], Error> {
        let n = self.u32()? as usize;
        if n > FRAGMENT_LIMIT {
            return Err(Error::Malformed("opaque length"));
        }
        self.bytes(n)
    }
    pub fn string(&mut self) -> Result<String, Error> {
        Ok(String::from_utf8_lossy(self.opaque()?).into_owned())
    }
    pub fn skip(&mut self, n: usize) -> Result<(), Error> {
        self.bytes(n).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdr_round_trips_with_padding() {
        let mut w = Writer::with_capacity(32);
        w.u32(7)
            .u64(0x1_0000_0002)
            .opaque(b"abc")
            .string("hello")
            .bytes(&[1, 2]);
        assert_eq!(w.0.len(), 4 + 8 + 8 + 12 + 4);
        let mut r = Reader::new(&w.0);
        assert_eq!(r.u32().unwrap(), 7);
        assert_eq!(r.u64().unwrap(), 0x1_0000_0002);
        assert_eq!(r.opaque().unwrap(), b"abc");
        assert_eq!(r.string().unwrap(), "hello");
        assert_eq!(r.bytes(2).unwrap(), &[1, 2]);
        assert!(r.u32().is_err());
    }

    /// A one-call server on loopback: the record marking, the AUTH_SYS
    /// credential, and an accepted reply with results.
    #[test]
    fn a_call_goes_through_record_marking() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut mark = [0u8; 4];
            s.read_exact(&mut mark).unwrap();
            let len = (u32::from_be_bytes(mark) & 0x7fff_ffff) as usize;
            let mut req = vec![0u8; len];
            s.read_exact(&mut req).unwrap();
            let mut r = Reader::new(&req);
            let xid = r.u32().unwrap();
            assert_eq!(r.u32().unwrap(), CALL);
            assert_eq!(r.u32().unwrap(), RPC_VERSION);
            assert_eq!(
                (r.u32().unwrap(), r.u32().unwrap(), r.u32().unwrap()),
                (100_003, 3, 19)
            );
            assert_eq!(r.u32().unwrap(), AUTH_SYS);
            let cred = r.opaque().unwrap();
            let mut c = Reader::new(cred);
            c.u32().unwrap();
            assert_eq!(c.string().unwrap(), "evo");
            assert_eq!((c.u32().unwrap(), c.u32().unwrap()), (1000, 1000));
            // Reply in two fragments, to prove gathering.
            let mut w = Writer::with_capacity(32);
            w.u32(xid)
                .u32(REPLY)
                .u32(MSG_ACCEPTED)
                .u32(AUTH_NONE)
                .u32(0)
                .u32(0)
                .u32(42);
            let (a, b) = w.0.split_at(12);
            s.write_all(&(a.len() as u32).to_be_bytes()).unwrap();
            s.write_all(a).unwrap();
            s.write_all(&((b.len() as u32) | 0x8000_0000).to_be_bytes())
                .unwrap();
            s.write_all(b).unwrap();
        });
        let id = Identity {
            uid: 1000,
            gid: 1000,
            machine: "evo".into(),
        };
        let mut c =
            Client::connect("127.0.0.1", port, 100_003, 3, id, &StdDialer)
                .unwrap();
        let mut r = c.call(19, &[]).unwrap();
        assert_eq!(r.u32().unwrap(), 42);
    }
}

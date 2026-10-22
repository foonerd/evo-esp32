//! A TCP connection from a reserved port, for NFS: a knfsd export
//! marked `secure` (the usual default, Synology's among them) takes
//! mounts only from ports below 1024. On the chip there is no
//! privilege to bind one, so the client always may; `std` offers no way
//! to choose a source port, hence the sockets API through `libc`.

use std::net::{SocketAddr, TcpStream};
use std::os::fd::FromRawFd;
use std::sync::atomic::{AtomicU16, Ordering::Relaxed};
use std::time::Duration;

use org_evoframework_source_nfs::Dialer;

/// The reserved ports tried, top down.
const LOWEST: u16 = 600;
const HIGHEST: u16 = 1023;

/// Where the next connection starts looking: one below the last port
/// taken, round and round. A server keeps a closed connection's
/// four-tuple in TIME_WAIT for a minute; a new connection from the
/// same port to the same server inside that minute is reset or
/// ignored, which is what a remount after the NAS dropped an idle
/// connection ran into. A kernel client picks a fresh reserved port
/// each time for the same reason.
static NEXT: AtomicU16 = AtomicU16::new(HIGHEST);

/// Connects from a port between 600 and 1023, a different one each
/// time; falls back to the ordinary connect when none can be bound or
/// the address is not IPv4.
pub struct ReservedPort;

impl Dialer for ReservedPort {
    fn dial(
        &self,
        addr: SocketAddr,
        timeout: Duration,
    ) -> std::io::Result<TcpStream> {
        match reserved(&addr, timeout) {
            Some(stream) => Ok(stream),
            None => TcpStream::connect_timeout(&addr, timeout),
        }
    }
}

fn reserved(addr: &SocketAddr, timeout: Duration) -> Option<TcpStream> {
    let SocketAddr::V4(v4) = addr else {
        return None;
    };
    // SAFETY: sockets API on values this function owns; the descriptor
    // is closed on every path that does not hand it to a TcpStream,
    // which then owns it.
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return None;
        }
        let tv = libc::timeval {
            tv_sec: timeout.as_secs() as _,
            tv_usec: 0,
        };
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_SNDTIMEO,
            (&tv as *const libc::timeval).cast(),
            size_of::<libc::timeval>() as libc::socklen_t,
        );
        let mut local: libc::sockaddr_in = std::mem::zeroed();
        local.sin_family = libc::AF_INET as _;
        let mut bound = false;
        let span = u32::from(HIGHEST - LOWEST + 1);
        let start = NEXT.load(Relaxed);
        for i in 0..span {
            let port = LOWEST
                + ((u32::from(start - LOWEST) + span - i) % span) as u16;
            local.sin_port = port.to_be();
            if libc::bind(
                fd,
                (&local as *const libc::sockaddr_in).cast(),
                size_of::<libc::sockaddr_in>() as libc::socklen_t,
            ) == 0
            {
                bound = true;
                let below = if port == LOWEST { HIGHEST } else { port - 1 };
                NEXT.store(below, Relaxed);
                break;
            }
            let errno =
                std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            if errno != libc::EADDRINUSE {
                break;
            }
        }
        if !bound {
            libc::close(fd);
            return None;
        }
        let mut remote: libc::sockaddr_in = std::mem::zeroed();
        remote.sin_family = libc::AF_INET as _;
        remote.sin_port = v4.port().to_be();
        remote.sin_addr.s_addr = u32::from_ne_bytes(v4.ip().octets());
        if libc::connect(
            fd,
            (&remote as *const libc::sockaddr_in).cast(),
            size_of::<libc::sockaddr_in>() as libc::socklen_t,
        ) != 0
        {
            libc::close(fd);
            return None;
        }
        Some(TcpStream::from_raw_fd(fd))
    }
}

// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! A file on a server as a seekable reader.
//!
//! A track served over HTTP (a DLNA server's, a share's) is a file like
//! the card's, with an end and a place to seek to, where a station is
//! neither. Here the response is read as such a file. A seek moves the
//! position; the next read serves it from what is kept (the file's
//! first bytes, its last few kilobytes), from the open connection when
//! it stands there or a short way before (read through the gap), and
//! otherwise opens the file again from the place
//! ([`StreamOpener::open_at`], HTTP's `Range`). So a probe's look at
//! the end and return to the start costs one short request, not two
//! connections, and a seek into the body costs one. A connection that
//! ends early is opened again at the place reached, a few times. The
//! decoder never knows; the read-ahead thread ([`ahead`](crate::ahead))
//! does the waiting.

use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

use evo_esp32_sdk::contract::audio::playback::{ByteStream, StreamOpener};

/// A forward seek no further than this is read through rather than
/// opened again: sixteen kilobytes is well under a connection's worth.
const SKIP_THROUGH: u64 = 16 * 1024;
/// How many times in a row a connection that ended short of the file's
/// end is opened again before the read fails.
const REOPENS: u32 = 3;

/// What a response says about being a file the player can seek in: its
/// length, when the server gave one and does ranges, and the response
/// is not a station's (no `icy-metaint`).
pub fn seekable_length(stream: &dyn ByteStream) -> Option<u64> {
    if stream.header("icy-metaint").is_some() {
        return None;
    }
    let ranges = stream
        .header("accept-ranges")
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("bytes"));
    if !ranges {
        return None;
    }
    stream
        .header("content-length")
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
}

/// Where a ranged response starts, from its `Content-Range: bytes
/// START-END/TOTAL`.
fn range_start(stream: &dyn ByteStream) -> Option<u64> {
    let v = stream.header("content-range")?;
    let rest = v.trim().strip_prefix("bytes")?.trim_start();
    rest.split('-').next()?.trim().parse().ok()
}

/// What of the file's start is kept once read, so the probe's return
/// to byte zero after a look at the end is served from memory.
const HEAD: usize = 64 * 1024;
/// What of the file's end is fetched whole, in one short request, when
/// a read first lands there: the probe looks at the last 128 bytes for
/// an ID3v1 tag, a tag reader at the last few kilobytes.
const TAIL: u64 = 4 * 1024;

/// An open response and where in the file its next byte is.
struct Open {
    bytes: Box<dyn ByteStream>,
    at: u64,
}

/// A file on a server, read and sought like one on the card.
pub struct NetFile {
    opener: Arc<dyn StreamOpener>,
    uri: String,
    len: u64,
    /// The byte the next read returns.
    pos: u64,
    /// The open response, wherever it stands; a read elsewhere opens
    /// another, a read at its place carries on with it.
    stream: Option<Open>,
    /// The file's first bytes, as read in order from zero.
    head: Vec<u8>,
    /// The file's last bytes, once asked for: where they start, and them.
    tail: Option<(u64, Vec<u8>)>,
    /// Connections in a row that ended before the file did.
    short: u32,
}

impl NetFile {
    /// From the response `first`, open at byte zero, of a file `len`
    /// bytes long at `uri`; later opens go through `opener`.
    pub fn new(
        opener: Arc<dyn StreamOpener>,
        uri: &str,
        first: Box<dyn ByteStream>,
        len: u64,
    ) -> Self {
        Self {
            opener,
            uri: uri.to_string(),
            len,
            pos: 0,
            stream: Some(Open {
                bytes: first,
                at: 0,
            }),
            head: Vec::with_capacity(HEAD),
            tail: None,
            short: 0,
        }
    }

    /// The file's length.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether the file is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// A response open at `at`, checked to start there.
    fn open_at(&self, at: u64) -> std::io::Result<Box<dyn ByteStream>> {
        log::debug!("netfile: opening {} at {} of {}", self.uri, at, self.len);
        let stream = self
            .opener
            .open_at(&self.uri, at)
            .map_err(std::io::Error::other)?;
        if at > 0 {
            match range_start(stream.as_ref()) {
                Some(start) if start == at => {}
                Some(start) => {
                    return Err(std::io::Error::other(format!(
                        "{}: asked from {at}, the server answered from {start}",
                        self.uri
                    )));
                }
                None => {
                    return Err(std::io::Error::other(format!(
                        "{}: the server answered whole where a range was asked",
                        self.uri
                    )));
                }
            }
        }
        Ok(stream)
    }

    /// Fetch the file's last [`TAIL`] bytes in one short request, apart
    /// from the connection that reads the body.
    fn fetch_tail(&mut self) -> std::io::Result<()> {
        let start = self.len.saturating_sub(TAIL);
        let mut stream = self.open_at(start)?;
        let want = (self.len - start) as usize;
        let mut bytes = vec![0u8; want];
        let mut got = 0;
        while got < want {
            match stream.read(&mut bytes[got..]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(e) => return Err(std::io::Error::other(e)),
            }
        }
        bytes.truncate(got);
        self.tail = Some((start, bytes));
        Ok(())
    }

    /// Serve `buf` from a cached run of bytes at `start`, if `pos` is in it.
    fn from_cache(
        pos: u64,
        start: u64,
        bytes: &[u8],
        buf: &mut [u8],
    ) -> Option<usize> {
        let end = start + bytes.len() as u64;
        if pos < start || pos >= end {
            return None;
        }
        let at = (pos - start) as usize;
        let n = buf.len().min(bytes.len() - at);
        buf[..n].copy_from_slice(&bytes[at..at + n]);
        Some(n)
    }
}

impl Read for NetFile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() || self.pos >= self.len {
            return Ok(0);
        }
        if let Some(n) = Self::from_cache(self.pos, 0, &self.head, buf) {
            self.pos += n as u64;
            return Ok(n);
        }
        if let Some((start, bytes)) = &self.tail {
            if let Some(n) = Self::from_cache(self.pos, *start, bytes, buf) {
                self.pos += n as u64;
                return Ok(n);
            }
        }
        if self.tail.is_none() && self.len - self.pos <= TAIL {
            self.fetch_tail()?;
            return self.read(buf);
        }
        loop {
            // The open connection serves when it stands at the place, or a
            // short way before it (read through the gap).
            match self.stream.as_mut() {
                Some(open) if open.at == self.pos => {}
                Some(open)
                    if open.at < self.pos
                        && self.pos - open.at <= SKIP_THROUGH =>
                {
                    let mut gap = vec![0u8; (self.pos - open.at) as usize];
                    let mut got = 0;
                    while got < gap.len() {
                        match open.bytes.read(&mut gap[got..]) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => got += n,
                        }
                    }
                    open.at += got as u64;
                    if open.at != self.pos {
                        self.stream = None;
                    }
                    continue;
                }
                _ => {
                    let bytes = self.open_at(self.pos)?;
                    self.stream = Some(Open {
                        bytes,
                        at: self.pos,
                    });
                }
            }
            let open = self.stream.as_mut().expect("set above");
            let want = buf.len().min((self.len - self.pos) as usize);
            match open.bytes.read(&mut buf[..want]) {
                Ok(0) => {
                    // The connection ended before the file did.
                    self.stream = None;
                    self.short += 1;
                    if self.short > REOPENS {
                        return Err(std::io::Error::other(format!(
                            "{}: the connection ended at {} of {} bytes, {} times",
                            self.uri, self.pos, self.len, self.short
                        )));
                    }
                }
                Ok(n) => {
                    open.at += n as u64;
                    if self.pos as usize == self.head.len()
                        && self.head.len() < HEAD
                    {
                        let keep = n.min(HEAD - self.head.len());
                        self.head.extend_from_slice(&buf[..keep]);
                    }
                    self.pos += n as u64;
                    self.short = 0;
                    return Ok(n);
                }
                Err(e) => {
                    self.stream = None;
                    self.short += 1;
                    if self.short > REOPENS {
                        return Err(std::io::Error::other(format!(
                            "{}: {e}",
                            self.uri
                        )));
                    }
                }
            }
        }
    }
}

impl Seek for NetFile {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        let target = match to {
            SeekFrom::Start(p) => p,
            SeekFrom::End(d) => self.len.saturating_add_signed(d),
            SeekFrom::Current(d) => self.pos.saturating_add_signed(d),
        }
        .min(self.len);
        // The connection stays where it is: a read decides whether it
        // serves, reads through to the place, or another is opened.
        self.pos = target;
        Ok(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A server with one file, answering ranges, counting opens.
    struct Server {
        bytes: Vec<u8>,
        opens: Mutex<Vec<u64>>,
        /// Each response ends after this many bytes, as a flaky link.
        cut_after: Option<usize>,
    }

    struct Body {
        bytes: Vec<u8>,
        at: usize,
        start: u64,
        total: u64,
    }

    impl ByteStream for Body {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, String> {
            let n = (self.bytes.len() - self.at).min(buf.len()).min(700);
            buf[..n].copy_from_slice(&self.bytes[self.at..self.at + n]);
            self.at += n;
            Ok(n)
        }
        fn header(&self, name: &str) -> Option<String> {
            match name {
                "content-length" => Some(self.bytes.len().to_string()),
                "accept-ranges" => Some("bytes".into()),
                "content-range" if self.start > 0 => Some(format!(
                    "bytes {}-{}/{}",
                    self.start,
                    self.total - 1,
                    self.total
                )),
                _ => None,
            }
        }
    }

    impl StreamOpener for Server {
        fn open(&self, uri: &str) -> Result<Box<dyn ByteStream>, String> {
            self.open_at(uri, 0)
        }
        fn open_at(
            &self,
            _uri: &str,
            offset: u64,
        ) -> Result<Box<dyn ByteStream>, String> {
            self.opens.lock().unwrap().push(offset);
            let mut bytes = self.bytes[offset as usize..].to_vec();
            if let Some(cut) = self.cut_after {
                bytes.truncate(cut);
            }
            Ok(Box::new(Body {
                bytes,
                at: 0,
                start: offset,
                total: self.bytes.len() as u64,
            }))
        }
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 253) as u8).collect()
    }

    #[test]
    fn reads_seeks_and_reopens_where_it_must() {
        let bytes = pattern(200_000);
        let server = Arc::new(Server {
            bytes: bytes.clone(),
            opens: Mutex::new(Vec::new()),
            cut_after: None,
        });
        let first = server.open("http://s/a.flac").unwrap();
        assert_eq!(seekable_length(first.as_ref()), Some(200_000));
        let mut f =
            NetFile::new(server.clone(), "http://s/a.flac", first, 200_000);
        let mut head = vec![0u8; 5000];
        f.read_exact(&mut head).unwrap();
        assert_eq!(head, &bytes[..5000]);
        // A short hop forward reads through: no new connection.
        assert_eq!(f.seek(SeekFrom::Start(9000)).unwrap(), 9000);
        let mut at = [0u8; 100];
        f.read_exact(&mut at).unwrap();
        assert_eq!(&at[..], &bytes[9000..9100]);
        assert_eq!(server.opens.lock().unwrap().len(), 1);
        // A long one opens again at the place.
        assert_eq!(f.seek(SeekFrom::Start(150_000)).unwrap(), 150_000);
        f.read_exact(&mut at).unwrap();
        assert_eq!(&at[..], &bytes[150_000..150_100]);
        assert_eq!(server.opens.lock().unwrap().as_slice(), &[0, 150_000]);
        // Backwards, relative, and from the end: the end is one short
        // request for the tail, served from memory after.
        assert_eq!(f.seek(SeekFrom::Current(-100)).unwrap(), 150_000);
        assert_eq!(f.seek(SeekFrom::End(-10)).unwrap(), 199_990);
        let mut tail = Vec::new();
        f.read_to_end(&mut tail).unwrap();
        assert_eq!(tail, &bytes[199_990..]);
        assert_eq!(f.read(&mut at).unwrap(), 0);
        assert_eq!(
            server.opens.lock().unwrap().as_slice(),
            &[0, 150_000, 200_000 - TAIL]
        );
        assert_eq!(f.len(), 200_000);
    }

    #[test]
    fn the_probes_look_at_the_end_and_return_to_the_start_cost_one_short_request(
    ) {
        let bytes = pattern(300_000);
        let server = Arc::new(Server {
            bytes: bytes.clone(),
            opens: Mutex::new(Vec::new()),
            cut_after: None,
        });
        let first = server.open("http://s/a.flac").unwrap();
        let mut f =
            NetFile::new(server.clone(), "http://s/a.flac", first, 300_000);
        let mut buf = vec![0u8; 40_000];
        f.read_exact(&mut buf).unwrap();
        // The last 128 bytes (an ID3v1 tag's place)...
        f.seek(SeekFrom::End(-128)).unwrap();
        let mut id3 = [0u8; 128];
        f.read_exact(&mut id3).unwrap();
        assert_eq!(&id3[..], &bytes[300_000 - 128..]);
        // ...and back to the start: from memory, the body's connection
        // still where it was.
        f.seek(SeekFrom::Start(0)).unwrap();
        let mut again = vec![0u8; 40_000 + 1000];
        f.read_exact(&mut again).unwrap();
        assert_eq!(again, &bytes[..41_000]);
        assert_eq!(
            server.opens.lock().unwrap().as_slice(),
            &[0, 300_000 - TAIL],
            "the first connection and the tail's, nothing more"
        );
    }

    #[test]
    fn a_connection_that_ends_early_is_taken_up_again() {
        let bytes = pattern(50_000);
        let server = Arc::new(Server {
            bytes: bytes.clone(),
            opens: Mutex::new(Vec::new()),
            cut_after: Some(7000),
        });
        let first = server.open("http://s/a.mp3").unwrap();
        let mut f =
            NetFile::new(server.clone(), "http://s/a.mp3", first, 50_000);
        let mut out = Vec::new();
        f.read_to_end(&mut out).unwrap();
        assert_eq!(out, bytes);
        let opens = server.opens.lock().unwrap();
        assert_eq!(opens[0], 0);
        assert_eq!(opens[1], 7000);
        // The last stretch is the tail, fetched whole.
        assert_eq!(opens.last(), Some(&(50_000 - TAIL)));
    }

    struct Whole(Server);

    impl StreamOpener for Whole {
        fn open(&self, uri: &str) -> Result<Box<dyn ByteStream>, String> {
            self.0.open_at(uri, 0)
        }
        fn open_at(
            &self,
            uri: &str,
            _offset: u64,
        ) -> Result<Box<dyn ByteStream>, String> {
            // Ignores the range: answers from the start.
            self.0.open_at(uri, 0)
        }
    }

    #[test]
    fn a_server_that_ignores_the_range_is_an_error_not_a_wrong_place() {
        let server = Arc::new(Whole(Server {
            bytes: pattern(30_000),
            opens: Mutex::new(Vec::new()),
            cut_after: None,
        }));
        let first = server.open("http://s/a").unwrap();
        let mut f = NetFile::new(server, "http://s/a", first, 30_000);
        f.seek(SeekFrom::Start(25_000)).unwrap();
        let mut at = [0u8; 10];
        let err = f.read(&mut at).unwrap_err();
        assert!(err.to_string().contains("answered whole"), "{err}");
    }
}

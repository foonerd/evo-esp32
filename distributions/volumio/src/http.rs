// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Just enough HTTP/1.1 for polling: one request and one response per
//! connection, bodies by `Content-Length` or chunked, nothing compressed
//! because nothing asks for it.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// The most a body may be; a polling payload is a few kilobytes.
const BODY_LIMIT: usize = 64 * 1024;
/// The most a head (request line and headers) may be.
const HEAD_LIMIT: usize = 8 * 1024;

/// A request as read from a peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// `GET` or `POST`.
    pub method: String,
    /// The path with its query, as sent.
    pub target: String,
    /// Header names lower-cased.
    pub headers: Vec<(String, String)>,
    /// The body, decoded.
    pub body: Vec<u8>,
}

impl Request {
    /// The path part of the target.
    pub fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or("")
    }

    /// The value of a query parameter, undecoded.
    pub fn query(&self, key: &str) -> Option<&str> {
        let query = self.target.split_once('?')?.1;
        query
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v)
    }

    /// A header by name, case-insensitive.
    pub fn header(&self, name: &str) -> Option<&str> {
        header(&self.headers, name)
    }
}

/// A response as read from a peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// The status code.
    pub status: u16,
    /// Header names lower-cased.
    pub headers: Vec<(String, String)>,
    /// The body, decoded.
    pub body: Vec<u8>,
}

impl Response {
    /// A header by name, case-insensitive.
    pub fn header(&self, name: &str) -> Option<&str> {
        header(&self.headers, name)
    }

    /// The body as text.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn bad(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_string())
}

/// The head: a first line, then headers to the empty line.
fn read_head<R: BufRead>(
    r: &mut R,
) -> io::Result<(String, Vec<(String, String)>)> {
    let mut first = String::new();
    let mut total = 0;
    loop {
        first.clear();
        let n = r.read_line(&mut first)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "no request",
            ));
        }
        total += n;
        if total > HEAD_LIMIT {
            return Err(bad("head too long"));
        }
        if !first.trim().is_empty() {
            break;
        }
    }
    let mut headers = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        let n = r.read_line(&mut line)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "head cut short",
            ));
        }
        total += n;
        if total > HEAD_LIMIT {
            return Err(bad("head too long"));
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    Ok((first.trim_end_matches(['\r', '\n']).to_string(), headers))
}

/// The body after the head, by its framing; `to_end` when neither
/// framing is given (a response closed by the peer).
fn read_body<R: BufRead>(
    r: &mut R,
    headers: &[(String, String)],
    to_end: bool,
) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    if let Some(len) = header(headers, "content-length") {
        if len.trim().parse::<usize>().is_ok_and(|l| l > BODY_LIMIT) {
            return Err(bad("body too long"));
        }
    }
    Body::open(r, headers, to_end)?
        .take(BODY_LIMIT as u64 + 1)
        .read_to_end(&mut body)?;
    if body.len() > BODY_LIMIT {
        return Err(bad("body too long"));
    }
    Ok(body)
}

/// A body read as it comes, by its framing.
pub enum Body<'a, R: BufRead> {
    /// So many bytes.
    Length(io::Take<&'a mut R>),
    /// Chunk by chunk.
    Chunked {
        /// The connection.
        r: &'a mut R,
        /// What is left of the current chunk.
        left: usize,
        /// The last chunk was seen.
        done: bool,
    },
    /// Everything to the close.
    ToEnd(&'a mut R),
    /// Nothing.
    Empty,
}

impl<'a, R: BufRead> Body<'a, R> {
    /// The body by the headers; `to_end` allows the unframed kind.
    pub fn open(
        r: &'a mut R,
        headers: &[(String, String)],
        to_end: bool,
    ) -> io::Result<Self> {
        if let Some(te) = header(headers, "transfer-encoding") {
            if te.eq_ignore_ascii_case("chunked") {
                return Ok(Body::Chunked {
                    r,
                    left: 0,
                    done: false,
                });
            }
            return Err(bad("transfer encoding not supported"));
        }
        if let Some(len) = header(headers, "content-length") {
            let len: u64 =
                len.trim().parse().map_err(|_| bad("content length"))?;
            return Ok(Body::Length(r.take(len)));
        }
        Ok(if to_end { Body::ToEnd(r) } else { Body::Empty })
    }
}

impl<R: BufRead> Read for Body<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Body::Length(t) => t.read(buf),
            Body::ToEnd(r) => r.read(buf),
            Body::Empty => Ok(0),
            Body::Chunked { r, left, done } => {
                if *done {
                    return Ok(0);
                }
                if *left == 0 {
                    let mut line = String::new();
                    if r.read_line(&mut line)? == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "chunk cut short",
                        ));
                    }
                    let size_text = line.trim().split(';').next().unwrap_or("");
                    let size = usize::from_str_radix(size_text, 16)
                        .map_err(|_| bad("chunk size"))?;
                    if size == 0 {
                        // Trailers to the empty line.
                        loop {
                            line.clear();
                            if r.read_line(&mut line)? == 0
                                || line.trim().is_empty()
                            {
                                break;
                            }
                        }
                        *done = true;
                        return Ok(0);
                    }
                    *left = size;
                }
                let want = buf.len().min(*left);
                let n = r.read(&mut buf[..want])?;
                if n == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "chunk cut short",
                    ));
                }
                *left -= n;
                if *left == 0 {
                    let mut end = String::new();
                    r.read_line(&mut end)?;
                }
                Ok(n)
            }
        }
    }
}

/// One request from a connection.
pub fn read_request<R: Read>(stream: R) -> io::Result<Request> {
    let mut r = BufReader::new(stream);
    let (first, headers) = read_head(&mut r)?;
    let mut parts = first.split_whitespace();
    let method = parts.next().ok_or_else(|| bad("request line"))?.to_string();
    let target = parts.next().ok_or_else(|| bad("request line"))?.to_string();
    let body = read_body(&mut r, &headers, false)?;
    Ok(Request {
        method,
        target,
        headers,
        body,
    })
}

/// One response from a connection.
pub fn read_response<R: Read>(stream: R) -> io::Result<Response> {
    let mut r = BufReader::new(stream);
    let (first, headers) = read_head(&mut r)?;
    let status = first
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| bad("status line"))?;
    let body = read_body(&mut r, &headers, true)?;
    Ok(Response {
        status,
        headers,
        body,
    })
}

/// A response with a body, the connection to close after it. The
/// `Access-Control-Allow-Origin` is what the reference server sends, for
/// a browser that may look too.
pub fn write_response<W: Write>(
    w: &mut W,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
) -> io::Result<()> {
    write_response_with(w, status, reason, content_type, "", body)
}

/// As [`write_response`], with `extra` header lines (each ending in
/// `\r\n`).
pub fn write_response_with<W: Write>(
    w: &mut W,
    status: u16,
    reason: &str,
    content_type: &str,
    extra: &str,
    body: &[u8],
) -> io::Result<()> {
    write!(
        w,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{extra}Access-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    w.write_all(body)?;
    w.flush()
}

/// A request to `host` (as `ip:port`), the response read within
/// `read_timeout` of silence; a long poll answers within the peer's
/// own time, so the caller picks the patience.
pub fn fetch(
    host: &str,
    method: &str,
    target: &str,
    body: Option<&[u8]>,
    connect_timeout: Duration,
    read_timeout: Duration,
) -> io::Result<Response> {
    let stream =
        send(host, method, target, body, connect_timeout, read_timeout)?;
    read_response(stream)
}

/// As `fetch`, but the body is handed to `read` as it arrives, so a
/// large one need not be held whole.
pub fn fetch_with<T>(
    host: &str,
    method: &str,
    target: &str,
    body: Option<&[u8]>,
    connect_timeout: Duration,
    read_timeout: Duration,
    read: impl FnOnce(u16, &mut dyn Read) -> io::Result<T>,
) -> io::Result<T> {
    let stream =
        send(host, method, target, body, connect_timeout, read_timeout)?;
    let mut r = BufReader::new(stream);
    let (first, headers) = read_head(&mut r)?;
    let status = first
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| bad("status line"))?;
    let mut body = Body::open(&mut r, &headers, true)?;
    read(status, &mut body)
}

/// The request on its way; the response is left to read.
fn send(
    host: &str,
    method: &str,
    target: &str,
    body: Option<&[u8]>,
    connect_timeout: Duration,
    read_timeout: Duration,
) -> io::Result<TcpStream> {
    let addr = host.parse().map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, format!("address {host}"))
    })?;
    let mut stream = TcpStream::connect_timeout(&addr, connect_timeout)?;
    stream.set_read_timeout(Some(read_timeout))?;
    stream.set_write_timeout(Some(connect_timeout))?;
    stream.set_nodelay(true)?;
    let body = body.unwrap_or(&[]);
    let content = if body.is_empty() {
        String::new()
    } else {
        format!(
            "Content-Type: text/plain;charset=UTF-8\r\nContent-Length: {}\r\n",
            body.len()
        )
    };
    write!(
        stream,
        "{method} {target} HTTP/1.1\r\nHost: {host}\r\n{content}Connection: close\r\n\r\n"
    )?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_read_with_their_bodies() {
        let raw = b"POST /socket.io/?EIO=3&transport=polling&sid=abc HTTP/1.1\r\nHost: x\r\nContent-Type: text/plain;charset=UTF-8\r\nContent-Length: 4\r\n\r\n1:2\n";
        let req = read_request(&raw[..]).unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.path(), "/socket.io/");
        assert_eq!(req.query("EIO"), Some("3"));
        assert_eq!(req.query("sid"), Some("abc"));
        assert_eq!(req.query("t"), None);
        assert_eq!(
            req.header("Content-Type"),
            Some("text/plain;charset=UTF-8")
        );
        assert_eq!(req.body, b"1:2\n");
        let raw = b"GET /socket.io/?EIO=3&transport=polling HTTP/1.1\r\n\r\n";
        let req = read_request(&raw[..]).unwrap();
        assert_eq!(req.method, "GET");
        assert!(req.body.is_empty());
    }

    #[test]
    fn responses_are_read_by_length_chunks_or_close() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=UTF-8\r\nContent-Length: 5\r\n\r\n2:40x";
        let res = read_response(&raw[..]).unwrap();
        assert_eq!(res.status, 200);
        assert_eq!(res.text(), "2:40x");
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\n2:4\r\n1\r\n0\r\n0\r\n\r\n";
        let res = read_response(&raw[..]).unwrap();
        assert_eq!(res.text(), "2:40");
        let raw = b"HTTP/1.1 400 Bad Request\r\n\r\n{\"code\":1}";
        let res = read_response(&raw[..]).unwrap();
        assert_eq!(res.status, 400);
        assert_eq!(res.text(), "{\"code\":1}");
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 70000\r\n\r\n";
        assert!(read_response(&raw[..]).is_err());
    }

    #[test]
    fn responses_are_written_to_close() {
        let mut out = Vec::new();
        write_response(
            &mut out,
            200,
            "OK",
            "text/plain; charset=UTF-8",
            b"2:ok",
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Length: 4\r\n"));
        assert!(text.contains("Connection: close\r\n"));
        assert!(text.ends_with("\r\n\r\n2:ok"));
    }
}

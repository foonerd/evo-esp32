// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! `http://` streams over a plain socket: the host's `StreamOpener`,
//! enough to play a station on the desk and to test the path. HTTP/1.0,
//! one request, the body read to the end; a SHOUTcast server's
//! `ICY 200 OK` status line is accepted as well as HTTP's. No TLS: an
//! `https://` station is for the board, whose platform has the bundle.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use evo_esp32_sdk::contract::audio::playback::{ByteStream, StreamOpener};

const TIMEOUT: Duration = Duration::from_secs(10);

/// Opens `http://` streams over `std::net`.
#[derive(Debug, Default, Clone, Copy)]
pub struct PlainHttp;

struct Response {
    reader: BufReader<TcpStream>,
    headers: Vec<(String, String)>,
}

impl StreamOpener for PlainHttp {
    fn open(&self, uri: &str) -> Result<Box<dyn ByteStream>, String> {
        let rest = uri
            .strip_prefix("http://")
            .ok_or_else(|| format!("{uri}: only http:// on the host"))?;
        let (host_port, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let (host, port) = match host_port.rsplit_once(':') {
            Some((h, p)) => {
                (h, p.parse::<u16>().map_err(|_| format!("{uri}: bad port"))?)
            }
            None => (host_port, 80),
        };
        let addr = (host, port)
            .to_socket_addrs()
            .map_err(|e| format!("{host}: {e}"))?
            .next()
            .ok_or_else(|| format!("{host}: no address"))?;
        let mut tcp = TcpStream::connect_timeout(&addr, TIMEOUT)
            .map_err(|e| format!("{host}: {e}"))?;
        tcp.set_read_timeout(Some(TIMEOUT))
            .map_err(|e| format!("{host}: {e}"))?;
        write!(
            tcp,
            "GET {path} HTTP/1.0\r\nHost: {host}\r\nIcy-MetaData: 1\r\nUser-Agent: evo-esp32-host\r\nConnection: close\r\n\r\n"
        )
        .map_err(|e| format!("{host}: {e}"))?;
        let mut reader = BufReader::new(tcp);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|e| format!("{host}: {e}"))?;
        // "HTTP/1.0 200 OK" or "ICY 200 OK".
        let status: u16 = line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| format!("{uri}: no status in {line:?}"))?;
        if !(200..300).contains(&status) {
            return Err(format!("{uri}: HTTP {status}"));
        }
        let mut headers = Vec::new();
        loop {
            line.clear();
            reader
                .read_line(&mut line)
                .map_err(|e| format!("{host}: {e}"))?;
            let l = line.trim_end();
            if l.is_empty() {
                break;
            }
            if let Some((k, v)) = l.split_once(':') {
                headers.push((
                    k.trim().to_ascii_lowercase(),
                    v.trim().to_string(),
                ));
            }
        }
        Ok(Box::new(Response { reader, headers }))
    }
}

impl ByteStream for Response {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, String> {
        self.reader.read(buf).map_err(|e| e.to_string())
    }

    fn header(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    }
}

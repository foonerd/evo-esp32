// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! `http://` and `https://` streams over ESP-IDF's HTTP client, with the
//! certificate bundle for TLS: the board's [`StreamOpener`].
//!
//! The request asks for the station's metadata (`Icy-MetaData: 1`) and
//! follows redirects; the response's headers are kept for the decoder
//! to read the content type, the station's name, its bitrate and the
//! metadata interval. A file on a server is opened at an offset with a
//! `Range` header, for the decoder's seeks. A read that gets nothing
//! for the timeout is the transport giving up, which the stream says.

use std::time::Duration;

use esp_idf_svc::http::client::{
    Configuration, EspHttpConnection, FollowRedirectsPolicy,
};
use esp_idf_svc::http::Method;
use esp_idf_svc::sys;
use evo_esp32_sdk::contract::audio::playback::{ByteStream, StreamOpener};

/// How long a read may wait for the server before the stream is lost.
const TIMEOUT: Duration = Duration::from_secs(10);
/// The client's receive buffer, internal SRAM: one chunk of a stream.
const BUFFER: usize = 4096;
/// The client's send buffer: the request is a few hundred bytes.
const BUFFER_TX: usize = 1024;
/// Who is asking.
const USER_AGENT: &str = concat!("evo-esp32/", env!("CARGO_PKG_VERSION"));

/// Opens streams through `esp_http_client`.
#[derive(Debug, Default, Clone, Copy)]
pub struct HttpOpener;

/// An open response, read to its end.
pub struct HttpStream {
    connection: EspHttpConnection,
}

// SAFETY: an `esp_http_client` handle is not bound to the task that made
// it; ESP-IDF asks only that one task use it at a time. The stream is
// handed whole to the thread that reads it and is not touched by anyone
// else afterwards.
unsafe impl Send for HttpStream {}

impl StreamOpener for HttpOpener {
    fn open(&self, uri: &str) -> Result<Box<dyn ByteStream>, String> {
        request(uri, 0)
    }

    fn open_at(
        &self,
        uri: &str,
        offset: u64,
    ) -> Result<Box<dyn ByteStream>, String> {
        request(uri, offset)
    }
}

/// GET `uri`, from `offset` when it is not zero (`Range`), which the
/// server must honour with a 206: a 200 to a range is the whole file
/// from the start, not the place asked for.
fn request(uri: &str, offset: u64) -> Result<Box<dyn ByteStream>, String> {
    let phase = |what: &str| {
        // SAFETY: plain getters.
        let (free, low) = unsafe {
            (
                sys::heap_caps_get_free_size(sys::MALLOC_CAP_INTERNAL),
                sys::heap_caps_get_minimum_free_size(sys::MALLOC_CAP_INTERNAL),
            )
        };
        log::debug!("http: {what}: internal {free} B free, low-water {low} B");
    };
    phase("before the client");
    let mut connection = EspHttpConnection::new(&Configuration {
        buffer_size: Some(BUFFER),
        buffer_size_tx: Some(BUFFER_TX),
        timeout: Some(TIMEOUT),
        follow_redirects_policy: FollowRedirectsPolicy::FollowAll,
        crt_bundle_attach: Some(sys::esp_crt_bundle_attach),
        ..Default::default()
    })
    .map_err(|e| format!("http client: {e}"))?;
    phase("client made");
    let range = format!("bytes={offset}-");
    let mut headers: Vec<(&str, &str)> =
        vec![("Icy-MetaData", "1"), ("User-Agent", USER_AGENT)];
    if offset > 0 {
        headers.push(("Range", &range));
    }
    connection
        .initiate_request(Method::Get, uri, &headers)
        .map_err(|e| format!("{uri}: {e}"))?;
    phase("request sent (connected, TLS up, redirects followed)");
    connection
        .initiate_response()
        .map_err(|e| format!("{uri}: {e}"))?;
    phase("response headers in");
    let status = connection.status();
    if offset > 0 && status != 206 {
        return Err(format!("{uri}: HTTP {status} to a range from {offset}"));
    }
    if !(200..300).contains(&status) {
        return Err(format!("{uri}: HTTP {status}"));
    }
    Ok(Box::new(HttpStream { connection }))
}

impl ByteStream for HttpStream {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, String> {
        self.connection.read(buf).map_err(|e| format!("read: {e}"))
    }

    fn header(&self, name: &str) -> Option<String> {
        self.connection.header(name).map(str::to_string)
    }
}

// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! The network side of a DLNA source: SSDP to find media servers, HTTP
//! to read their descriptions and to ask their ContentDirectory for a
//! listing (SOAP), over `std::net` — a server on the LAN speaks plain
//! HTTP/1.1, and nothing here needs the platform's TLS.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

use crate::xml;

/// SSDP's address.
const SSDP: SocketAddrV4 =
    SocketAddrV4::new(Ipv4Addr::new(239, 255, 255, 250), 1900);
/// The device type asked for.
pub const MEDIA_SERVER: &str = "urn:schemas-upnp-org:device:MediaServer:1";
/// The service a listing comes from.
pub const CONTENT_DIRECTORY: &str =
    "urn:schemas-upnp-org:service:ContentDirectory:1";
/// How long a server may take to answer a request on the LAN before
/// the listing fails: a browse is a tap away, and the loop waits.
const TIMEOUT: Duration = Duration::from_secs(4);
/// The most a response body may be: a description is a few kilobytes,
/// a listing of two hundred items under a megabyte.
const BODY_LIMIT: usize = 1024 * 1024;
/// Who is asking.
const USER_AGENT: &str =
    concat!("evo-esp32/", env!("CARGO_PKG_VERSION"), " UPnP/1.0");

/// What an SSDP answer says: where the description is, and the device's
/// unique name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// Where the device's description is.
    pub location: String,
    /// The device's unique service name: its UDN, then the type.
    pub usn: String,
}

/// Ask the network for media servers and gather the answers for `wait`:
/// an M-SEARCH to the SSDP group from a port of our own; the answers
/// come back to that port, unicast. `from` is the address of the
/// interface to ask on; none is the system's choice (a board has one).
pub fn search(
    wait: Duration,
    from: Option<Ipv4Addr>,
) -> Result<Vec<Found>, String> {
    // Bound to the interface's address, the multicast leaves by it.
    let socket = UdpSocket::bind((from.unwrap_or(Ipv4Addr::UNSPECIFIED), 0))
        .map_err(|e| format!("ssdp socket: {e}"))?;
    socket
        .set_read_timeout(Some(Duration::from_millis(300)))
        .map_err(|e| format!("ssdp socket: {e}"))?;
    let mx = wait.as_secs().clamp(1, 5);
    let request = format!(
        "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: {mx}\r\nST: {MEDIA_SERVER}\r\nUSER-AGENT: {USER_AGENT}\r\n\r\n"
    );
    // Twice, a moment apart: UDP on Wi-Fi loses one now and then.
    socket
        .send_to(request.as_bytes(), SSDP)
        .map_err(|e| format!("ssdp send: {e}"))?;
    let started = Instant::now();
    let mut found: Vec<Found> = Vec::new();
    let mut buf = [0u8; 2048];
    let mut sent_again = false;
    while started.elapsed() < wait {
        if !sent_again && started.elapsed() > Duration::from_millis(400) {
            let _ = socket.send_to(request.as_bytes(), SSDP);
            sent_again = true;
        }
        let n = match socket.recv_from(&mut buf) {
            Ok((n, _)) => n,
            Err(_) => continue,
        };
        let text = String::from_utf8_lossy(&buf[..n]);
        if let Some(f) = parse_answer(&text) {
            if !found.iter().any(|g| g.usn == f.usn) {
                found.push(f);
            }
        }
    }
    Ok(found)
}

/// An SSDP answer's LOCATION and USN, if it is one for a media server.
pub fn parse_answer(text: &str) -> Option<Found> {
    let mut lines = text.lines();
    let status = lines.next()?;
    if !status.starts_with("HTTP/1.1 200") && !status.starts_with("NOTIFY") {
        return None;
    }
    let mut location = None;
    let mut usn = None;
    let mut st = None;
    for line in lines {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim();
        match k.trim().to_ascii_lowercase().as_str() {
            "location" => location = Some(v.to_string()),
            "usn" => usn = Some(v.to_string()),
            "st" | "nt" => st = Some(v.to_string()),
            _ => {}
        }
    }
    if st.as_deref() != Some(MEDIA_SERVER) {
        return None;
    }
    Some(Found {
        location: location?,
        usn: usn?,
    })
}

/// A media server as its description tells it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Server {
    /// Its unique device name (`uuid:…`), the key the player lists it by.
    pub udn: String,
    /// What it calls itself.
    pub name: String,
    /// Where ContentDirectory actions go, absolute.
    pub control: String,
    /// Its picture, absolute, if it offers one.
    pub icon: Option<String>,
}

/// Read the description at `location` and find the ContentDirectory.
pub fn describe(location: &str) -> Result<Server, String> {
    let (_, body) = get(location)?;
    let root = xml::parse(&body)?;
    let device = root
        .child("device")
        .ok_or_else(|| format!("{location}: no device in the description"))?;
    let udn = device
        .text_of("UDN")
        .ok_or_else(|| format!("{location}: a device without a UDN"))?
        .to_string();
    let name = device
        .text_of("friendlyName")
        .unwrap_or("Media server")
        .to_string();
    let base = root
        .text_of("URLBase")
        .map(str::to_string)
        .unwrap_or_else(|| location.to_string());
    let mut services = Vec::new();
    device.find_all("service", &mut services);
    let control = services
        .iter()
        .find(|s| s.text_of("serviceType") == Some(CONTENT_DIRECTORY))
        .and_then(|s| s.text_of("controlURL"))
        .ok_or_else(|| format!("{name}: no ContentDirectory service"))?;
    let icon = device
        .find("iconList")
        .map(|list| {
            let mut icons = Vec::new();
            list.find_all("icon", &mut icons);
            icons
        })
        .and_then(|icons| {
            // The largest PNG or JPEG.
            icons
                .iter()
                .filter(|i| {
                    i.text_of("mimetype")
                        .is_some_and(|m| m == "image/png" || m == "image/jpeg")
                })
                .max_by_key(|i| {
                    i.text_of("width")
                        .and_then(|w| w.parse::<u32>().ok())
                        .unwrap_or(0)
                })
                .and_then(|i| i.text_of("url"))
                .map(|u| resolve(&base, u))
        });
    Ok(Server {
        udn,
        name,
        control: resolve(&base, control),
        icon,
    })
}

/// One thing a listing holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Object {
    /// A container: somewhere to browse further.
    Container {
        /// Its object id, to browse it by.
        id: String,
        /// Its title.
        title: String,
    },
    /// An audio item with a stream the player can fetch.
    Item {
        /// Its object id.
        id: String,
        /// Its title.
        title: String,
        /// `upnp:artist`, or `dc:creator`.
        artist: Option<String>,
        /// `upnp:album`.
        album: Option<String>,
        /// The resource's duration.
        duration_ms: Option<u64>,
        /// `upnp:albumArtURI`.
        art: Option<String>,
        /// The resource's URL.
        uri: String,
    },
}

/// What a Browse answered: its objects and how many the container holds.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Browsed {
    /// The objects returned, in the server's order.
    pub objects: Vec<Object>,
    /// How many the container holds in all.
    pub total: u32,
}

/// Browse the direct children of `object_id` from `start`, at most
/// `count` of them.
pub fn browse(
    control: &str,
    object_id: &str,
    start: u32,
    count: u32,
) -> Result<Browsed, String> {
    let body = format!(
        concat!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>",
            "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" ",
            "s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body>",
            "<u:Browse xmlns:u=\"{service}\"><ObjectID>{id}</ObjectID>",
            "<BrowseFlag>BrowseDirectChildren</BrowseFlag><Filter>*</Filter>",
            "<StartingIndex>{start}</StartingIndex><RequestedCount>{count}</RequestedCount>",
            "<SortCriteria></SortCriteria></u:Browse></s:Body></s:Envelope>"
        ),
        service = CONTENT_DIRECTORY,
        id = xml::escape(object_id),
        start = start,
        count = count,
    );
    let action = format!("\"{CONTENT_DIRECTORY}#Browse\"");
    let (status, text) = post(control, &body, &action)?;
    let envelope = xml::parse(&text)?;
    if status != 200 {
        let detail = envelope
            .find("errorDescription")
            .map(|n| n.text.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("HTTP {status}"));
        return Err(format!("browse {object_id}: {detail}"));
    }
    let result = envelope
        .find("Result")
        .ok_or_else(|| "a Browse response without a Result".to_string())?;
    let total = envelope
        .find("TotalMatches")
        .and_then(|n| n.text.trim().parse().ok())
        .unwrap_or(0);
    Ok(Browsed {
        objects: parse_didl(&result.text)?,
        total,
    })
}

/// The containers and audio items of a DIDL-Lite document, in order.
pub fn parse_didl(didl: &str) -> Result<Vec<Object>, String> {
    let root = xml::parse(didl)?;
    let mut out = Vec::new();
    for node in &root.children {
        let id = node.attr("id").unwrap_or("").to_string();
        let title = node.text_of("title").unwrap_or("").to_string();
        match node.local() {
            "container" => out.push(Object::Container { id, title }),
            "item" => {
                let class = node.text_of("class").unwrap_or("");
                if !class.starts_with("object.item.audioItem") {
                    continue;
                }
                let mut resources = Vec::new();
                node.find_all("res", &mut resources);
                // An http-get resource for audio, the first such.
                let Some(res) = resources.iter().find(|r| {
                    r.attr("protocolInfo")
                        .is_none_or(|p| p.starts_with("http-get"))
                        && r.text.trim().starts_with("http")
                }) else {
                    continue;
                };
                out.push(Object::Item {
                    id,
                    title,
                    artist: node
                        .text_of("artist")
                        .or_else(|| node.text_of("creator"))
                        .map(str::to_string),
                    album: node.text_of("album").map(str::to_string),
                    duration_ms: res.attr("duration").and_then(duration_ms),
                    art: node.text_of("albumArtURI").map(str::to_string),
                    uri: res.text.trim().to_string(),
                });
            }
            _ => {}
        }
    }
    Ok(out)
}

/// `H:MM:SS.mmm` (or `H:MM:SS`, or `MM:SS`) in milliseconds.
pub fn duration_ms(s: &str) -> Option<u64> {
    let mut parts: Vec<&str> = s.trim().split(':').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    let last = parts.pop()?;
    let (secs, frac) = last.split_once('.').unwrap_or((last, ""));
    let mut ms = secs.parse::<u64>().ok()? * 1000;
    if !frac.is_empty() {
        let digits: String = frac.chars().take(3).collect();
        let n = digits.parse::<u64>().ok()?;
        ms += n * 10u64.pow(3 - digits.len() as u32);
    }
    let mut scale = 60_000;
    for p in parts.iter().rev() {
        ms += p.parse::<u64>().ok()? * scale;
        scale *= 60;
    }
    Some(ms)
}

/// `reference` against `base`: absolute as is, from the host for a
/// leading slash, else from the base's folder.
pub fn resolve(base: &str, reference: &str) -> String {
    if reference.contains("://") {
        return reference.to_string();
    }
    let scheme_end = base.find("://").map_or(0, |i| i + 3);
    if let Some(path) = reference.strip_prefix('/') {
        let host_end = base[scheme_end..]
            .find('/')
            .map_or(base.len(), |i| scheme_end + i);
        return format!("{}/{path}", &base[..host_end]);
    }
    match base.rfind('/') {
        Some(i) if i >= scheme_end => format!("{}{reference}", &base[..=i]),
        _ => format!("{base}/{reference}"),
    }
}

/// GET `url`: the status and the body as text.
pub fn get(url: &str) -> Result<(u16, String), String> {
    request(url, "GET", None, None)
}

/// POST a SOAP `body` to `url` with the `SOAPAction` given.
pub fn post(
    url: &str,
    body: &str,
    action: &str,
) -> Result<(u16, String), String> {
    request(url, "POST", Some(body), Some(action))
}

/// One HTTP/1.1 request on its own connection, the body read whole
/// (chunked or by length), as text.
fn request(
    url: &str,
    method: &str,
    body: Option<&str>,
    action: Option<&str>,
) -> Result<(u16, String), String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("{url}: only http:// here"))?;
    let (host_port, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) => {
            (h, p.parse::<u16>().map_err(|_| format!("{url}: bad port"))?)
        }
        None => (host_port, 80),
    };
    let addr = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("{host}: {e}"))?
        .next()
        .ok_or_else(|| format!("{host}: no address"))?;
    let mut tcp = TcpStream::connect_timeout(&addr, TIMEOUT)
        .map_err(|e| format!("{host_port}: {e}"))?;
    tcp.set_read_timeout(Some(TIMEOUT))
        .map_err(|e| format!("{host}: {e}"))?;
    tcp.set_write_timeout(Some(TIMEOUT))
        .map_err(|e| format!("{host}: {e}"))?;
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host_port}\r\nUser-Agent: {USER_AGENT}\r\nConnection: close\r\n"
    );
    if let Some(a) = action {
        head.push_str(&format!(
            "Content-Type: text/xml; charset=\"utf-8\"\r\nSOAPAction: {a}\r\n"
        ));
    }
    if let Some(b) = body {
        head.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    head.push_str("\r\n");
    tcp.write_all(head.as_bytes())
        .map_err(|e| format!("{host}: {e}"))?;
    if let Some(b) = body {
        tcp.write_all(b.as_bytes())
            .map_err(|e| format!("{host}: {e}"))?;
    }
    let mut reader = BufReader::new(tcp);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("{host}: {e}"))?;
    let status: u16 = line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("{url}: no status in {line:?}"))?;
    let mut length: Option<usize> = None;
    let mut chunked = false;
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
            match k.trim().to_ascii_lowercase().as_str() {
                "content-length" => length = v.trim().parse().ok(),
                "transfer-encoding" => {
                    chunked = v.to_ascii_lowercase().contains("chunked");
                }
                _ => {}
            }
        }
    }
    let mut bytes = Vec::new();
    if chunked {
        loop {
            line.clear();
            reader
                .read_line(&mut line)
                .map_err(|e| format!("{host}: {e}"))?;
            let size = line.trim().split(';').next().unwrap_or("0");
            let size = usize::from_str_radix(size.trim(), 16)
                .map_err(|_| format!("{url}: bad chunk size {size:?}"))?;
            if size == 0 {
                break;
            }
            if bytes.len() + size > BODY_LIMIT {
                return Err(format!(
                    "{url}: more than {} KB",
                    BODY_LIMIT / 1024
                ));
            }
            let at = bytes.len();
            bytes.resize(at + size, 0);
            reader
                .read_exact(&mut bytes[at..])
                .map_err(|e| format!("{host}: {e}"))?;
            // The CRLF after the chunk.
            let mut crlf = [0u8; 2];
            let _ = reader.read_exact(&mut crlf);
        }
    } else if let Some(n) = length {
        if n > BODY_LIMIT {
            return Err(format!("{url}: more than {} KB", BODY_LIMIT / 1024));
        }
        bytes.resize(n, 0);
        reader
            .read_exact(&mut bytes)
            .map_err(|e| format!("{host}: {e}"))?;
    } else {
        reader
            .take(BODY_LIMIT as u64)
            .read_to_end(&mut bytes)
            .map_err(|e| format!("{host}: {e}"))?;
    }
    Ok((status, String::from_utf8_lossy(&bytes).into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ssdp_answer_is_read_and_others_are_not() {
        let answer = "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nST: urn:schemas-upnp-org:device:MediaServer:1\r\nUSN: uuid:4d696e69-444c-4e41-9d41-000000000001::urn:schemas-upnp-org:device:MediaServer:1\r\nEXT:\r\nSERVER: 3.13.0 DLNADOC/1.50 UPnP/1.0 MiniDLNA/1.3.3\r\nLOCATION: http://192.0.2.10:8200/rootDesc.xml\r\nContent-Length: 0\r\n\r\n";
        let f = parse_answer(answer).unwrap();
        assert_eq!(f.location, "http://192.0.2.10:8200/rootDesc.xml");
        assert!(f.usn.starts_with("uuid:4d696e69"));
        let renderer =
            answer.replace("device:MediaServer:1", "device:MediaRenderer:1");
        assert!(parse_answer(&renderer).is_none());
        assert!(parse_answer("M-SEARCH * HTTP/1.1\r\n\r\n").is_none());
    }

    #[test]
    fn didl_items_become_objects_with_their_facts() {
        let didl = "<DIDL-Lite xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\" xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\">\n<container id=\"64\" parentID=\"0\" restricted=\"1\" childCount=\"5\"><dc:title>Browse Folders</dc:title><upnp:class>object.container.storageFolder</upnp:class></container>\n<item id=\"64$0\" parentID=\"64\" restricted=\"1\"><dc:title>Sweep 1</dc:title><upnp:class>object.item.audioItem.musicTrack</upnp:class><dc:creator>Bench Signals</dc:creator><upnp:artist>Bench Signals</upnp:artist><upnp:album>Envelopes</upnp:album><upnp:originalTrackNumber>1</upnp:originalTrackNumber><res size=\"532957\" duration=\"0:00:45.000\" bitrate=\"94747\" sampleFrequency=\"44100\" nrAudioChannels=\"2\" protocolInfo=\"http-get:*:audio/x-flac:*\">http://192.0.2.10:8200/MediaItems/22.flac</res></item>\n<item id=\"3$1\" parentID=\"3\"><dc:title>holiday.jpg</dc:title><upnp:class>object.item.imageItem.photo</upnp:class><res protocolInfo=\"http-get:*:image/jpeg:*\">http://h/p.jpg</res></item>\n<item id=\"9\" parentID=\"64\"><dc:title>No stream</dc:title><upnp:class>object.item.audioItem.musicTrack</upnp:class></item></DIDL-Lite>";
        let objects = parse_didl(didl).unwrap();
        assert_eq!(
            objects.len(),
            2,
            "a photo and an item without a stream are left out"
        );
        assert_eq!(
            objects[0],
            Object::Container {
                id: "64".into(),
                title: "Browse Folders".into()
            }
        );
        match &objects[1] {
            Object::Item {
                id,
                title,
                artist,
                album,
                duration_ms,
                art,
                uri,
            } => {
                assert_eq!(id, "64$0");
                assert_eq!(title, "Sweep 1");
                assert_eq!(artist.as_deref(), Some("Bench Signals"));
                assert_eq!(album.as_deref(), Some("Envelopes"));
                assert_eq!(*duration_ms, Some(45_000));
                assert_eq!(*art, None);
                assert_eq!(uri, "http://192.0.2.10:8200/MediaItems/22.flac");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(duration_ms("1:02:03.500"), Some(3_723_500));
        assert_eq!(duration_ms("02:03"), Some(123_000));
        assert_eq!(duration_ms("0:00:45"), Some(45_000));
        assert_eq!(duration_ms("abc"), None);
    }

    #[test]
    fn references_resolve() {
        assert_eq!(
            resolve("http://192.0.2.10:8200/rootDesc.xml", "/ctl/ContentDir"),
            "http://192.0.2.10:8200/ctl/ContentDir"
        );
        assert_eq!(
            resolve("http://h:8200/dev/desc.xml", "ctl"),
            "http://h:8200/dev/ctl"
        );
        assert_eq!(resolve("http://h", "a"), "http://h/a");
        assert_eq!(resolve("http://h/x", "http://k/y"), "http://k/y");
    }
}

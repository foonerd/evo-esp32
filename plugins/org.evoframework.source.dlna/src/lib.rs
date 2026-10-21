// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! A source over the DLNA media servers on the network.
//!
//! `org.evoframework.source.dlna` stocks `source/dlna`. The servers are
//! found by SSDP on a thread of the plugin's own, which asks the network
//! when it loads and every so often after, and keeps what answered with
//! each server's description read once. On `list` the root is the
//! servers found, each a folder; a server's path is its ContentDirectory
//! browsed from the root object, and a folder below it is `udn|object`,
//! the object's children asked for in one call. Containers are folders;
//! audio items are [`Track`]s with the server's title, artist, album,
//! duration and picture, their URI the item's `http` resource — which
//! the playback plugin reads and seeks as a file, since a server gives
//! its length and does ranges. Every track listed is announced as a
//! `track` subject, as the files source announces the card's.
//!
//! The protocol is written here, in plain `std::net`: SSDP's M-SEARCH,
//! the description's XML, the Browse action's SOAP and its DIDL-Lite,
//! nothing more of UPnP than a listing needs.

#![forbid(unsafe_code)]

pub mod upnp;
pub mod xml;

use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use evo_esp32_sdk::context::Context;
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

use upnp::{Object, Server};

/// The plugin's name, as the catalogue and the steward know it.
pub const NAME: &str = "org.evoframework.source.dlna";
/// Between a server's unique name and an object's id in a path.
pub const SEPARATOR: char = '|';
/// The root object of every ContentDirectory.
const ROOT_OBJECT: &str = "0";
/// The most objects asked for in one listing.
const PAGE: u32 = 200;
/// How long one search listens for answers.
const SEARCH_FOR: Duration = Duration::from_millis(2500);
/// How long between searches while servers are known.
const SEARCH_EVERY: Duration = Duration::from_secs(60);
/// How long between searches while none is.
const SEARCH_EVERY_WHILE_NONE: Duration = Duration::from_secs(15);
/// The search thread's stack: two sockets and a description's XML.
const STACK: usize = 16 * 1024;

static STOCKS: [Stocking; 1] = [Stocking {
    rack: shelf::SOURCE,
    shelf: shelf::DLNA,
    shape: shelf::SHAPE,
}];

/// What the search thread knows.
#[derive(Default)]
struct Known {
    servers: Vec<Server>,
    /// Searches run, so a listing can say whether none has finished.
    searches: u32,
}

/// The DLNA source.
pub struct SourceDlna {
    known: Arc<Mutex<Known>>,
    announced: Vec<SubjectId>,
    searching: bool,
    /// The interface the search goes out of, where the host has several.
    from: Option<Ipv4Addr>,
}

impl Default for SourceDlna {
    fn default() -> Self {
        Self::new()
    }
}

impl SourceDlna {
    /// A source that has not looked yet; it looks when loaded.
    pub fn new() -> Self {
        Self {
            known: Arc::new(Mutex::new(Known::default())),
            announced: Vec::new(),
            searching: false,
            from: None,
        }
    }

    /// The same, asking on the interface at `from`.
    pub fn from_interface(from: Ipv4Addr) -> Self {
        Self {
            from: Some(from),
            ..Self::new()
        }
    }

    /// A source over servers already known, which does not search:
    /// for a host that names its server, and for tests.
    pub fn with_servers(servers: Vec<Server>) -> Self {
        Self {
            known: Arc::new(Mutex::new(Known {
                servers,
                searches: 1,
            })),
            announced: Vec::new(),
            searching: true,
            from: None,
        }
    }

    /// The servers known now.
    pub fn servers(&self) -> Vec<Server> {
        self.known
            .lock()
            .map(|k| k.servers.clone())
            .unwrap_or_default()
    }

    fn start_search(&mut self) -> Result<(), LoadError> {
        if self.searching {
            return Ok(());
        }
        let known = Arc::clone(&self.known);
        let from = self.from;
        thread::Builder::new()
            .name("ssdp".into())
            .stack_size(STACK)
            .spawn(move || search_loop(&known, from))
            .map_err(|e| LoadError(format!("search thread: {e}")))?;
        self.searching = true;
        Ok(())
    }

    fn list(
        &mut self,
        args: &ListArgs,
        ctx: &mut dyn Context,
    ) -> Result<Listing, Fault> {
        let path = args.path.trim_matches('/');
        if path.is_empty() {
            let known = self.known.lock().unwrap_or_else(|e| e.into_inner());
            let entries = known
                .servers
                .iter()
                .map(|s| Entry::Folder {
                    path: s.udn.clone(),
                    name: s.name.clone(),
                })
                .collect();
            return Ok(Listing {
                path: String::new(),
                entries,
            });
        }
        let (udn, object) = match path.split_once(SEPARATOR) {
            Some((u, o)) => (u, o),
            None => (path, ROOT_OBJECT),
        };
        let server = self
            .known
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .servers
            .iter()
            .find(|s| s.udn == udn)
            .cloned()
            .ok_or_else(|| Fault::Failed {
                detail: format!("{udn}: no such server on the network now"),
            })?;
        let browsed = upnp::browse(&server.control, object, 0, PAGE)
            .map_err(|detail| Fault::Failed { detail })?;
        if browsed.total > PAGE {
            info!(
                "dlna: {} has {} objects in {object}; the first {PAGE} listed",
                server.name, browsed.total
            );
        }
        let mut entries = Vec::with_capacity(browsed.objects.len());
        let mut tracks = Vec::new();
        for o in browsed.objects {
            match o {
                Object::Container { id, title } => {
                    entries.push(Entry::Folder {
                        path: format!("{udn}{SEPARATOR}{id}"),
                        name: if title.is_empty() { id } else { title },
                    })
                }
                Object::Item {
                    title,
                    artist,
                    album,
                    duration_ms,
                    art,
                    uri,
                    ..
                } => tracks.push(Track {
                    title: if title.is_empty() {
                        uri.rsplit('/').next().unwrap_or("track").to_string()
                    } else {
                        title
                    },
                    uri,
                    artist,
                    album,
                    duration_ms,
                    sample_rate: None,
                    art,
                    live: false,
                }),
            }
        }
        for t in &tracks {
            if let Ok(id) = ctx.announce(SubjectAnnouncement {
                addressing: ExternalAddressing::new("stream", t.uri.clone()),
                kind: TRACK_KIND.into(),
                state: to_value(t),
            }) {
                if !self.announced.contains(&id) {
                    self.announced.push(id);
                }
            }
        }
        entries.extend(tracks.into_iter().map(Entry::Track));
        Ok(Listing {
            path: path.to_string(),
            entries,
        })
    }
}

/// Ask the network, read what answered, keep it; again after a while.
fn search_loop(known: &Mutex<Known>, from: Option<Ipv4Addr>) {
    loop {
        let found = match upnp::search(SEARCH_FOR, from) {
            Ok(f) => f,
            Err(e) => {
                warn!("dlna: {e}");
                Vec::new()
            }
        };
        let mut servers: Vec<Server> = Vec::new();
        for f in found {
            // A server already described keeps its description.
            let already = known.lock().ok().and_then(|k| {
                k.servers
                    .iter()
                    .find(|s| f.usn.starts_with(&s.udn))
                    .cloned()
            });
            match already {
                Some(s) => servers.push(s),
                None => match upnp::describe(&f.location) {
                    Ok(s) => {
                        info!("dlna: {} at {}", s.name, f.location);
                        servers.push(s);
                    }
                    Err(e) => warn!("dlna: {}: {e}", f.location),
                },
            }
        }
        servers.sort_by(|a, b| a.name.cmp(&b.name));
        let none = servers.is_empty();
        if let Ok(mut k) = known.lock() {
            if k.servers != servers {
                info!(
                    "dlna: {} server{} on the network{}",
                    servers.len(),
                    if servers.len() == 1 { "" } else { "s" },
                    if none {
                        String::new()
                    } else {
                        format!(
                            ": {}",
                            servers
                                .iter()
                                .map(|s| s.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    }
                );
            }
            k.servers = servers;
            k.searches = k.searches.saturating_add(1);
        }
        thread::sleep(if none {
            SEARCH_EVERY_WHILE_NONE
        } else {
            SEARCH_EVERY
        });
    }
}

impl Plugin for SourceDlna {
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
        self.start_search()
    }

    fn unload(&mut self, ctx: &mut dyn Context) {
        for id in self.announced.drain(..) {
            let _ = ctx.retract(&id);
        }
    }
}

impl Respondent for SourceDlna {
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

#[cfg(test)]
mod tests {
    use super::*;
    use evo_esp32_sdk::catalogue::{Catalogue, Family, Kind, Rack, Shelf};
    use evo_esp32_sdk::respondent::CorrelationId;
    use evo_esp32_sdk::Ticks;
    use evo_esp32_steward::{Config, Steward};
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn catalogue() -> Catalogue {
        Catalogue {
            racks: vec![Rack {
                name: shelf::SOURCE.into(),
                family: Family::Domain,
                kinds: vec![Kind::Producer],
                charter: String::new(),
                shelves: vec![Shelf {
                    name: shelf::DLNA.into(),
                    shape: shelf::SHAPE,
                    description: String::new(),
                }],
            }],
        }
    }

    /// A ContentDirectory of one container with two items, answering
    /// Browse by object id, chunked as many servers do.
    fn serve() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                // The whole request: headers, then the body by its length.
                let mut raw = Vec::new();
                let mut buf = [0u8; 2048];
                let req = loop {
                    let n = stream.read(&mut buf).unwrap_or(0);
                    if n == 0 {
                        break String::from_utf8_lossy(&raw).into_owned();
                    }
                    raw.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&raw).into_owned();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let length = text[..end]
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|v| {
                                        v.trim().parse::<usize>().ok()
                                    })
                            })
                            .unwrap_or(0);
                        if raw.len() >= end + 4 + length {
                            break text;
                        }
                    }
                };
                let didl = if req.contains("<ObjectID>0</ObjectID>") {
                    "<DIDL-Lite xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\" xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\"><container id=\"64\" parentID=\"0\"><dc:title>Music</dc:title><upnp:class>object.container</upnp:class></container></DIDL-Lite>".to_string()
                } else {
                    "<DIDL-Lite xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\" xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\"><item id=\"64$0\" parentID=\"64\"><dc:title>Sweep 1</dc:title><upnp:class>object.item.audioItem.musicTrack</upnp:class><upnp:artist>Bench Signals</upnp:artist><upnp:album>Envelopes</upnp:album><res duration=\"0:00:45.000\" protocolInfo=\"http-get:*:audio/x-flac:*\">http://127.0.0.1/MediaItems/22.flac</res></item><item id=\"64$1\" parentID=\"64\"><dc:title>Sweep 2</dc:title><upnp:class>object.item.audioItem.musicTrack</upnp:class><res protocolInfo=\"http-get:*:audio/mpeg:*\">http://127.0.0.1/MediaItems/23.mp3</res></item></DIDL-Lite>".to_string()
                };
                let body = format!(
                    "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><u:BrowseResponse xmlns:u=\"{}\"><Result>{}</Result><NumberReturned>2</NumberReturned><TotalMatches>2</TotalMatches><UpdateID>1</UpdateID></u:BrowseResponse></s:Body></s:Envelope>",
                    upnp::CONTENT_DIRECTORY,
                    xml::escape(&didl)
                );
                let (a, b) = body.split_at(body.len() / 2);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{a}\r\n{:x}\r\n{b}\r\n0\r\n\r\n",
                    a.len(),
                    b.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        format!("http://127.0.0.1:{port}/ctl/ContentDir")
    }

    fn list(s: &mut Steward, path: &str) -> Response {
        s.request(
            &Request {
                cid: CorrelationId(1),
                shelf: shelf::source_dlna(),
                verb: LIST.into(),
                subject: None,
                args: to_value(&ListArgs {
                    path: path.to_string(),
                }),
            },
            Ticks(0),
        )
        .unwrap()
    }

    #[test]
    fn the_root_lists_servers_and_a_server_browses_to_its_tracks() {
        let control = serve();
        let server = Server {
            udn: "uuid:bench".into(),
            name: "Bench".into(),
            control,
            icon: None,
        };
        let mut s = Steward::new(catalogue(), Config::default()).unwrap();
        s.admit_respondent(
            Box::new(SourceDlna::with_servers(vec![server])),
            Ticks(0),
        )
        .unwrap();
        let root: Listing =
            from_value(&list(&mut s, "").outcome.unwrap()).unwrap();
        assert_eq!(
            root.entries,
            vec![Entry::Folder {
                path: "uuid:bench".into(),
                name: "Bench".into()
            }]
        );
        let top: Listing =
            from_value(&list(&mut s, "uuid:bench").outcome.unwrap()).unwrap();
        assert_eq!(
            top.entries,
            vec![Entry::Folder {
                path: "uuid:bench|64".into(),
                name: "Music".into()
            }]
        );
        let music: Listing =
            from_value(&list(&mut s, "uuid:bench|64").outcome.unwrap())
                .unwrap();
        assert_eq!(music.entries.len(), 2);
        let Entry::Track(t) = &music.entries[0] else {
            panic!("{:?}", music.entries[0]);
        };
        assert_eq!(t.title, "Sweep 1");
        assert_eq!(t.artist.as_deref(), Some("Bench Signals"));
        assert_eq!(t.album.as_deref(), Some("Envelopes"));
        assert_eq!(t.duration_ms, Some(45_000));
        assert_eq!(t.uri, "http://127.0.0.1/MediaItems/22.flac");
        assert!(!t.live, "a server's item is a file, not a station");
        let Entry::Track(t2) = &music.entries[1] else {
            panic!()
        };
        assert_eq!(t2.duration_ms, None);
        assert!(!t2.live);
        // The tracks were announced as subjects.
        let announced =
            s.projection(evo_esp32_sdk::projection::ProjectionKey::Rack {
                name: shelf::SOURCE.into(),
            });
        assert_eq!(announced.subjects.len(), 2);
        // An unknown server is a fault, not a panic.
        assert!(list(&mut s, "uuid:other|1").outcome.is_err());
    }
}

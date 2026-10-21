// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The player as a Volumio device on the network.
//!
//! Volumio devices find each other by mDNS and keep a socket.io 2
//! session to each other; one asks another for its state and shows it
//! in Multiroom, and a leader tells a device to join its group by
//! broadcasting to everything connected to it. This crate gives the
//! player both ends of that: a [`server`] the peers connect to, which
//! answers for the player's state and announces changes, and a
//! [`client`] to each peer found, which hears the leader's word. What
//! the player should then do comes out as a [`Command`].
//!
//! Volume is not offered: a device with no volume of its own says so in
//! its state and Volumio shows it without a slider.
//!
//! Peers are learned from who connects: a Volumio that finds the player
//! by mDNS connects to it and introduces itself, and the player then
//! keeps a session back to it. The player does not search the network
//! for `_Volumio._tcp` itself, on purpose: avahi (which Volumio runs)
//! treats a query for a key from another host as a sign that its cached
//! records for that key may be out of date (RFC 6762 §10.5; avahi
//! `cache.c`, `start_poof_callback`), and unless the record is confirmed
//! by a response or by the querier's own known-answer section it drops
//! the record on the fifth such query. A responder never answers its own
//! query and the mDNS stack here puts none of its records in the
//! known-answer section, so a player that asked for its own service type
//! every minute vanished from every Volumio's list two minutes after it
//! was found, and came back at their next query. Measured on the bench
//! with the record browser on a Volumio and a wired capture.

pub mod client;
pub mod http;
pub mod server;
pub mod wire;

use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

pub use client::Peer;
use client::{Client, Heard};
use server::{Inbound, Server};

/// The port Volumio devices listen on, and look for each other on.
pub const PORT: u16 = 3000;
/// The mDNS service Volumio devices announce and search for.
pub const SERVICE: &str = "_Volumio";
/// The path peers fetch the player's picture from, on [`PORT`].
pub const ART_PATH: &str = "/albumart";
/// The player's tile when nothing with a cover plays: the word on the
/// evo tile, 256 pixels square.
pub const TILE_PNG: &[u8] = include_bytes!("../assets/tile.png");
/// How often the housekeeping thread looks round.
const TICK: Duration = Duration::from_millis(200);
/// The housekeeping thread's stack.
const STACK: usize = 16 * 1024;

/// A picture peers show for the player: what plays, or the tile.
#[derive(Clone)]
pub struct Art {
    /// Its media type.
    pub mime: &'static str,
    /// Its bytes, as the file has them.
    pub bytes: Arc<Vec<u8>>,
}

impl Art {
    /// A picture from its bytes, if they are a JPEG or a PNG.
    pub fn sniff(bytes: Vec<u8>) -> Option<Self> {
        let mime = match bytes.as_slice() {
            [0xff, 0xd8, ..] => "image/jpeg",
            [0x89, b'P', b'N', b'G', ..] => "image/png",
            _ => return None,
        };
        Some(Self {
            mime,
            bytes: Arc::new(bytes),
        })
    }

    /// The tile.
    pub fn tile() -> Self {
        Self {
            mime: "image/png",
            bytes: Arc::new(TILE_PNG.to_vec()),
        }
    }
}

/// Who the player is to Volumio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// The name shown in Multiroom.
    pub name: String,
    /// The id Volumio keys devices by; the same as announced in mDNS.
    pub uuid: String,
}

/// Whether the player is playing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Status {
    /// Sound is coming out.
    Play,
    /// Paused.
    Pause,
    /// Nothing on.
    #[default]
    Stop,
}

impl Status {
    fn word(self) -> &'static str {
        match self {
            Status::Play => "play",
            Status::Pause => "pause",
            Status::Stop => "stop",
        }
    }
}

/// What the player is doing, as Volumio shows it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Playing {
    /// Playing, paused or stopped.
    pub status: Status,
    /// The track's title.
    pub title: String,
    /// The track's artist.
    pub artist: String,
    /// The track's album.
    pub album: String,
}

/// What a leader asked of the player.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Join the group of the leader at `host` (an address, no scheme).
    Join {
        /// Where the leader's Snapcast server is.
        host: String,
        /// The leader's id.
        id: String,
        /// The leader's name.
        name: String,
    },
    /// Leave the group.
    Leave,
}

struct Shared {
    identity: Identity,
    address: Option<Ipv4Addr>,
    port: u16,
    playing: Playing,
    enabled: bool,
    leader: Option<String>,
    /// Session ids and the peer ids they introduced themselves with.
    known: Vec<(String, String)>,
    /// Counts the pictures set, so a peer's browser fetches a new one.
    art_version: u32,
}

/// The player's presence among Volumio devices.
#[derive(Clone)]
pub struct Presence {
    server: Server,
    client: Client,
    shared: Arc<Mutex<Shared>>,
    commands: Arc<Mutex<VecDeque<Command>>>,
}

impl Presence {
    /// A presence for `identity`, not yet on the network.
    pub fn new(identity: Identity) -> Self {
        let client = Client::new();
        client.set_hello(vec![wire::event(
            "initSocket",
            &json!({ "id": identity.uuid }),
        )]);
        Self {
            server: Server::new(Art::tile()),
            client,
            shared: Arc::new(Mutex::new(Shared {
                identity,
                address: None,
                port: PORT,
                playing: Playing::default(),
                enabled: false,
                leader: None,
                known: Vec::new(),
                art_version: 0,
            })),
            commands: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    /// Listen on `port` and keep house; two threads.
    pub fn start(&self, port: u16) -> std::io::Result<()> {
        self.lock().port = port;
        let server = self.server.clone();
        std::thread::Builder::new()
            .name("volumio".into())
            .stack_size(STACK)
            .spawn(move || {
                if let Err(e) = server.serve(port) {
                    log::error!("volumio: server: {e}");
                }
            })?;
        let me = self.clone();
        std::thread::Builder::new()
            .name("volumio-tick".into())
            .stack_size(STACK)
            .spawn(move || loop {
                me.client.tick();
                me.pump();
                std::thread::sleep(TICK);
            })?;
        Ok(())
    }

    /// Our address, for the `host` peers dial; `None` while off the
    /// network.
    pub fn set_address(&self, address: Option<Ipv4Addr>) {
        let changed = {
            let mut shared = self.lock();
            let changed = shared.address != address;
            shared.address = address;
            changed
        };
        if changed && address.is_some() {
            self.announce();
        }
    }

    /// What the player is doing; peers hear of a change.
    pub fn set_playing(&self, playing: Playing) {
        let changed = {
            let mut shared = self.lock();
            let changed = shared.playing != playing;
            shared.playing = playing;
            changed
        };
        if changed {
            self.announce();
        }
    }

    /// The picture of what plays, or `None` for the tile; peers hear of
    /// it and fetch it.
    pub fn set_art(&self, art: Option<Art>) {
        self.server.set_art(art);
        self.lock().art_version += 1;
        self.announce();
    }

    /// The Volumio devices on the network, besides the ones that
    /// connected to us; our own address is left out.
    pub fn set_peers(&self, peers: &[Peer]) {
        let own = self.lock().address;
        let others: Vec<Peer> = peers
            .iter()
            .filter(|p| Some(*p.addr.ip()) != own)
            .cloned()
            .collect();
        self.client.set_peers(&others);
    }

    /// The peers kept sessions with, by name and address.
    pub fn peers(&self) -> Vec<Peer> {
        self.client.peers()
    }

    /// The next thing a leader asked for.
    pub fn take_command(&self) -> Option<Command> {
        self.commands
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
    }

    /// Peers connected to us, and peers we are connected to.
    pub fn connections(&self) -> (usize, usize) {
        (self.server.connected(), self.client.connected())
    }

    /// The peers we hold a session with.
    pub fn heard(&self) -> Vec<SocketAddrV4> {
        self.client.heard_from()
    }

    /// Whether a leader has the player in its group.
    pub fn grouped(&self) -> bool {
        self.lock().enabled
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Tell every peer connected to us where we stand.
    fn announce(&self) {
        let (state, output) = {
            let shared = self.lock();
            (state_json(&shared), output_json(&shared))
        };
        self.server.broadcast(&wire::event("pushState", &state));
        self.server
            .broadcast(&wire::event("pushMultiroomSyncOutput", &output));
    }

    /// What came in, acted on.
    fn pump(&self) {
        for inbound in self.server.take_inbound() {
            match inbound {
                Inbound::Hello { sid, id, addr } => {
                    log::info!("volumio: session {sid} is {id} at {addr}");
                    let mut shared = self.lock();
                    shared.known.retain(|(s, _)| *s != sid);
                    shared.known.push((sid, id.clone()));
                    let own = shared.address;
                    drop(shared);
                    // A peer that found us is one to hear; it listens on
                    // the port all Volumio devices listen on.
                    if let std::net::SocketAddr::V4(from) = addr {
                        if Some(*from.ip()) != own {
                            self.client.add_peer(&peer(
                                &id,
                                SocketAddrV4::new(*from.ip(), PORT),
                            ));
                        }
                    }
                }
                Inbound::Bye { sid } => {
                    self.lock().known.retain(|(s, _)| *s != sid);
                }
                Inbound::Event { sid, name, data } => match name.as_str() {
                    "getState" => {
                        let state = state_json(&self.lock());
                        self.server
                            .send(&sid, &wire::event("pushState", &state));
                    }
                    "getMultiroomSyncOutput" => {
                        let output = output_json(&self.lock());
                        self.server.send(
                            &sid,
                            &wire::event("pushMultiroomSyncOutput", &output),
                        );
                    }
                    "enableMultiroomSyncOutput"
                    | "disableMultiroomSyncOutput" => {
                        self.leader_said(&name, &data);
                    }
                    other => log::debug!("volumio: {sid} asked {other}"),
                },
            }
        }
        for heard in self.client.take_heard() {
            match heard {
                Heard::Connected { addr } => {
                    log::info!("volumio: hearing {addr}")
                }
                Heard::Lost { addr } => {
                    log::info!("volumio: no longer hearing {addr}")
                }
                Heard::Event { addr, name, data } => match name.as_str() {
                    "enableMultiroomSyncOutput"
                    | "disableMultiroomSyncOutput" => {
                        log::info!("volumio: {addr} said {name}");
                        self.leader_said(&name, &data);
                    }
                    "getMultiroomSyncOutput" => self.announce(),
                    _ => {}
                },
            }
        }
    }

    /// A leader's enable or disable, if it names us.
    fn leader_said(&self, name: &str, data: &Value) {
        let command = {
            let mut shared = self.lock();
            if data.get("id").and_then(Value::as_str)
                != Some(shared.identity.uuid.as_str())
            {
                return;
            }
            if name == "enableMultiroomSyncOutput" {
                let server = &data["server"];
                let Some(host) = server.get("host").and_then(Value::as_str)
                else {
                    log::warn!("volumio: enable without a host");
                    return;
                };
                let host = host
                    .trim_start_matches("http://")
                    .trim_start_matches("https://")
                    .trim_end_matches('/')
                    .to_string();
                let id = server
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let leader = server
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                shared.enabled = true;
                shared.leader = Some(id.clone());
                Command::Join {
                    host,
                    id,
                    name: leader,
                }
            } else {
                shared.enabled = false;
                shared.leader = None;
                Command::Leave
            }
        };
        self.commands
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(command);
        self.announce();
    }
}

/// Where peers fetch the picture: absolute, so a peer does not go to
/// port 80 for it, with the version so a browser fetches a new one.
fn art_url(shared: &Shared) -> String {
    match shared.address {
        Some(a) => format!(
            "http://{a}:{}{ART_PATH}?v={}",
            shared.port, shared.art_version
        ),
        None => String::new(),
    }
}

/// The state as Volumio's `pushState` carries it; the fields a peer
/// reads for its device list, and no volume to offer.
fn state_json(shared: &Shared) -> Value {
    let p = &shared.playing;
    json!({
        "status": p.status.word(),
        "title": p.title,
        "artist": p.artist,
        "album": p.album,
        "albumart": art_url(shared),
        "uri": "",
        "seek": 0,
        "duration": 0,
        "volume": 100,
        "mute": false,
        "disableVolumeControl": true,
        "service": "",
    })
}

/// The output as Volumio's Multiroom lists it.
fn output_json(shared: &Shared) -> Value {
    let p = &shared.playing;
    let host = shared
        .address
        .map(|a| format!("http://{a}"))
        .unwrap_or_default();
    json!({
        "name": shared.identity.name,
        "id": shared.identity.uuid,
        "host": host,
        "icon": "fa fa-speaker",
        "volume": 100,
        "volumeAvailable": false,
        "available": true,
        "enabled": shared.enabled,
        "mute": false,
        "type": "multiroomSync",
        "plugin": "audio_interface/multiroom",
        "leader": shared.leader,
        "state": {
            "status": p.status.word(),
            "artist": p.artist,
            "track": p.title,
            "albumart": art_url(shared),
            "album": p.album,
        },
    })
}

/// The mDNS TXT record Volumio devices announce with.
pub fn txt(identity: &Identity) -> Vec<(String, String)> {
    vec![
        ("volumioName".to_string(), identity.name.clone()),
        ("UUID".to_string(), identity.uuid.clone()),
        ("type".to_string(), "device".to_string()),
    ]
}

/// A peer from what mDNS found.
pub fn peer(name: &str, addr: SocketAddrV4) -> Peer {
    Peer {
        name: name.to_string(),
        addr,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::fetch;
    use crate::wire::Packet;
    use std::net::TcpListener;
    use std::time::Instant;

    fn free_port() -> u16 {
        TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn get(port: u16, target: &str) -> Vec<String> {
        let res = fetch(
            &format!("127.0.0.1:{port}"),
            "GET",
            target,
            None,
            Duration::from_secs(2),
            Duration::from_secs(3),
        )
        .unwrap();
        assert_eq!(res.status, 200, "{}", res.text());
        wire::decode_payload(&res.text()).unwrap()
    }

    fn post(port: u16, target: &str, body: &str) {
        let res = fetch(
            &format!("127.0.0.1:{port}"),
            "POST",
            target,
            Some(body.as_bytes()),
            Duration::from_secs(2),
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(res.text(), "ok");
    }

    #[test]
    fn a_peer_asks_and_is_told_and_hears_of_changes() {
        let presence = Presence::new(Identity {
            name: "Kitchen".into(),
            uuid: "evo-1".into(),
        });
        presence.set_address(Some(Ipv4Addr::new(10, 0, 0, 7)));
        let port = free_port();
        presence.start(port).unwrap();
        let started = Instant::now();
        loop {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(3));
            std::thread::sleep(Duration::from_millis(20));
        }
        // Volumio connects, introduces itself, asks as it does.
        let packets = get(port, &server::query(None, 1));
        let Packet::Open(open) = wire::parse(&packets[0]) else {
            panic!("{packets:?}")
        };
        let sid = open["sid"].as_str().unwrap().to_string();
        post(
            port,
            &server::query(Some(&sid), 2),
            &wire::encode_payload([
                "42[\"initSocket\",{\"id\":\"volumio-a\"}]",
                "42[\"getState\",\"\"]",
                "42[\"getMultiroomSyncOutput\",\"\"]",
            ]),
        );
        let mut got = Vec::new();
        while got.len() < 2 {
            got.extend(get(port, &server::query(Some(&sid), 3)));
        }
        let Packet::Event(name, state) = wire::parse(&got[0]) else {
            panic!("{got:?}")
        };
        assert_eq!(name, "pushState");
        assert_eq!(state["status"], "stop");
        assert_eq!(state["disableVolumeControl"], true);
        let Packet::Event(name, output) = wire::parse(&got[1]) else {
            panic!("{got:?}")
        };
        assert_eq!(name, "pushMultiroomSyncOutput");
        assert_eq!(output["id"], "evo-1");
        assert_eq!(output["name"], "Kitchen");
        assert_eq!(output["host"], "http://10.0.0.7");
        assert_eq!(output["volumeAvailable"], false);
        assert_eq!(output["enabled"], false);
        assert_eq!(output["leader"], Value::Null);
        assert_eq!(output["state"]["status"], "stop");
        let art = format!("http://10.0.0.7:{port}{ART_PATH}?v=0");
        assert_eq!(state["albumart"], art);
        assert_eq!(output["state"]["albumart"], art);

        // A change in what plays reaches the open poll; so does a new
        // picture, by its version.
        let p = presence.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            p.set_art(Art::sniff(vec![0xff, 0xd8, 0xff, 9]));
            p.set_playing(Playing {
                status: Status::Play,
                title: "Song".into(),
                artist: "Band".into(),
                album: String::new(),
            });
        });
        let mut got = Vec::new();
        while got.len() < 4 {
            got.extend(get(port, &server::query(Some(&sid), 4)));
        }
        let Packet::Event(_, state) = wire::parse(&got[2]) else {
            panic!("{got:?}")
        };
        assert_eq!(state["status"], "play");
        assert_eq!(state["title"], "Song");
        assert_eq!(
            state["albumart"],
            format!("http://10.0.0.7:{port}{ART_PATH}?v=1")
        );
        let Packet::Event(_, output) = wire::parse(&got[3]) else {
            panic!("{got:?}")
        };
        assert_eq!(output["state"]["track"], "Song");
        assert_eq!(output["state"]["artist"], "Band");

        // A leader's word, arriving by whichever way, groups us; the
        // host loses its scheme on the way to the command.
        post(
            port,
            &server::query(Some(&sid), 5),
            &wire::encode_payload([&wire::event(
                "enableMultiroomSyncOutput",
                &json!({"id": "someone-else", "server": {"host": "http://10.0.0.1"}}),
            )]),
        );
        post(
            port,
            &server::query(Some(&sid), 6),
            &wire::encode_payload([&wire::event(
                "enableMultiroomSyncOutput",
                &json!({"id": "evo-1", "server": {"host": "http://10.0.0.1", "id": "volumio-a", "name": "Living"}}),
            )]),
        );
        let started = Instant::now();
        let command = loop {
            if let Some(c) = presence.take_command() {
                break c;
            }
            assert!(started.elapsed() < Duration::from_secs(3));
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(
            command,
            Command::Join {
                host: "10.0.0.1".into(),
                id: "volumio-a".into(),
                name: "Living".into()
            }
        );
        assert!(presence.grouped());
        let mut got = Vec::new();
        while got.len() < 2 {
            got.extend(get(port, &server::query(Some(&sid), 7)));
        }
        let Packet::Event(_, output) = wire::parse(&got[1]) else {
            panic!("{got:?}")
        };
        assert_eq!(output["enabled"], true);
        assert_eq!(output["leader"], "volumio-a");

        post(
            port,
            &server::query(Some(&sid), 8),
            &wire::encode_payload([&wire::event(
                "disableMultiroomSyncOutput",
                &json!({"id": "evo-1"}),
            )]),
        );
        let started = Instant::now();
        let command = loop {
            if let Some(c) = presence.take_command() {
                break c;
            }
            assert!(started.elapsed() < Duration::from_secs(3));
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(command, Command::Leave);
        assert!(!presence.grouped());
        assert_eq!(presence.connections(), (1, 0));
    }

    #[test]
    fn the_txt_record_is_what_volumio_reads() {
        let id = Identity {
            name: "Kitchen".into(),
            uuid: "evo-1".into(),
        };
        assert_eq!(
            txt(&id),
            vec![
                ("volumioName".to_string(), "Kitchen".to_string()),
                ("UUID".to_string(), "evo-1".to_string()),
                ("type".to_string(), "device".to_string()),
            ]
        );
    }
}

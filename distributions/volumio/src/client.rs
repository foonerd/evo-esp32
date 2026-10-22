// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The client half: a session to each peer found, so what a peer says
//! to everyone connected to it reaches us. One thread per peer keeps a
//! poll open; pings and the odd post go on their own short connections,
//! never two posts at once because the reference server closes the
//! session on an overlap.

use std::collections::VecDeque;
use std::io::{self, Read};
use std::net::SocketAddrV4;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::http::{fetch, fetch_with};
use crate::server::query;
use crate::wire::{self, Packet};

/// Packets longer than this, in UTF-16 units, are passed over; a peer's
/// queue or library pushes are of no use to us.
const KEEP: usize = 4096;
/// To connect, or to be answered to a post.
const SHORT: Duration = Duration::from_secs(4);
/// Past the ping interval a poll is answered by the pong; this much
/// more and the peer is not answering.
const POLL_GRACE: Duration = Duration::from_secs(20);
/// Between attempts after a failure, and after many.
const RETRY: Duration = Duration::from_secs(5);
const RETRY_LATER: Duration = Duration::from_secs(30);
const RETRY_STEP: Duration = Duration::from_millis(250);
/// The thread's stack; the JSON of a packet is parsed on it.
const STACK: usize = 16 * 1024;
/// The most peers kept; a home has a few Volumio devices, not dozens.
const PEER_LIMIT: usize = 8;

/// A peer to keep a session with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    /// Its name, as announced.
    pub name: String,
    /// Where it listens.
    pub addr: SocketAddrV4,
}

/// What came from a peer.
#[derive(Debug, Clone, PartialEq)]
pub enum Heard {
    /// A session with the peer is up.
    Connected {
        /// The peer.
        addr: SocketAddrV4,
    },
    /// The peer sent an event.
    Event {
        /// The peer.
        addr: SocketAddrV4,
        /// The event's name.
        name: String,
        /// The event's data.
        data: Value,
    },
    /// The session with the peer ended.
    Lost {
        /// The peer.
        addr: SocketAddrV4,
    },
}

struct Link {
    peer: Peer,
    wanted: bool,
    sid: Option<String>,
    interval: Duration,
    last_ping: Instant,
    posting: Arc<Mutex<()>>,
}

struct Inner {
    links: Mutex<Vec<Link>>,
    heard: Mutex<VecDeque<Heard>>,
    hello: Mutex<Vec<String>>,
    t: Mutex<u32>,
}

/// Our side of the polling transport, calling.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    /// A client with no peers.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                links: Mutex::new(Vec::new()),
                heard: Mutex::new(VecDeque::new()),
                hello: Mutex::new(Vec::new()),
                t: Mutex::new(0),
            }),
        }
    }

    /// Packets posted to a peer as soon as a session is up.
    pub fn set_hello(&self, packets: Vec<String>) {
        *self.inner.hello.lock().unwrap_or_else(|e| e.into_inner()) = packets;
    }

    /// The peers to keep sessions with; new ones get a thread, ones no
    /// longer listed are let go.
    pub fn set_peers(&self, peers: &[Peer]) {
        let mut links =
            self.inner.links.lock().unwrap_or_else(|e| e.into_inner());
        for link in links.iter_mut() {
            link.wanted = peers.iter().any(|p| p.addr == link.peer.addr);
        }
        for peer in peers {
            Self::add_locked(&self.inner, &mut links, peer);
        }
    }

    /// One more peer to keep a session with; nothing if it is known.
    /// Up to eight are kept.
    pub fn add_peer(&self, peer: &Peer) {
        let mut links =
            self.inner.links.lock().unwrap_or_else(|e| e.into_inner());
        if links.len() >= PEER_LIMIT
            && !links.iter().any(|l| l.peer.addr == peer.addr)
        {
            log::warn!(
                "volumio: {} not kept, {PEER_LIMIT} peers already",
                peer.addr
            );
            return;
        }
        Self::add_locked(&self.inner, &mut links, peer);
    }

    /// The peers kept.
    pub fn peers(&self) -> Vec<Peer> {
        self.inner
            .links
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|l| l.peer.clone())
            .collect()
    }

    fn add_locked(inner: &Arc<Inner>, links: &mut Vec<Link>, peer: &Peer) {
        if let Some(link) = links.iter_mut().find(|l| l.peer.addr == peer.addr)
        {
            link.peer.name.clone_from(&peer.name);
            link.wanted = true;
            return;
        }
        links.push(Link {
            peer: peer.clone(),
            wanted: true,
            sid: None,
            interval: Duration::from_secs(25),
            last_ping: Instant::now(),
            posting: Arc::new(Mutex::new(())),
        });
        let inner = inner.clone();
        let addr = peer.addr;
        let spawned = evo_esp32_sdk::thread::spawn(
            evo_esp32_sdk::thread::Role::Background,
            "peer",
            STACK,
            move || run(inner, addr),
        );
        if let Err(e) = spawned {
            log::warn!("volumio: no thread for {addr}: {e}");
            links.retain(|l| l.peer.addr != addr);
        }
    }

    /// Pings due are sent; call about once a second from a thread that
    /// may wait on a peer.
    pub fn tick(&self) {
        let due: Vec<(SocketAddrV4, String, Arc<Mutex<()>>, u32)> = {
            let mut links =
                self.inner.links.lock().unwrap_or_else(|e| e.into_inner());
            let mut due = Vec::new();
            for link in links.iter_mut() {
                let Some(sid) = &link.sid else { continue };
                if link.last_ping.elapsed() < link.interval {
                    continue;
                }
                link.last_ping = Instant::now();
                due.push((
                    link.peer.addr,
                    sid.clone(),
                    link.posting.clone(),
                    self.next_t(),
                ));
            }
            due
        };
        for (addr, sid, posting, t) in due {
            let _hold = posting.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(e) = post(addr, &sid, t, &wire::control(wire::PING)) {
                log::debug!("volumio: {addr} ping: {e}");
            }
        }
    }

    /// What peers said since the last call.
    pub fn take_heard(&self) -> Vec<Heard> {
        self.inner
            .heard
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect()
    }

    /// How many peers have a session up.
    pub fn connected(&self) -> usize {
        self.heard_from().len()
    }

    /// The peers with a session up.
    pub fn heard_from(&self) -> Vec<SocketAddrV4> {
        self.inner
            .links
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|l| l.sid.is_some())
            .map(|l| l.peer.addr)
            .collect()
    }

    fn next_t(&self) -> u32 {
        let mut t = self.inner.t.lock().unwrap_or_else(|e| e.into_inner());
        *t = t.wrapping_add(1);
        *t
    }
}

impl Inner {
    fn wanted(&self, addr: SocketAddrV4) -> bool {
        self.links
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|l| l.peer.addr == addr && l.wanted)
    }

    fn heard(&self, what: Heard) {
        self.heard
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(what);
    }

    fn next_t(&self) -> u32 {
        let mut t = self.t.lock().unwrap_or_else(|e| e.into_inner());
        *t = t.wrapping_add(1);
        *t
    }

    fn set_session(
        &self,
        addr: SocketAddrV4,
        sid: Option<String>,
        interval: Duration,
    ) {
        let mut links = self.links.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(link) = links.iter_mut().find(|l| l.peer.addr == addr) {
            link.sid = sid;
            link.interval = interval;
            link.last_ping = Instant::now();
        }
    }

    fn posting(&self, addr: SocketAddrV4) -> Option<Arc<Mutex<()>>> {
        self.links
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|l| l.peer.addr == addr)
            .map(|l| l.posting.clone())
    }

    fn forget(&self, addr: SocketAddrV4) {
        self.links
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|l| l.peer.addr != addr);
    }
}

/// A post to a session; the server answers `ok`.
fn post(
    addr: SocketAddrV4,
    sid: &str,
    t: u32,
    payload: &str,
) -> io::Result<()> {
    let res = fetch(
        &addr.to_string(),
        "POST",
        &query(Some(sid), t),
        Some(payload.as_bytes()),
        SHORT,
        SHORT,
    )?;
    if res.status != 200 {
        return Err(io::Error::other(format!(
            "post answered {} {}",
            res.status,
            res.text()
        )));
    }
    Ok(())
}

/// The handshake: the session id and ping interval the peer gives.
fn open(inner: &Inner, addr: SocketAddrV4) -> io::Result<(String, Duration)> {
    let res = fetch(
        &addr.to_string(),
        "GET",
        &query(None, inner.next_t()),
        None,
        SHORT,
        SHORT,
    )?;
    if res.status != 200 {
        return Err(io::Error::other(format!(
            "open answered {} {}",
            res.status,
            res.text()
        )));
    }
    let packets = wire::read_payload(&mut &res.body[..], KEEP)?;
    let Some(Packet::Open(open)) = packets.first().map(|p| wire::parse(p))
    else {
        return Err(io::Error::other("open without an open packet"));
    };
    let sid = open
        .get("sid")
        .and_then(Value::as_str)
        .ok_or_else(|| io::Error::other("open without a sid"))?
        .to_string();
    let interval = open
        .get("pingInterval")
        .and_then(Value::as_u64)
        .map_or(Duration::from_secs(25), Duration::from_millis);
    Ok((sid, interval))
}

/// A session's life, from the hello to the first failure; `Ok` when
/// the peer closed it or we were told to stop.
fn session(
    inner: &Inner,
    addr: SocketAddrV4,
    sid: &str,
    interval: Duration,
) -> io::Result<()> {
    let hello = inner
        .hello
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if !hello.is_empty() {
        let posting = inner.posting(addr);
        let _hold = posting
            .as_ref()
            .map(|p| p.lock().unwrap_or_else(|e| e.into_inner()));
        post(
            addr,
            sid,
            inner.next_t(),
            &wire::encode_payload(hello.iter().map(String::as_str)),
        )?;
    }
    let patience = interval + POLL_GRACE;
    loop {
        if !inner.wanted(addr) {
            let posting = inner.posting(addr);
            let _hold = posting
                .as_ref()
                .map(|p| p.lock().unwrap_or_else(|e| e.into_inner()));
            let _ =
                post(addr, sid, inner.next_t(), &wire::control(wire::CLOSE));
            return Ok(());
        }
        let packets = fetch_with(
            &addr.to_string(),
            "GET",
            &query(Some(sid), inner.next_t()),
            None,
            SHORT,
            patience,
            |status, body| {
                if status != 200 {
                    let mut text = String::new();
                    let _ = body.take(256).read_to_string(&mut text);
                    return Err(io::Error::other(format!(
                        "poll answered {status} {text}"
                    )));
                }
                wire::read_payload(body, KEEP)
            },
        )?;
        for packet in packets {
            match wire::parse(&packet) {
                Packet::Event(name, data) => {
                    inner.heard(Heard::Event { addr, name, data })
                }
                Packet::Close | Packet::Disconnected => return Ok(()),
                Packet::Ping => {
                    // Not in this version; answered all the same.
                    let posting = inner.posting(addr);
                    let _hold = posting
                        .as_ref()
                        .map(|p| p.lock().unwrap_or_else(|e| e.into_inner()));
                    let _ = post(
                        addr,
                        sid,
                        inner.next_t(),
                        &wire::control(wire::PONG),
                    );
                }
                Packet::Pong
                | Packet::Noop
                | Packet::Connected
                | Packet::Open(_) => {}
                Packet::Other(p) => log::debug!("volumio: {addr} sent {p:.40}"),
            }
        }
    }
}

/// The thread for one peer.
fn run(inner: Arc<Inner>, addr: SocketAddrV4) {
    let mut failures = 0u32;
    loop {
        if !inner.wanted(addr) {
            inner.forget(addr);
            return;
        }
        match open(&inner, addr) {
            Ok((sid, interval)) => {
                log::info!(
                    "volumio: {addr} session {sid}, ping every {interval:?}"
                );
                inner.set_session(addr, Some(sid.clone()), interval);
                inner.heard(Heard::Connected { addr });
                failures = 0;
                let ended = session(&inner, addr, &sid, interval);
                inner.set_session(addr, None, interval);
                inner.heard(Heard::Lost { addr });
                match ended {
                    Ok(()) => log::info!("volumio: {addr} session over"),
                    Err(e) => {
                        failures += 1;
                        log::warn!("volumio: {addr} session: {e}");
                    }
                }
            }
            Err(e) => {
                failures += 1;
                if failures <= 2 {
                    log::warn!("volumio: {addr} open: {e}");
                } else {
                    log::debug!("volumio: {addr} open: {e}");
                }
            }
        }
        let wait = if failures == 0 {
            Duration::ZERO
        } else if failures < 4 {
            RETRY
        } else {
            RETRY_LATER
        };
        let until = Instant::now() + wait;
        while Instant::now() < until {
            if !inner.wanted(addr) {
                break;
            }
            std::thread::sleep(RETRY_STEP);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{read_request, write_response};
    use serde_json::json;
    use std::net::{Ipv4Addr, TcpListener};

    const TEXT: &str = "text/plain; charset=UTF-8";

    /// A peer the way the reference server behaves: opens, takes posts,
    /// answers one poll with what it has (or a pong when pinged), and
    /// drops the whole session when a second poll overlaps.
    fn peer() -> (
        SocketAddrV4,
        Arc<Mutex<Vec<String>>>,
        Arc<Mutex<Vec<String>>>,
    ) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = match listener.local_addr().unwrap() {
            std::net::SocketAddr::V4(a) => a,
            _ => unreachable!(),
        };
        let posted = Arc::new(Mutex::new(Vec::new()));
        let to_send = Arc::new(Mutex::new(Vec::new()));
        let (p, s) = (posted.clone(), to_send.clone());
        std::thread::spawn(move || {
            let mut sid_count = 0;
            let mut open_sid: Option<String> = None;
            let mut queued: Vec<String> = Vec::new();
            let mut waiting: Option<std::net::TcpStream> = None;
            listener.set_nonblocking(true).unwrap();
            loop {
                // Push what the test queued into a waiting poll.
                {
                    let mut s = s.lock().unwrap();
                    queued.append(&mut s);
                }
                if !queued.is_empty() {
                    if let Some(mut w) = waiting.take() {
                        let payload = wire::encode_payload(queued.drain(..));
                        write_response(
                            &mut w,
                            200,
                            "OK",
                            TEXT,
                            payload.as_bytes(),
                        )
                        .unwrap();
                    }
                }
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                };
                stream.set_nonblocking(false).unwrap();
                let req = read_request(&stream).unwrap();
                let sid = req.query("sid").map(str::to_string);
                match (req.method.as_str(), sid) {
                    ("GET", None) => {
                        sid_count += 1;
                        let sid = format!("s{sid_count}");
                        open_sid = Some(sid.clone());
                        queued.clear();
                        waiting = None;
                        let open = format!(
                            "0{}",
                            json!({"sid": sid, "upgrades": [], "pingInterval": 300, "pingTimeout": 5000})
                        );
                        let payload =
                            wire::encode_payload([open.as_str(), "40"]);
                        write_response(
                            &mut stream,
                            200,
                            "OK",
                            TEXT,
                            payload.as_bytes(),
                        )
                        .unwrap();
                    }
                    ("GET", Some(sid)) if Some(&sid) == open_sid.as_ref() => {
                        if waiting.is_some() {
                            open_sid = None;
                            write_response(
                                &mut stream,
                                400,
                                "Bad Request",
                                TEXT,
                                b"{\"code\":1}",
                            )
                            .unwrap();
                            continue;
                        }
                        waiting = Some(stream);
                    }
                    ("POST", Some(sid)) if Some(&sid) == open_sid.as_ref() => {
                        for packet in wire::decode_payload(
                            &String::from_utf8_lossy(&req.body),
                        )
                        .unwrap()
                        {
                            if packet == "2" {
                                queued.push("3".into());
                            } else if packet == "1" {
                                open_sid = None;
                            }
                            p.lock().unwrap().push(packet);
                        }
                        write_response(&mut stream, 200, "OK", TEXT, b"ok")
                            .unwrap();
                    }
                    _ => {
                        write_response(
                            &mut stream,
                            400,
                            "Bad Request",
                            TEXT,
                            b"{\"code\":1,\"message\":\"Session ID unknown\"}",
                        )
                        .unwrap();
                    }
                }
            }
        });
        (addr, posted, to_send)
    }

    fn wait_for(
        client: &Client,
        mut found: impl FnMut(&Heard) -> bool,
    ) -> Vec<Heard> {
        let started = Instant::now();
        let mut all = Vec::new();
        while started.elapsed() < Duration::from_secs(5) {
            let heard = client.take_heard();
            let hit = heard.iter().any(&mut found);
            all.extend(heard);
            if hit {
                return all;
            }
            client.tick();
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("not heard in time: {all:?}");
    }

    #[test]
    fn a_session_is_opened_greeted_polled_and_pinged() {
        let (addr, posted, to_send) = peer();
        let client = Client::new();
        client.set_hello(vec![wire::event("initSocket", &json!({"id": "us"}))]);
        client.set_peers(&[Peer {
            name: "Peer".into(),
            addr,
        }]);
        wait_for(&client, |h| matches!(h, Heard::Connected { .. }));
        assert_eq!(client.connected(), 1);
        // The hello went out first.
        let started = Instant::now();
        while posted.lock().unwrap().is_empty()
            && started.elapsed() < Duration::from_secs(2)
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            posted.lock().unwrap()[0],
            "42[\"initSocket\",{\"id\":\"us\"}]"
        );
        // An event from the peer is heard.
        to_send.lock().unwrap().push(wire::event(
            "enableMultiroomSyncOutput",
            &json!({"id": "us", "server": {"host": "http://10.0.0.1"}}),
        ));
        let heard = wait_for(&client, |h| matches!(h, Heard::Event { .. }));
        assert!(heard.contains(&Heard::Event {
            addr,
            name: "enableMultiroomSyncOutput".into(),
            data: json!({"id": "us", "server": {"host": "http://10.0.0.1"}}),
        }));
        // Pings go out at the peer's interval, and come back as pongs.
        std::thread::sleep(Duration::from_millis(400));
        client.tick();
        std::thread::sleep(Duration::from_millis(100));
        assert!(posted.lock().unwrap().iter().any(|p| p == "2"));
        assert_eq!(client.connected(), 1);
        // Letting the peer go closes the session.
        client.set_peers(&[]);
        wait_for(&client, |h| matches!(h, Heard::Lost { .. }));
        let started = Instant::now();
        while !posted.lock().unwrap().iter().any(|p| p == "1")
            && started.elapsed() < Duration::from_secs(2)
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(posted.lock().unwrap().iter().any(|p| p == "1"));
        assert_eq!(client.connected(), 0);
    }

    #[test]
    fn a_peer_that_is_not_there_is_retried_without_noise() {
        let client = Client::new();
        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 1);
        client.set_peers(&[Peer {
            name: "Nobody".into(),
            addr,
        }]);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(client.connected(), 0);
        assert!(client.take_heard().is_empty());
        client.set_peers(&[]);
        std::thread::sleep(Duration::from_millis(600));
        assert!(client.inner.links.lock().unwrap().is_empty());
    }
}

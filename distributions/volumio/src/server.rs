// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The server half: peers that find us connect to port 3000 and speak
//! Engine.IO 3 over polling. A session is a queue of packets to the
//! peer and, when the peer has a poll open, the connection to answer it
//! with. What the peer sends arrives as posts and is handed to the
//! owner, who decides what goes back. Answers to held polls are written
//! by a thread of the server's own: the owner announces every change of
//! state from the thread that paints, and a write to a peer that has
//! gone quiet blocks for its timeout.

use std::collections::VecDeque;
use std::io;
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::http::{read_request, write_response, write_response_with, Request};
use crate::wire::{self, Packet};
use crate::{Art, ART_PATH};

/// What we tell a peer at the handshake; the client pings at the
/// interval and gives up after the timeout without a pong.
const PING_INTERVAL_MS: u64 = 25_000;
const PING_TIMEOUT_MS: u64 = 60_000;
/// A session that has not been heard from for this long is gone.
const SESSION_TTL: Duration =
    Duration::from_millis(PING_INTERVAL_MS + PING_TIMEOUT_MS);
/// A session opened but never polled is one the peer gave up on while
/// settling (the reference client opens afresh when told to connect
/// before its last attempt finished); it is let go this much sooner.
const UNPOLLED_TTL: Duration = Duration::from_secs(10);
/// A poll held this long is released with a noop, so a peer's stale
/// request does not hold a socket.
const POLL_HOLD: Duration = Duration::from_secs(50);
/// A peer gets this long to send its request once connected.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Between looks at the listener when nothing is arriving.
const ACCEPT_PAUSE: Duration = Duration::from_millis(100);
/// The most sessions we keep; the rest are refused at the handshake.
const SESSION_LIMIT: usize = 6;
/// Packets longer than this, in UTF-16 units, in a post are passed
/// over; what a peer asks of us is short.
const POST_KEEP: usize = 4096;
/// A picture's send may take this long before it is given up.
const ART_TIMEOUT: Duration = Duration::from_secs(20);
/// The stack of the thread that sends it.
const ART_STACK: usize = 6 * 1024;
/// The stack of the thread that answers held polls: a payload is
/// formatted by the caller; this thread only writes it.
const OUTBOX_STACK: usize = 4 * 1024;

const TEXT: &str = "text/plain; charset=UTF-8";

/// An answer to a held poll, waiting for the outbox thread to write it.
struct Answer {
    stream: TcpStream,
    peer: SocketAddr,
    payload: String,
}

/// Answers queued for the outbox thread, and its wake-up.
#[derive(Default)]
struct Outbox {
    queue: Mutex<VecDeque<Answer>>,
    ready: Condvar,
}

impl Outbox {
    fn post(&self, answer: Answer) {
        self.queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(answer);
        self.ready.notify_one();
    }

    /// Write answers as they come, for the life of the server.
    fn run(&self) {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            let Some(mut answer) = queue.pop_front() else {
                queue =
                    self.ready.wait(queue).unwrap_or_else(|e| e.into_inner());
                continue;
            };
            drop(queue);
            if let Err(e) = write_response(
                &mut answer.stream,
                200,
                "OK",
                TEXT,
                answer.payload.as_bytes(),
            ) {
                log::debug!("volumio: {} poll answer: {e}", answer.peer);
            }
            let _ = answer.stream.shutdown(Shutdown::Both);
            queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// What arrived from a peer, for the owner to act on.
#[derive(Debug, Clone, PartialEq)]
pub enum Inbound {
    /// A peer introduced itself with its id.
    Hello {
        /// Its session.
        sid: String,
        /// The id it gave.
        id: String,
        /// Where it connected from.
        addr: SocketAddr,
    },
    /// A peer asked for something by event name, with the data it sent.
    Event {
        /// Its session.
        sid: String,
        /// The event's name.
        name: String,
        /// The event's data.
        data: Value,
    },
    /// A session ended.
    Bye {
        /// Its session.
        sid: String,
    },
}

struct Session {
    sid: String,
    peer: SocketAddr,
    queue: VecDeque<String>,
    waiting: Option<(TcpStream, Instant)>,
    last_seen: Instant,
    polled: bool,
}

impl Session {
    /// Answer the open poll with what is queued, if both are there: the
    /// answer goes to `outbox`, whose thread writes it.
    fn flush(&mut self, outbox: &Outbox) {
        if self.queue.is_empty() {
            return;
        }
        if let Some((stream, _)) = self.waiting.take() {
            let packets: Vec<String> = self.queue.drain(..).collect();
            let payload =
                wire::encode_payload(packets.iter().map(String::as_str));
            outbox.post(Answer {
                stream,
                peer: self.peer,
                payload,
            });
        }
    }

    fn release(&mut self, why: &str, outbox: &Outbox) {
        if let Some((stream, _)) = self.waiting.take() {
            outbox.post(Answer {
                stream,
                peer: self.peer,
                payload: wire::encode_payload([why]),
            });
        }
    }
}

/// The sessions, shared between the listener thread and whoever pushes.
#[derive(Default)]
struct Sessions {
    list: Vec<Session>,
    next: u32,
}

impl Sessions {
    fn get(&mut self, sid: &str) -> Option<&mut Session> {
        self.list.iter_mut().find(|s| s.sid == sid)
    }

    fn remove(&mut self, sid: &str) -> Option<Session> {
        let at = self.list.iter().position(|s| s.sid == sid)?;
        Some(self.list.remove(at))
    }
}

/// Our side of the polling transport, listening.
#[derive(Clone)]
pub struct Server {
    sessions: Arc<Mutex<Sessions>>,
    inbound: Arc<Mutex<VecDeque<Inbound>>>,
    /// The picture served at [`ART_PATH`]: what plays, else the tile.
    art: Arc<Mutex<Option<Art>>>,
    tile: Art,
    outbox: Arc<Outbox>,
}

impl Server {
    /// A server with no listener yet; `serve` binds and runs. `tile` is
    /// the picture served while none is set.
    pub fn new(tile: Art) -> Self {
        Self {
            sessions: Arc::new(Mutex::new(Sessions::default())),
            inbound: Arc::new(Mutex::new(VecDeque::new())),
            art: Arc::new(Mutex::new(None)),
            tile,
            outbox: Arc::new(Outbox::default()),
        }
    }

    /// The picture to serve; `None` for the tile.
    pub fn set_art(&self, art: Option<Art>) {
        *self.art.lock().unwrap_or_else(|e| e.into_inner()) = art;
    }

    /// Bind `port` and serve until the listener fails; meant for its own
    /// thread.
    pub fn serve(&self, port: u16) -> io::Result<()> {
        let listener = TcpListener::bind(("0.0.0.0", port))?;
        listener.set_nonblocking(true)?;
        log::info!("volumio: listening on {port}");
        let mut last_sweep = Instant::now();
        loop {
            match listener.accept() {
                Ok((stream, peer)) => {
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));
                    let _ = stream.set_write_timeout(Some(REQUEST_TIMEOUT));
                    let _ = stream.set_nodelay(true);
                    self.handle(stream, peer);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(ACCEPT_PAUSE);
                }
                Err(e) => {
                    log::warn!("volumio: accept: {e}");
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
            if last_sweep.elapsed() >= Duration::from_secs(1) {
                last_sweep = Instant::now();
                self.sweep();
            }
        }
    }

    /// Sessions not heard from are dropped; polls held too long are
    /// released.
    fn sweep(&self) {
        let mut gone = Vec::new();
        {
            let mut sessions =
                self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            sessions.list.retain_mut(|s| {
                let ttl = if s.polled { SESSION_TTL } else { UNPOLLED_TTL };
                if s.last_seen.elapsed() > ttl {
                    log::info!(
                        "volumio: {} session {} {}",
                        s.peer,
                        s.sid,
                        if s.polled {
                            "timed out"
                        } else {
                            "never polled"
                        }
                    );
                    s.release(&wire::CLOSE.to_string(), &self.outbox);
                    gone.push(s.sid.clone());
                    return false;
                }
                if let Some((_, since)) = &s.waiting {
                    if since.elapsed() > POLL_HOLD {
                        s.release(&wire::NOOP.to_string(), &self.outbox);
                    }
                }
                true
            });
        }
        let mut inbound =
            self.inbound.lock().unwrap_or_else(|e| e.into_inner());
        for sid in gone {
            inbound.push_back(Inbound::Bye { sid });
        }
    }

    fn handle(&self, mut stream: TcpStream, peer: SocketAddr) {
        let request = match read_request(&stream) {
            Ok(r) => r,
            Err(e) => {
                log::debug!("volumio: {peer} request: {e}");
                return;
            }
        };
        if request.path() == ART_PATH {
            self.serve_art(stream, peer);
            return;
        }
        if request.path() != "/socket.io/" {
            let _ = write_response(&mut stream, 404, "Not Found", TEXT, b"");
            return;
        }
        if request.query("transport") != Some("polling") {
            // A websocket upgrade attempt or something else; we offered
            // no upgrades.
            let _ = write_response(
                &mut stream,
                400,
                "Bad Request",
                TEXT,
                b"{\"code\":3}",
            );
            return;
        }
        match (request.method.as_str(), request.query("sid")) {
            ("GET", None) => self.open(stream, peer),
            ("GET", Some(sid)) => self.poll(stream, peer, sid),
            ("POST", Some(sid)) => self.post(stream, peer, sid, &request),
            _ => {
                let _ = write_response(
                    &mut stream,
                    400,
                    "Bad Request",
                    TEXT,
                    b"{\"code\":1}",
                );
            }
        }
    }

    /// The picture, written from a thread of its own: a cover is a few
    /// hundred kilobytes and a peer's browser may take its time, and the
    /// polls must not wait for it.
    fn serve_art(&self, mut stream: TcpStream, peer: SocketAddr) {
        let art = self
            .art
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_else(|| self.tile.clone());
        let spawned = std::thread::Builder::new()
            .name("art".into())
            .stack_size(ART_STACK)
            .spawn(move || {
                let _ = stream.set_write_timeout(Some(ART_TIMEOUT));
                if let Err(e) = write_response_with(
                    &mut stream,
                    200,
                    "OK",
                    art.mime,
                    "Cache-Control: max-age=86400\r\n",
                    &art.bytes,
                ) {
                    log::debug!("volumio: {peer} art: {e}");
                }
                let _ = stream.shutdown(Shutdown::Both);
            },
        );
        if let Err(e) = spawned {
            log::warn!("volumio: no thread for the picture: {e}");
        }
    }

    fn open(&self, mut stream: TcpStream, peer: SocketAddr) {
        let sid = {
            let mut sessions =
                self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            if sessions.list.len() >= SESSION_LIMIT {
                drop(sessions);
                log::warn!(
                    "volumio: {peer} refused, {SESSION_LIMIT} sessions already"
                );
                let _ = write_response(&mut stream, 503, "Busy", TEXT, b"");
                return;
            }
            sessions.next = sessions.next.wrapping_add(1);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u32)
                .unwrap_or(0);
            let sid = format!(
                "{:08x}{:08x}",
                sessions.next,
                nanos ^ ((peer.port() as u32) << 16)
            );
            sessions.list.push(Session {
                sid: sid.clone(),
                peer,
                queue: VecDeque::new(),
                waiting: None,
                last_seen: Instant::now(),
                polled: false,
            });
            sid
        };
        let open = json!({
            "sid": sid,
            "upgrades": [],
            "pingInterval": PING_INTERVAL_MS,
            "pingTimeout": PING_TIMEOUT_MS,
        });
        let first = format!("{}{open}", wire::OPEN);
        let payload = wire::encode_payload([first.as_str(), wire::CONNECTED]);
        if let Err(e) =
            write_response(&mut stream, 200, "OK", TEXT, payload.as_bytes())
        {
            log::debug!("volumio: {peer} open: {e}");
        }
        log::info!("volumio: {peer} connected as {sid}");
    }

    fn poll(&self, mut stream: TcpStream, peer: SocketAddr, sid: &str) {
        let mut sessions =
            self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let Some(session) = sessions.get(sid) else {
            drop(sessions);
            let _ = write_response(
                &mut stream,
                400,
                "Bad Request",
                TEXT,
                b"{\"code\":1}",
            );
            return;
        };
        session.last_seen = Instant::now();
        session.polled = true;
        if session.waiting.is_some() {
            // The reference server treats two polls at once as a fault
            // and closes the session; so do we.
            log::warn!("volumio: {peer} polled twice, closing {sid}");
            session.release(&wire::CLOSE.to_string(), &self.outbox);
            let _ = write_response(
                &mut stream,
                400,
                "Bad Request",
                TEXT,
                b"{\"code\":1}",
            );
            sessions.remove(sid);
            drop(sessions);
            self.inbound
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_back(Inbound::Bye {
                    sid: sid.to_string(),
                });
            return;
        }
        // A held poll may wait a long time; no read is pending on it.
        let _ = stream.set_write_timeout(Some(REQUEST_TIMEOUT));
        session.waiting = Some((stream, Instant::now()));
        session.flush(&self.outbox);
    }

    fn post(
        &self,
        mut stream: TcpStream,
        peer: SocketAddr,
        sid: &str,
        request: &Request,
    ) {
        let packets =
            match wire::read_payload(&mut &request.body[..], POST_KEEP) {
                Ok(p) => p,
                Err(e) => {
                    log::debug!("volumio: {peer} payload: {e}");
                    let _ = write_response(
                        &mut stream,
                        400,
                        "Bad Request",
                        TEXT,
                        b"{\"code\":4}",
                    );
                    return;
                }
            };
        let mut events = Vec::new();
        let mut bye = false;
        {
            let mut sessions =
                self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            let Some(session) = sessions.get(sid) else {
                drop(sessions);
                let _ = write_response(
                    &mut stream,
                    400,
                    "Bad Request",
                    TEXT,
                    b"{\"code\":1}",
                );
                return;
            };
            session.last_seen = Instant::now();
            for packet in packets {
                match wire::parse(&packet) {
                    Packet::Ping => {
                        session.queue.push_back(wire::PONG.to_string())
                    }
                    Packet::Close | Packet::Disconnected => bye = true,
                    Packet::Event(name, data) => events.push((name, data)),
                    Packet::Connected | Packet::Noop | Packet::Pong => {}
                    other => log::debug!("volumio: {peer} sent {other:?}"),
                }
            }
            session.flush(&self.outbox);
            if bye {
                if let Some(mut s) = sessions.remove(sid) {
                    s.release(&wire::CLOSE.to_string(), &self.outbox);
                }
            }
        }
        let _ = write_response(&mut stream, 200, "OK", TEXT, b"ok");
        let mut inbound =
            self.inbound.lock().unwrap_or_else(|e| e.into_inner());
        for (name, data) in events {
            if name == "initSocket" {
                if let Some(id) = data.get("id").and_then(Value::as_str) {
                    inbound.push_back(Inbound::Hello {
                        sid: sid.to_string(),
                        id: id.to_string(),
                        addr: peer,
                    });
                    continue;
                }
            }
            inbound.push_back(Inbound::Event {
                sid: sid.to_string(),
                name,
                data,
            });
        }
        if bye {
            log::info!("volumio: {peer} left {sid}");
            inbound.push_back(Inbound::Bye {
                sid: sid.to_string(),
            });
        }
    }

    /// What peers sent since the last call.
    pub fn take_inbound(&self) -> Vec<Inbound> {
        self.inbound
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect()
    }

    /// Queue a packet to one session, answering its poll if open.
    pub fn send(&self, sid: &str, packet: &str) {
        let mut sessions =
            self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(session) = sessions.get(sid) {
            session.queue.push_back(packet.to_string());
            session.flush(&self.outbox);
        }
    }

    /// Queue a packet to every session. Nothing is written here: the
    /// caller is the thread that paints.
    pub fn broadcast(&self, packet: &str) {
        let mut sessions =
            self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        for session in &mut sessions.list {
            session.queue.push_back(packet.to_string());
            session.flush(&self.outbox);
        }
    }

    /// How many peers hold a session.
    pub fn connected(&self) -> usize {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .list
            .len()
    }
}

/// A request's query to a peer's server, as the reference client forms
/// it; `b64=1` asks for the text framing, which an older server would
/// otherwise not use for a client it takes to handle binary.
pub fn query(sid: Option<&str>, t: u32) -> String {
    match sid {
        Some(sid) => {
            format!("/socket.io/?EIO=3&transport=polling&t={t}&b64=1&sid={sid}")
        }
        None => format!("/socket.io/?EIO=3&transport=polling&t={t}&b64=1"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::fetch;
    use std::net::TcpListener;

    fn free_port() -> u16 {
        TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn get(
        port: u16,
        target: &str,
        patience: Duration,
    ) -> crate::http::Response {
        fetch(
            &format!("127.0.0.1:{port}"),
            "GET",
            target,
            None,
            Duration::from_secs(2),
            patience,
        )
        .unwrap()
    }

    fn post(port: u16, target: &str, body: &str) -> crate::http::Response {
        fetch(
            &format!("127.0.0.1:{port}"),
            "POST",
            target,
            Some(body.as_bytes()),
            Duration::from_secs(2),
            Duration::from_secs(2),
        )
        .unwrap()
    }

    fn running() -> (Server, u16) {
        let port = free_port();
        let server = Server::new(Art::tile());
        let s = server.clone();
        std::thread::spawn(move || s.serve(port));
        // Until it listens.
        for _ in 0..50 {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        (server, port)
    }

    #[test]
    fn a_peer_opens_posts_and_is_answered_through_its_poll() {
        let (server, port) = running();
        let res = get(port, &query(None, 1), Duration::from_secs(2));
        assert_eq!(res.status, 200);
        assert_eq!(res.header("content-type"), Some(TEXT));
        let packets = wire::decode_payload(&res.text()).unwrap();
        let Packet::Open(open) = wire::parse(&packets[0]) else {
            panic!("{packets:?}")
        };
        assert_eq!(packets[1], "40");
        assert_eq!(open["pingInterval"], 25000);
        let sid = open["sid"].as_str().unwrap().to_string();
        assert_eq!(server.connected(), 1);

        // The peer introduces itself and asks, all in one post.
        let body = wire::encode_payload([
            "42[\"initSocket\",{\"id\":\"peer-1\"}]",
            "42[\"getState\",\"\"]",
            "2",
        ]);
        let res = post(port, &query(Some(&sid), 2), &body);
        assert_eq!(res.text(), "ok");
        let inbound = server.take_inbound();
        assert_eq!(inbound.len(), 2);
        match &inbound[0] {
            Inbound::Hello { sid: s, id, addr } => {
                assert_eq!(s, &sid);
                assert_eq!(id, "peer-1");
                assert!(addr.ip().is_loopback());
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            inbound[1],
            Inbound::Event {
                sid: sid.clone(),
                name: "getState".into(),
                data: json!("")
            }
        );
        // The pong is queued; the poll gets it at once along with what
        // the owner queued.
        server
            .send(&sid, &wire::event("pushState", &json!({"status": "play"})));
        let res = get(port, &query(Some(&sid), 3), Duration::from_secs(2));
        let packets = wire::decode_payload(&res.text()).unwrap();
        assert_eq!(packets[0], "3");
        assert_eq!(
            wire::parse(&packets[1]),
            Packet::Event("pushState".into(), json!({"status": "play"}))
        );

        // A poll with nothing queued waits until something is sent.
        let s = server.clone();
        let sid2 = sid.clone();
        let pusher = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            s.broadcast("42[\"pushState\",{\"status\":\"stop\"}]");
        });
        let started = Instant::now();
        let res = get(port, &query(Some(&sid2), 4), Duration::from_secs(5));
        assert!(started.elapsed() >= Duration::from_millis(250));
        assert_eq!(
            wire::decode_payload(&res.text()).unwrap(),
            vec!["42[\"pushState\",{\"status\":\"stop\"}]"]
        );
        pusher.join().unwrap();

        // Leaving ends the session.
        let res = post(port, &query(Some(&sid), 5), "1:1");
        assert_eq!(res.text(), "ok");
        assert_eq!(
            server.take_inbound(),
            vec![Inbound::Bye { sid: sid.clone() }]
        );
        assert_eq!(server.connected(), 0);
        let res = get(port, &query(Some(&sid), 6), Duration::from_secs(2));
        assert_eq!(res.status, 400);
    }

    #[test]
    fn other_requests_are_turned_away() {
        let (server, port) = running();
        assert_eq!(get(port, "/", Duration::from_secs(2)).status, 404);
        // The picture: the tile until one is set, then that.
        let res = get(port, ART_PATH, Duration::from_secs(5));
        assert_eq!(res.status, 200);
        assert_eq!(res.header("content-type"), Some("image/png"));
        assert_eq!(res.body, crate::TILE_PNG);
        server.set_art(Art::sniff(vec![0xff, 0xd8, 0xff, 1, 2, 3]));
        let res = get(port, &format!("{ART_PATH}?v=2"), Duration::from_secs(5));
        assert_eq!(res.header("content-type"), Some("image/jpeg"));
        assert_eq!(res.body, vec![0xff, 0xd8, 0xff, 1, 2, 3]);
        assert!(Art::sniff(vec![1, 2, 3]).is_none());
        assert_eq!(
            get(
                port,
                "/socket.io/?EIO=3&transport=websocket",
                Duration::from_secs(2)
            )
            .status,
            400
        );
        assert_eq!(
            post(port, "/socket.io/?EIO=3&transport=polling&sid=nope", "1:2")
                .status,
            400
        );
    }
}

// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The presence on its own, for watching from a Volumio or a script:
//!
//! ```text
//! presence <name> <uuid> <our-ip> [peer-ip:port ...]
//! ```
//!
//! Listens on port 3000, keeps a session to each peer given, prints what
//! leaders ask, and changes what plays every half minute so a watcher
//! sees the pushes.

use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::Duration;

use evo_esp32_volumio::{peer, Identity, Playing, Presence, Status, PORT};

/// Lines to stderr; the crate speaks through `log`.
struct Stderr;

impl log::Log for Stderr {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::Level::Debug
    }
    fn log(&self, r: &log::Record) {
        if self.enabled(r.metadata()) {
            eprintln!("{:5} {}", r.level(), r.args());
        }
    }
    fn flush(&self) {}
}

fn main() {
    let _ = log::set_logger(&Stderr);
    log::set_max_level(log::LevelFilter::Debug);
    let mut args = std::env::args().skip(1);
    let (Some(name), Some(uuid), Some(ip)) =
        (args.next(), args.next(), args.next())
    else {
        eprintln!("presence <name> <uuid> <our-ip> [peer-ip:port ...]");
        std::process::exit(2);
    };
    let ip: Ipv4Addr = ip.parse().expect("an IPv4 address");
    let peers: Vec<_> = args
        .map(|a| {
            let addr: SocketAddrV4 = a.parse().expect("peer as ip:port");
            peer(&a, addr)
        })
        .collect();

    let presence = Presence::new(Identity { name, uuid });
    presence.set_address(Some(ip));
    presence.start(PORT).expect("threads");
    presence.set_peers(&peers);

    let tracks = [
        (Status::Play, "First", "Someone"),
        (Status::Pause, "First", "Someone"),
        (Status::Play, "Second", "Someone Else"),
        (Status::Stop, "", ""),
    ];
    let mut at = 0;
    loop {
        for _ in 0..30 {
            while let Some(command) = presence.take_command() {
                println!("command: {command:?}");
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        let (status, title, artist) = tracks[at % tracks.len()];
        at += 1;
        presence.set_playing(Playing {
            status,
            title: title.into(),
            artist: artist.into(),
            album: String::new(),
        });
        let (served, heard) = presence.connections();
        println!("now {status:?} {title:?}; {served} connected to us, hearing {heard}");
    }
}

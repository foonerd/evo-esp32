// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The network, joined on its own thread so the music does not wait for
//! the search. The companion's `WifiNet` does the joining; this module
//! owns the thread, keeps the link up, and tells the main loop what it
//! has so Settings and `evostatus.txt` can say.

use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::Duration;

use companion_platform_esp_idf::{Band, NvsStore, WifiNet};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::hal::task::thread::ThreadSpawnConfiguration;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sys;
use log::{info, warn};

/// The thread's stack: the driver's own tasks have theirs, this one
/// only configures, waits and formats. It stays in internal SRAM: the
/// thread writes NVS, and a task whose stack is in PSRAM may not run
/// while the flash cache is off.
const STACK: usize = 8 * 1024;
/// Below the main task's (2, raised by the art fetcher): the join's key
/// derivation is a few hundred milliseconds of arithmetic, and the
/// decoder is not to wait for it.
const PRIORITY: u8 = 1;
/// How often the link is checked once up.
const WATCH: Duration = Duration::from_secs(2);
/// How long to wait after a failed join before the next.
const RETRY: Duration = Duration::from_secs(10);
/// The namespace in NVS where the remembered access point lives.
const NAMESPACE: &str = "evo";

/// What the main loop learns of the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Joined, with an address.
    Up {
        /// The network's name.
        ssid: String,
        /// The station's address.
        address: String,
        /// The hostname the router was told.
        hostname: String,
        /// The channel, which says the band.
        channel: u8,
        /// The signal at the join, dBm; 0 when the driver did not say.
        rssi: i8,
    },
    /// The link went away; a join is being tried again.
    Down,
    /// The driver would not start or the join failed for good.
    Failed(String),
}

/// The network's side the main loop holds.
pub struct Net {
    events: Receiver<Event>,
    /// The last thing the thread said.
    pub state: State,
}

/// What the face and the status file show.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum State {
    /// Not asked for, or still joining.
    #[default]
    Joining,
    /// On the network.
    Up {
        /// The network's name.
        ssid: String,
        /// The station's address.
        address: String,
        /// The hostname the router was told.
        hostname: String,
        /// The channel.
        channel: u8,
        /// The signal at the join, dBm; 0 when unknown.
        rssi: i8,
    },
    /// Not on the network; why.
    Off(String),
}

/// Below this the link is called weak: packets go out at the lowest
/// rates and are retried, and a stream's timing wanders.
const WEAK_DBM: i8 = -75;

impl State {
    /// One line for Settings and the status file.
    pub fn line(&self) -> String {
        match self {
            State::Joining => "joining".to_string(),
            State::Up {
                address,
                channel,
                rssi,
                ..
            } => {
                let band = match Band::of_channel(*channel) {
                    Band::Five => "5 GHz",
                    _ => "2.4 GHz",
                };
                if *rssi == 0 {
                    format!("{address} ({band})")
                } else {
                    format!("{address} ({band}, {rssi} dBm)")
                }
            }
            State::Off(why) => why.clone(),
        }
    }

    /// A word for the status strip when the link is one a stream will
    /// feel: the 2.4 GHz band (a round trip of tens to hundreds of
    /// milliseconds against single figures on 5 GHz, measured on the
    /// bench) or a weak signal on either. Empty when the link is good
    /// or there is none.
    pub fn caution(&self) -> &'static str {
        match self {
            State::Up { channel, rssi, .. } => {
                let two_four =
                    !matches!(Band::of_channel(*channel), Band::Five);
                match (two_four, *rssi != 0 && *rssi < WEAK_DBM) {
                    (true, true) => "weak 2.4 GHz Wi-Fi",
                    (true, false) => "2.4 GHz Wi-Fi",
                    (false, true) => "weak Wi-Fi",
                    (false, false) => "",
                }
            }
            _ => "",
        }
    }
}

impl Net {
    /// Start the network thread: it joins `ssid` with `password` (empty
    /// for an open network) and tells `hostname` to the router, then
    /// watches the link and rejoins when it drops. Internal heap is
    /// logged before and after the driver starts, which is the measure
    /// of what the network costs this board.
    pub fn start(
        modem: Modem<'static>,
        nvs: EspDefaultNvsPartition,
        ssid: String,
        password: String,
        hostname: Option<String>,
    ) -> Result<Self, std::io::Error> {
        let (tx, events) = channel::<Event>();
        let before = ThreadSpawnConfiguration::get();
        ThreadSpawnConfiguration {
            name: Some(c"net"),
            stack_size: STACK,
            priority: PRIORITY,
            ..Default::default()
        }
        .set()
        .map_err(std::io::Error::other)?;
        let spawned = thread::Builder::new()
            .name("net".into())
            .stack_size(STACK)
            .spawn(move || {
                work(modem, nvs, &ssid, &password, hostname.as_deref(), &tx)
            });
        before
            .unwrap_or_default()
            .set()
            .map_err(std::io::Error::other)?;
        spawned?;
        Ok(Self {
            events,
            state: State::Joining,
        })
    }

    /// The next thing the thread said, if anything; `state` is kept.
    pub fn poll(&mut self) -> Option<Event> {
        let event = match self.events.try_recv() {
            Ok(e) => e,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => {
                // The thread is gone; its last word stands.
                return None;
            }
        };
        self.state = match &event {
            Event::Up {
                ssid,
                address,
                hostname,
                channel,
                rssi,
            } => State::Up {
                ssid: ssid.clone(),
                address: address.clone(),
                hostname: hostname.clone(),
                channel: *channel,
                rssi: *rssi,
            },
            Event::Down => State::Joining,
            Event::Failed(why) => State::Off(why.clone()),
        };
        Some(event)
    }
}

fn work(
    modem: Modem<'static>,
    nvs: EspDefaultNvsPartition,
    ssid: &str,
    password: &str,
    hostname: Option<&str>,
    tx: &Sender<Event>,
) {
    let before = free_internal();
    let started = (|| -> Result<(WifiNet<'static>, NvsStore), String> {
        let sys_loop = EspSystemEventLoop::take()
            .map_err(|e| format!("event loop: {e}"))?;
        let store = NvsStore::open(nvs.clone(), NAMESPACE)
            .map_err(|e| format!("nvs: {e}"))?;
        let mut net = WifiNet::start(modem, sys_loop, nvs, hostname)
            .map_err(|e| format!("start: {e}"))?;
        net.set_band(Band::Both);
        Ok((net, store))
    })();
    let (mut net, mut store) = match started {
        Ok(ok) => ok,
        Err(why) => {
            warn!("net: {why}; running without a network");
            let _ = tx.send(Event::Failed(why));
            return;
        }
    };
    info!(
        "net: driver started; internal heap {before} B before, {} B after",
        free_internal()
    );
    // A mains-powered player: the modem stays awake, so a stream's
    // packets are not held for the next beacon (with the driver's
    // default, a round trip wandered to two seconds). Read back, since
    // the driver may say yes and keep its own counsel.
    // SAFETY: plain setter and getter on the started driver.
    unsafe {
        let set = sys::esp_wifi_set_ps(sys::wifi_ps_type_t_WIFI_PS_NONE);
        let mut now = sys::wifi_ps_type_t_WIFI_PS_MIN_MODEM;
        let got = sys::esp_wifi_get_ps(&mut now);
        if set == sys::ESP_OK
            && got == sys::ESP_OK
            && now == sys::wifi_ps_type_t_WIFI_PS_NONE
        {
            info!("net: power save off");
        } else {
            warn!(
                "net: power save stays on (set {set}, get {got}, mode {now})"
            );
        }
    }
    loop {
        match net.join(&mut store, ssid, password) {
            Ok(()) => {
                let joined = Event::Up {
                    ssid: ssid.to_string(),
                    address: net.address().unwrap_or_default(),
                    hostname: net.hostname(),
                    channel: net.joined_ap().map_or(0, |ap| ap.channel),
                    rssi: signal(),
                };
                info!("net: {joined:?}; internal heap {} B", free_internal());
                if tx.send(joined).is_err() {
                    return;
                }
            }
            Err(e) => {
                warn!("net: could not join '{ssid}': {e}; again in {RETRY:?}");
                thread::sleep(RETRY);
                continue;
            }
        }
        // Up: watch the link until it drops, then round again.
        loop {
            thread::sleep(WATCH);
            if !net.wifi().is_up().unwrap_or(false) {
                warn!("net: link down; rejoining");
                if tx.send(Event::Down).is_err() {
                    return;
                }
                break;
            }
        }
    }
}

/// Internal heap free, bytes: what the Wi-Fi stack competes for.
fn free_internal() -> usize {
    // SAFETY: a plain getter with no preconditions.
    unsafe { sys::heap_caps_get_free_size(sys::MALLOC_CAP_INTERNAL) }
}

/// The link as the driver sees it now, for the heartbeat: band and
/// channel, signal, and whether power save is still off (a modem that
/// sleeps holds packets for the next beacon).
pub fn link_line() -> String {
    // SAFETY: zeroed records the driver fills; plain getters.
    let (channel, rssi, ps) = unsafe {
        let mut ap: sys::wifi_ap_record_t = std::mem::zeroed();
        let joined = sys::esp_wifi_sta_get_ap_info(&mut ap) == sys::ESP_OK;
        let mut ps = sys::wifi_ps_type_t_WIFI_PS_MIN_MODEM;
        let got = sys::esp_wifi_get_ps(&mut ps);
        (
            if joined { ap.primary } else { 0 },
            if joined { ap.rssi } else { 0 },
            if got == sys::ESP_OK { Some(ps) } else { None },
        )
    };
    let band = match Band::of_channel(channel) {
        Band::Five => "5 GHz",
        _ => "2.4 GHz",
    };
    let ps = match ps {
        Some(sys::wifi_ps_type_t_WIFI_PS_NONE) => "off",
        Some(_) => "ON",
        None => "?",
    };
    format!("{band} channel {channel}, {rssi} dBm, power save {ps}")
}

/// The signal from the access point, dBm; 0 when the driver has no
/// record (not joined).
fn signal() -> i8 {
    // SAFETY: a zeroed record the driver fills; a plain getter.
    unsafe {
        let mut ap: sys::wifi_ap_record_t = std::mem::zeroed();
        if sys::esp_wifi_sta_get_ap_info(&mut ap) == sys::ESP_OK {
            ap.rssi
        } else {
            0
        }
    }
}

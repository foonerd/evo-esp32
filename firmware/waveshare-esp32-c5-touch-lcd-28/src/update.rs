// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Firmware updates: the slots, the writer, the health gate, the page.
//!
//! The flash holds three application slots (`partitions.csv`): `factory`,
//! what USB flashes and the bootloader's last resort, and `ota_0`/`ota_1`,
//! which updates alternate between. An update is written into whichever
//! of the two is not running, the bootloader is told to boot it, and the
//! player restarts. With the bootloader's rollback on, the new firmware
//! boots once as *pending*: this module confirms it after a healthy
//! start — the panel lit, the card listed, the sink opened when there is
//! something to play, Wi-Fi joined when the card names a network (or a
//! few minutes passed trying), and the loop turning for a minute — and
//! on a reset before that, or on the gate failing, the bootloader returns
//! to the slot the update came from. A confirmed firmware may be kept as
//! the factory copy, a deliberate act from the page, so the last resort
//! follows what the owner has accepted.
//!
//! The page is ESP-IDF's HTTP server on port 80: `GET /` shows what the
//! slots hold and takes a file; `POST /update` takes the image, from a
//! browser's form or a script's body; `POST /factory` keeps the running
//! firmware as the factory copy; `GET /status` is the same as text. The
//! server's task runs at the background tier with its stack in internal
//! memory: it writes flash, and a task whose stack is in PSRAM must not.
//! Playback is stopped before the flash is written — the erase of each
//! 64 KB block stalls every task that runs from flash — and the face says
//! what is happening.

use std::ffi::CStr;
use std::fmt::Write as _;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use esp_idf_svc::http::server::{
    Configuration, EspHttpConnection, EspHttpServer, Request,
};
use esp_idf_svc::http::Method;
use esp_idf_svc::io::{Read, Write};
use esp_idf_svc::sys;
use evo_esp32_update::lock::Lock;
use evo_esp32_update::{
    image_length, outcome_page, Form, Framing, Slot, Status, IMAGE_MAGIC,
};
use log::{info, warn};

/// The page's port.
pub const PORT: u16 = 80;
/// The server task's stack: the handler reads 4 KB at a time and writes
/// flash; internal memory, as flash writes need.
const SERVER_STACK: usize = 10 * 1024;
/// A browser and a script at once; a third waits.
const SERVER_SOCKETS: usize = 3;
/// The read size from the request; a flash sector.
const CHUNK: usize = 4096;
/// The pause after each chunk is in the flash. A flash erase or write
/// runs with interrupts off, and back to back they stop the kernel's
/// tick: a task asleep for 50 ms slept for seconds of wall time and the
/// loop missed its watchdog (measured: four trips per 4 MB image). Two
/// ticks between chunks keep time moving for everyone; the write takes
/// a few seconds longer.
const WRITE_BREATH: Duration = Duration::from_millis(2);
/// How long the loop may take to stop playback before the write begins.
const STOP_WAIT: Duration = Duration::from_secs(3);
/// The health gate: the loop turning this long after the panel lit.
const SETTLED: Duration = Duration::from_secs(60);
/// The gate's patience for Wi-Fi when the card names a network; past
/// it the firmware is confirmed without — a router that is off is not
/// the firmware's fault.
const WIFI_PATIENCE: Duration = Duration::from_secs(180);
/// A pending firmware that has not met the gate by then is rolled back.
/// A `gate-test` build never meets it and is rolled back soon: the
/// proof, on the bench, that the bootloader returns.
const GATE_DEADLINE: Duration = if cfg!(feature = "gate-test") {
    Duration::from_secs(30)
} else {
    Duration::from_secs(300)
};
/// The pause between the answer and the restart, so the answer leaves.
const RESTART_AFTER: Duration = Duration::from_millis(800);

/// What the page asks of the loop, and what the loop answers.
pub enum Ask {
    /// Stop playback and say so on the face; answer when quiet.
    Quiet(Sender<()>),
    /// The update is written; the player restarts into `slot`.
    Restarting(&'static str),
    /// The update failed; `why` is for the face and the log.
    Failed(String),
    /// Something finished without a restart; `what` is for the face.
    Done(String),
}

/// The slot the player runs from, and how the bootloader holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    /// The bootloader is trying this firmware; it must be confirmed.
    Pending,
    /// Confirmed, or never subject to confirmation (the factory slot,
    /// or a USB flash).
    Settled,
}

/// The bootloader's word for a slot's state.
fn state_name(state: sys::esp_ota_img_states_t) -> &'static str {
    match state {
        sys::esp_ota_img_states_t_ESP_OTA_IMG_NEW => "new",
        sys::esp_ota_img_states_t_ESP_OTA_IMG_PENDING_VERIFY => "pending",
        sys::esp_ota_img_states_t_ESP_OTA_IMG_VALID => "valid",
        sys::esp_ota_img_states_t_ESP_OTA_IMG_INVALID => "invalid",
        sys::esp_ota_img_states_t_ESP_OTA_IMG_ABORTED => "aborted",
        _ => "",
    }
}

/// The label of a partition, as text.
fn label(p: *const sys::esp_partition_t) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: `p` came from the partition table, which lives for ever;
    // the label is a NUL-terminated field of fixed size.
    unsafe {
        CStr::from_ptr((*p).label.as_ptr())
            .to_string_lossy()
            .into_owned()
    }
}

/// What the image in `p` declares about itself: its version and the
/// first four bytes of its ELF sha256 as hex; both empty for an empty
/// slot. The version is the build's `git describe`, so two builds of a
/// working tree share it; the sha tells them apart.
fn described(p: *const sys::esp_partition_t) -> (String, String) {
    // SAFETY: a plain-data struct the call fills in; `p` is a table entry.
    let mut desc: sys::esp_app_desc_t = unsafe { std::mem::zeroed() };
    let ret = unsafe { sys::esp_ota_get_partition_description(p, &mut desc) };
    if ret != sys::ESP_OK {
        return (String::new(), String::new());
    }
    // SAFETY: `version` is a NUL-terminated field of fixed size.
    let version = unsafe {
        CStr::from_ptr(desc.version.as_ptr())
            .to_string_lossy()
            .into_owned()
    };
    let id = desc.app_elf_sha256[..4]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    (version, id)
}

/// The three slots, factory first.
fn app_slots() -> Vec<*const sys::esp_partition_t> {
    let mut out = Vec::with_capacity(3);
    for sub in [
        sys::esp_partition_subtype_t_ESP_PARTITION_SUBTYPE_APP_FACTORY,
        sys::esp_partition_subtype_t_ESP_PARTITION_SUBTYPE_APP_OTA_0,
        sys::esp_partition_subtype_t_ESP_PARTITION_SUBTYPE_APP_OTA_1,
    ] {
        // SAFETY: a lookup in the partition table; a null when absent.
        let p = unsafe {
            sys::esp_partition_find_first(
                sys::esp_partition_type_t_ESP_PARTITION_TYPE_APP,
                sub,
                std::ptr::null(),
            )
        };
        if !p.is_null() {
            out.push(p);
        }
    }
    out
}

/// Where the player runs from and how the bootloader holds it. A slot
/// whose last write never finished (power cut, a reset) is left as
/// nothing: the bootloader never pointed at it, but its first sector
/// may read as an image to anyone who looks.
pub fn standing() -> (String, Standing) {
    // SAFETY: plain getters on the partition table and otadata.
    let running = unsafe { sys::esp_ota_get_running_partition() };
    let boot = unsafe { sys::esp_ota_get_boot_partition() };
    for p in app_slots() {
        // SAFETY: a table entry.
        let is_factory = unsafe { (*p).subtype }
            == sys::esp_partition_subtype_t_ESP_PARTITION_SUBTYPE_APP_FACTORY;
        if p == running || p == boot || is_factory {
            continue;
        }
        let mut state = sys::esp_ota_img_states_t_ESP_OTA_IMG_UNDEFINED;
        // SAFETY: as above; a slot otadata never recorded has no state.
        let got = unsafe { sys::esp_ota_get_state_partition(p, &mut state) };
        let recorded = got == sys::ESP_OK
            && state != sys::esp_ota_img_states_t_ESP_OTA_IMG_UNDEFINED;
        if !recorded && !described(p).0.is_empty() {
            info!("update: {} holds an unfinished write; blanked", label(p));
            blank(p);
        }
    }
    let mut state = sys::esp_ota_img_states_t_ESP_OTA_IMG_UNDEFINED;
    let got = unsafe { sys::esp_ota_get_state_partition(running, &mut state) };
    let standing = if got == sys::ESP_OK
        && state == sys::esp_ota_img_states_t_ESP_OTA_IMG_PENDING_VERIFY
    {
        Standing::Pending
    } else {
        Standing::Settled
    };
    (label(running), standing)
}

/// What the page and the status file say.
pub fn status(name: &str, last: &str) -> Status {
    // SAFETY: plain getters.
    let running = unsafe { sys::esp_ota_get_running_partition() };
    let boot = unsafe { sys::esp_ota_get_boot_partition() };
    let mut slots = Vec::with_capacity(3);
    let mut may_keep = false;
    for p in app_slots() {
        let mut state = sys::esp_ota_img_states_t_ESP_OTA_IMG_UNDEFINED;
        // SAFETY: as above; fails for the factory slot, which has no state.
        let got = unsafe { sys::esp_ota_get_state_partition(p, &mut state) };
        let state = if got == sys::ESP_OK {
            state_name(state)
        } else {
            ""
        };
        let is_running = p == running;
        // SAFETY: a table entry.
        let is_factory = unsafe { (*p).subtype }
            == sys::esp_partition_subtype_t_ESP_PARTITION_SUBTYPE_APP_FACTORY;
        if is_running && !is_factory && state != "pending" {
            may_keep = true;
        }
        let (version, id) = described(p);
        slots.push(Slot {
            label: label(p),
            version,
            id,
            running: is_running,
            boot: p == boot,
            state: state.to_string(),
        });
    }
    Status {
        name: name.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        build: env!("EVO_BUILD_ID").to_string(),
        slots,
        may_keep,
        last: last.to_string(),
    }
}

/// The health gate: confirms a pending firmware once the player has
/// shown itself healthy, or rolls it back when it has not in time.
pub struct Gate {
    started: Instant,
    lit_at: Option<Instant>,
    sink_opened: bool,
    done: bool,
}

impl Gate {
    /// The gate for this boot.
    pub fn new(standing: Standing) -> Self {
        if cfg!(feature = "gate-test") {
            warn!(
                "update: a gate-test build: it will not confirm itself and rolls back in {} s",
                GATE_DEADLINE.as_secs()
            );
        }
        Self {
            started: Instant::now(),
            lit_at: None,
            sink_opened: false,
            done: standing == Standing::Settled,
        }
    }

    /// True while the firmware is pending and the gate has not spoken.
    pub fn pending(&self) -> bool {
        !self.done
    }

    /// Each turn of the loop: what the player has reached. `wifi` is
    /// `None` when the card names no network, else whether it is up;
    /// `wants_sink` whether there is anything to play at all.
    pub fn observe(
        &mut self,
        lit: bool,
        sink_opened: bool,
        wants_sink: bool,
        wifi: Option<bool>,
    ) {
        if self.done {
            return;
        }
        if lit && self.lit_at.is_none() {
            self.lit_at = Some(Instant::now());
        }
        self.sink_opened |= sink_opened;
        let settled = !cfg!(feature = "gate-test")
            && self.lit_at.is_some_and(|t| t.elapsed() >= SETTLED);
        let sink_ok = !wants_sink || self.sink_opened;
        let wifi_ok = match wifi {
            None | Some(true) => true,
            Some(false) => self.started.elapsed() >= WIFI_PATIENCE,
        };
        if settled && sink_ok && wifi_ok {
            self.done = true;
            // SAFETY: a plain call on otadata; the result is logged.
            let ret = unsafe { sys::esp_ota_mark_app_valid_cancel_rollback() };
            if ret == sys::ESP_OK {
                info!(
                    "update: {} confirmed after {} s (lit, {}{}the loop turning)",
                    env!("EVO_BUILD_ID"),
                    self.started.elapsed().as_secs(),
                    if wants_sink { "the sink opened, " } else { "" },
                    match wifi {
                        Some(true) => "wifi up, ",
                        Some(false) => "wifi not up (patience spent), ",
                        None => "",
                    }
                );
            } else {
                warn!("update: could not confirm this firmware ({ret})");
            }
            return;
        }
        if self.started.elapsed() >= GATE_DEADLINE {
            self.done = true;
            warn!(
                "update: {} did not pass the health gate in {} s (lit {}, sink {}/{}, wifi {:?}); rolling back",
                env!("EVO_BUILD_ID"),
                GATE_DEADLINE.as_secs(),
                self.lit_at.is_some(),
                self.sink_opened,
                wants_sink,
                wifi
            );
            // SAFETY: marks otadata and resets; does not return on success.
            let ret =
                unsafe { sys::esp_ota_mark_app_invalid_rollback_and_reboot() };
            warn!("update: rollback refused ({ret}); staying as is");
        }
    }
}

/// Writes an image into a slot.
struct Writer {
    target: *const sys::esp_partition_t,
    handle: sys::esp_ota_handle_t,
    written: usize,
}

impl Writer {
    /// Begin on the update slot not running.
    fn begin() -> Result<Self, String> {
        // SAFETY: a lookup in the partition table.
        let target =
            unsafe { sys::esp_ota_get_next_update_partition(std::ptr::null()) };
        if target.is_null() {
            return Err("no slot to write".into());
        }
        let mut handle: sys::esp_ota_handle_t = 0;
        // SAFETY: `target` is a table entry. The slot is erased a sector
        // at a time as the image arrives: erasing it whole first held the
        // core for seconds and tripped the loop's watchdog.
        let ret = unsafe {
            sys::esp_ota_begin(
                target,
                sys::OTA_WITH_SEQUENTIAL_WRITES as usize,
                &mut handle,
            )
        };
        if ret != sys::ESP_OK {
            return Err(format!("begin on {}: {}", label(target), err(ret)));
        }
        Ok(Self {
            target,
            handle,
            written: 0,
        })
    }

    fn write(&mut self, bytes: &[u8]) -> Result<(), String> {
        if self.written == 0 && bytes.first() != Some(&IMAGE_MAGIC) {
            return Err("not a firmware image".into());
        }
        // SAFETY: `bytes` lives for the call; the handle is open.
        let ret = unsafe {
            sys::esp_ota_write(self.handle, bytes.as_ptr().cast(), bytes.len())
        };
        if ret != sys::ESP_OK {
            return Err(format!("write at {}: {}", self.written, err(ret)));
        }
        self.written += bytes.len();
        Ok(())
    }

    /// Validate what was written and tell the bootloader to take it
    /// next. The slot's label on success; the slot blanked on failure.
    fn finish(self) -> Result<&'static str, String> {
        let Self {
            target,
            handle,
            written,
        } = self;
        // SAFETY: the handle is open; `end` validates the image.
        let ret = unsafe { sys::esp_ota_end(handle) };
        if ret != sys::ESP_OK {
            blank(target);
            return Err(format!(
                "the image is not valid after {written} bytes: {}",
                err(ret)
            ));
        }
        // SAFETY: `target` is a table entry holding a valid image.
        let ret = unsafe { sys::esp_ota_set_boot_partition(target) };
        if ret != sys::ESP_OK {
            return Err(format!("could not select the slot: {}", err(ret)));
        }
        Ok(label_static(target))
    }

    /// Give up; the slot is left blank, not half an image.
    fn abort(self) {
        // SAFETY: the handle is open; abort frees it.
        unsafe { sys::esp_ota_abort(self.handle) };
        blank(self.target);
    }
}

/// Erase a slot's first sector: whatever it held no longer reads as an
/// image, to the bootloader or the page.
fn blank(target: *const sys::esp_partition_t) {
    // SAFETY: `target` is a table entry; a sector at its start.
    let ret = unsafe { sys::esp_partition_erase_range(target, 0, 4096) };
    if ret != sys::ESP_OK {
        warn!("update: could not blank {}: {}", label(target), err(ret));
    }
}

/// A slot's label as a static string, for the log after the writer is
/// gone.
fn label_static(p: *const sys::esp_partition_t) -> &'static str {
    match label(p).as_str() {
        "factory" => "factory",
        "ota_0" => "ota_0",
        "ota_1" => "ota_1",
        _ => "a slot",
    }
}

fn err(code: sys::esp_err_t) -> String {
    sys::EspError::from(code)
        .map_or_else(|| code.to_string(), |e| e.to_string())
}

/// What the page needs from the firmware.
struct Shared {
    name: String,
    asks: Sender<Ask>,
    /// The last update's outcome, for the page.
    last: Mutex<String>,
    /// One update at a time.
    busy: Mutex<()>,
    /// The page's lock, when the card sets `web_user` and `web_password`.
    lock: Option<Lock>,
}

impl Shared {
    /// Let the request through, or answer it with the challenge and
    /// return `Err` so the handler stops there.
    fn admit<'a, 'c>(
        &self,
        req: Request<&'a mut EspHttpConnection<'c>>,
        method: &str,
    ) -> Result<Request<&'a mut EspHttpConnection<'c>>, anyhow::Result<()>>
    {
        let Some(lock) = &self.lock else {
            return Ok(req);
        };
        let now = uptime_secs();
        match lock.check(method, req.uri(), req.header("Authorization"), now) {
            Ok(()) => Ok(req),
            Err(refusal) => {
                let challenge = lock.challenge(now, refusal);
                let send = (|| {
                    let mut resp = req.into_response(
                        401,
                        Some("Unauthorized"),
                        &[
                            ("WWW-Authenticate", challenge.as_str()),
                            ("Content-Type", "text/plain; charset=utf-8"),
                        ],
                    )?;
                    resp.write_all(b"This player's page asks for its name and password (web_user and web_password on the card).\n")?;
                    Ok(())
                })();
                Err(send)
            }
        }
    }
}

/// Seconds since boot, for the nonces.
fn uptime_secs() -> u64 {
    // SAFETY: a plain reading of the system timer.
    (unsafe { sys::esp_timer_get_time() } / 1_000_000) as u64
}

/// The page, on port 80, for the life of the firmware.
pub struct Page {
    _server: EspHttpServer<'static>,
}

impl Page {
    /// Start the page; `asks` reach the loop, which stops playback
    /// before a write and shows what is happening. With `credentials`
    /// (a name and a password from the card) every request is asked
    /// for them; without, the page is open to the network.
    pub fn start(
        name: &str,
        asks: Sender<Ask>,
        credentials: Option<(&str, &str)>,
    ) -> Result<Self, String> {
        let lock = credentials.map(|(user, password)| {
            let mut secret = [0u8; 16];
            // SAFETY: fills the buffer from the hardware RNG.
            unsafe {
                sys::esp_fill_random(secret.as_mut_ptr().cast(), secret.len())
            };
            Lock::new(user, password, secret)
        });
        let shared = Arc::new(Shared {
            name: name.to_string(),
            asks,
            last: Mutex::new(String::new()),
            busy: Mutex::new(()),
            lock,
        });
        let mut server = EspHttpServer::new(&Configuration {
            http_port: PORT,
            stack_size: SERVER_STACK,
            max_open_sockets: SERVER_SOCKETS,
            max_uri_handlers: 8,
            ..Default::default()
        })
        .map_err(|e| format!("http server: {e}"))?;
        lower_server_priority();

        let s = Arc::clone(&shared);
        server
            .fn_handler::<anyhow::Error, _>("/", Method::Get, move |req| {
                let req = match s.admit(req, "GET") {
                    Ok(r) => r,
                    Err(sent) => return sent,
                };
                let last =
                    s.last.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let page = status(&s.name, &last).page();
                let mut resp = req.into_response(
                    200,
                    Some("OK"),
                    &[("Content-Type", "text/html; charset=utf-8")],
                )?;
                resp.write_all(page.as_bytes())?;
                Ok(())
            })
            .map_err(|e| format!("route /: {e}"))?;

        let s = Arc::clone(&shared);
        server
            .fn_handler::<anyhow::Error, _>(
                "/status",
                Method::Get,
                move |req| {
                    let req = match s.admit(req, "GET") {
                        Ok(r) => r,
                        Err(sent) => return sent,
                    };
                    let last = s
                        .last
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
                    let st = status(&s.name, &last);
                    let mut text =
                        format!("{} {} {}\n", st.name, st.version, st.build);
                    for l in st.lines() {
                        let _ = writeln!(text, "{l}");
                    }
                    if !st.last.is_empty() {
                        let _ = writeln!(text, "last: {}", st.last);
                    }
                    let mut resp = req.into_response(
                        200,
                        Some("OK"),
                        &[("Content-Type", "text/plain; charset=utf-8")],
                    )?;
                    resp.write_all(text.as_bytes())?;
                    Ok(())
                },
            )
            .map_err(|e| format!("route /status: {e}"))?;

        let s = Arc::clone(&shared);
        server
            .fn_handler::<anyhow::Error, _>("/update", Method::Post, move |req| {
                let mut req = match s.admit(req, "POST") {
                    Ok(r) => r,
                    Err(sent) => return sent,
                };
                let browser = wants_html(&req);
                let Ok(_guard) = s.busy.try_lock() else {
                    return reply(req, &s.name, browser, false, "An update is already in progress", "Wait for it to finish, then try again.", None);
                };
                let framing = match Framing::from_content_type(req.header("Content-Type")) {
                    Ok(f) => f,
                    Err(why) => return reply(req, &s.name, browser, false, "Not a firmware upload", &why, None),
                };
                // Playback stops before the flash is touched.
                let (tx, rx) = channel();
                let _ = s.asks.send(Ask::Quiet(tx));
                let _ = rx.recv_timeout(STOP_WAIT);
                let began = Instant::now();
                let outcome = receive(&mut req, framing);
                let (line, headline, detail) = match &outcome {
                    Ok((slot, bytes)) => {
                        let line = format!(
                            "written {bytes} bytes to {slot} in {} s; restarting into it",
                            began.elapsed().as_secs()
                        );
                        info!("update: {line}");
                        let _ = s.asks.send(Ask::Restarting(slot));
                        (
                            line,
                            format!("Written to {slot}; the player is restarting"),
                            format!(
                                "{} KB arrived in {} s. The new firmware starts on trial and confirms itself \
                                 after a healthy minute; if it does not, the player comes back to the firmware it had.",
                                bytes / 1024,
                                began.elapsed().as_secs()
                            ),
                        )
                    }
                    Err(why) => {
                        warn!("update: {why}");
                        let _ = s.asks.send(Ask::Failed(why.clone()));
                        (
                            format!("failed: {why}"),
                            "The update was not applied".to_string(),
                            format!("{why}. Nothing changed; the player plays on as it was."),
                        )
                    }
                };
                *s.last.lock().unwrap_or_else(|e| e.into_inner()) = line;
                let ok = outcome.is_ok();
                reply(req, &s.name, browser, ok, &headline, &detail, ok.then_some(75))?;
                if ok {
                    restart_soon();
                }
                Ok(())
            })
            .map_err(|e| format!("route /update: {e}"))?;

        let s = Arc::clone(&shared);
        server
            .fn_handler::<anyhow::Error, _>("/factory", Method::Post, move |req| {
                let req = match s.admit(req, "POST") {
                    Ok(r) => r,
                    Err(sent) => return sent,
                };
                let browser = wants_html(&req);
                let Ok(_guard) = s.busy.try_lock() else {
                    return reply(req, &s.name, browser, false, "An update is already in progress", "Wait for it to finish, then try again.", None);
                };
                let st = status(&s.name, "");
                if !st.may_keep {
                    return reply(
                        req,
                        &s.name,
                        browser,
                        false,
                        "Nothing to keep yet",
                        "The running firmware is the factory copy already, or has not confirmed itself yet.",
                        None,
                    );
                }
                let (tx, rx) = channel();
                let _ = s.asks.send(Ask::Quiet(tx));
                let _ = rx.recv_timeout(STOP_WAIT);
                let began = Instant::now();
                let (line, ok, headline, detail) = match keep_as_factory() {
                    Ok(bytes) => {
                        let line = format!(
                            "kept the running firmware as the factory copy ({bytes} bytes in {} s)",
                            began.elapsed().as_secs()
                        );
                        info!("update: {line}");
                        let _ = s.asks.send(Ask::Done("Kept as the factory copy".into()));
                        (
                            line,
                            true,
                            "Kept as the factory copy".to_string(),
                            format!(
                                "{} KB copied in {} s. This is now the firmware the player falls back to.",
                                bytes / 1024,
                                began.elapsed().as_secs()
                            ),
                        )
                    }
                    Err(why) => {
                        warn!("update: factory copy: {why}");
                        let _ = s.asks.send(Ask::Failed(why.clone()));
                        (
                            format!("factory copy failed: {why}"),
                            false,
                            "The factory copy was not made".to_string(),
                            format!("{why}. The factory copy is as it was."),
                        )
                    }
                };
                *s.last.lock().unwrap_or_else(|e| e.into_inner()) = line;
                reply(req, &s.name, browser, ok, &headline, &detail, None)
            })
            .map_err(|e| format!("route /factory: {e}"))?;

        info!(
            "update: the page is on port {PORT}{}",
            if shared.lock.is_some() {
                ", locked with the card's name and password"
            } else {
                ", open to the network"
            }
        );
        Ok(Self { _server: server })
    }
}

/// Whether the request came from a browser: a form, or a client that
/// asks for HTML. A script's raw POST gets text.
fn wants_html(req: &Request<&mut EspHttpConnection<'_>>) -> bool {
    req.header("Content-Type").is_some_and(|ct| {
        ct.to_ascii_lowercase().starts_with("multipart/form-data")
    }) || req
        .header("Accept")
        .is_some_and(|a| a.contains("text/html"))
}

/// The answer: the outcome page for a browser, a line for a script.
fn reply(
    req: Request<&mut EspHttpConnection<'_>>,
    name: &str,
    browser: bool,
    ok: bool,
    headline: &str,
    detail: &str,
    back_in: Option<u32>,
) -> anyhow::Result<()> {
    let code = if ok { 200 } else { 400 };
    if browser {
        let page = outcome_page(name, headline, detail, ok, back_in);
        let mut resp = req.into_response(
            code,
            None,
            &[("Content-Type", "text/html; charset=utf-8")],
        )?;
        resp.write_all(page.as_bytes())?;
    } else {
        let mut resp = req.into_response(
            code,
            None,
            &[("Content-Type", "text/plain; charset=utf-8")],
        )?;
        resp.write_all(format!("{headline}: {detail}\n").as_bytes())?;
    }
    Ok(())
}

/// Read the request's image into the next update slot. The slot's label
/// and the bytes written.
fn receive<R: Read>(
    req: &mut R,
    framing: Framing,
) -> Result<(&'static str, usize), String>
where
    R::Error: std::fmt::Debug,
{
    let mut writer = Writer::begin()?;
    let mut buf = vec![0u8; CHUNK];
    let mut form = match &framing {
        Framing::Form { boundary } => Some(Form::new(boundary)),
        Framing::Raw => None,
    };
    loop {
        let n = match req.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                writer.abort();
                return Err(format!("the upload broke off: {e:?}"));
            }
        };
        let fed = match form.as_mut() {
            Some(f) => {
                let mut failed = None;
                let r = f.feed(&buf[..n], &mut |bytes: &[u8]| {
                    if let Err(e) = writer.write(bytes) {
                        failed = Some(e);
                        return Err(std::io::Error::other("flash"));
                    }
                    Ok(())
                });
                match (r, failed) {
                    (_, Some(why)) => Err(why),
                    (Err(e), None) => Err(format!("the form: {e}")),
                    (Ok(()), None) => Ok(()),
                }
            }
            None => writer.write(&buf[..n]),
        };
        if let Err(why) = fed {
            writer.abort();
            return Err(why);
        }
        std::thread::sleep(WRITE_BREATH);
        if form.as_ref().is_some_and(Form::done) {
            break;
        }
    }
    if let Some(f) = &form {
        if !f.done() {
            writer.abort();
            return Err("the form ended before the file did".into());
        }
    }
    if writer.written == 0 {
        writer.abort();
        return Err("no image in the request".into());
    }
    let written = writer.written;
    let slot = writer.finish()?;
    Ok((slot, written))
}

/// Copy the running image into the factory slot. Bytes copied.
///
/// `esp_ota_begin` writes update slots only (the factory slot is not
/// one), so this goes through the partition API: erase a sector ahead
/// of each write, then `esp_image_verify` on the copy before it counts.
/// Paced like an update. A copy that fails verification is blanked.
fn keep_as_factory() -> Result<usize, String> {
    // SAFETY: plain getters; the running partition is a table entry.
    let running = unsafe { sys::esp_ota_get_running_partition() };
    let factory = unsafe {
        sys::esp_partition_find_first(
            sys::esp_partition_type_t_ESP_PARTITION_TYPE_APP,
            sys::esp_partition_subtype_t_ESP_PARTITION_SUBTYPE_APP_FACTORY,
            std::ptr::null(),
        )
    };
    if factory.is_null() {
        return Err("no factory slot".into());
    }
    if factory == running {
        return Err("the running firmware is the factory copy".into());
    }
    let slot = unsafe { (*running).size } as usize;
    let room = unsafe { (*factory).size } as usize;
    let read_at = |at: usize, buf: &mut [u8]| -> std::io::Result<()> {
        if at + buf.len() > slot {
            return Err(std::io::Error::other("past the slot"));
        }
        // SAFETY: `buf` holds its length; `at + len` is within the slot.
        let ret = unsafe {
            sys::esp_partition_read(
                running,
                at,
                buf.as_mut_ptr().cast(),
                buf.len(),
            )
        };
        if ret == sys::ESP_OK {
            Ok(())
        } else {
            Err(std::io::Error::other(err(ret)))
        }
    };
    let size =
        image_length(read_at).map_err(|e| format!("the running image: {e}"))?;
    if size > room {
        return Err("the running image is longer than the factory slot".into());
    }
    let mut buf = vec![0u8; CHUNK];
    let mut at = 0usize;
    while at < size {
        let n = CHUNK.min(size - at);
        if let Err(e) = read_at(at, &mut buf[..n]) {
            blank(factory);
            return Err(format!("read at {at}: {e}"));
        }
        // SAFETY: `at` is sector-aligned (CHUNK is a sector); the erase
        // and the write stay within the factory slot.
        let erased =
            unsafe { sys::esp_partition_erase_range(factory, at, CHUNK) };
        if erased != sys::ESP_OK {
            blank(factory);
            return Err(format!("erase at {at}: {}", err(erased)));
        }
        let written = unsafe {
            sys::esp_partition_write(factory, at, buf.as_ptr().cast(), n)
        };
        if written != sys::ESP_OK {
            blank(factory);
            return Err(format!("write at {at}: {}", err(written)));
        }
        std::thread::sleep(WRITE_BREATH);
        at += n;
    }
    // The copy must verify as an image before it is the fallback.
    let pos = sys::esp_partition_pos_t {
        offset: unsafe { (*factory).address },
        size: unsafe { (*factory).size },
    };
    // SAFETY: a plain-data struct the verifier fills in.
    let mut meta: sys::esp_image_metadata_t = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        sys::esp_image_verify(
            sys::esp_image_load_mode_t_ESP_IMAGE_VERIFY_SILENT,
            &pos,
            &mut meta,
        )
    };
    if ok != sys::ESP_OK {
        blank(factory);
        return Err(format!("the copy does not verify: {}", err(ok)));
    }
    Ok(at)
}

/// The server's task runs at ESP-IDF's default of 5, level with the
/// loop; it belongs one below, with the other background threads.
fn lower_server_priority() {
    // SAFETY: a lookup by the name the component gives its task; a null
    // when it is not found, which `vTaskPrioritySet` must not see.
    let task = unsafe { sys::xTaskGetHandle(c"httpd".as_ptr()) };
    if task.is_null() {
        warn!("update: the server's task was not found; it stays at 5");
        return;
    }
    // SAFETY: a plain scheduler call on a live task.
    unsafe {
        sys::vTaskPrioritySet(
            task,
            u32::from(crate::threads::BACKGROUND_PRIORITY),
        );
    }
}

/// Restart once the answer has left.
fn restart_soon() {
    let _ = std::thread::Builder::new()
        .name("restart".into())
        .stack_size(2048)
        .spawn(|| {
            std::thread::sleep(RESTART_AFTER);
            // SAFETY: a clean restart; does not return.
            unsafe { sys::esp_restart() };
        });
}

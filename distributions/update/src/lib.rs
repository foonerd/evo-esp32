// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! A firmware update, before the flash.
//!
//! The player takes a new firmware three ways — a browser's form, a
//! script's `POST`, a file on the card — and writes it into the slot it
//! is not running from; the bootloader boots the new one once, and the
//! player confirms it after a healthy start or the bootloader goes back.
//! This crate holds the parts of that which are plain logic and must be
//! right on a workstation before they are right on a board: how a
//! request's body is told apart (a bare image, or a form that wraps one
//! in `multipart/form-data`) and split as it streams, 4 KB at a time,
//! into the bytes that go to the flash; the page the player serves; and
//! the words for what each slot holds. The flash, the slots and the
//! bootloader are the platform's.

#![forbid(unsafe_code)]

use std::io;

pub mod lock;
pub mod manifest;

/// The first bytes of every ESP32 application image.
pub const IMAGE_MAGIC: u8 = 0xE9;

/// The image header's length; the segment header's.
const HEADER: usize = 24;
const SEGMENT_HEADER: usize = 8;

/// The length of an application image read through `read_at`, which
/// fills a buffer from an offset into it: the header, the segments it
/// counts, the checksum padded to sixteen bytes, and the SHA-256 the
/// header says is appended. What a slot holds beyond that is erased
/// flash, not image.
pub fn image_length<F>(mut read_at: F) -> io::Result<usize>
where
    F: FnMut(usize, &mut [u8]) -> io::Result<()>,
{
    let mut header = [0u8; HEADER];
    read_at(0, &mut header)?;
    if header[0] != IMAGE_MAGIC {
        return Err(bad("not an application image"));
    }
    let segments = usize::from(header[1]);
    let hash_appended = header[23] == 1;
    let mut at = HEADER;
    for _ in 0..segments {
        let mut seg = [0u8; SEGMENT_HEADER];
        read_at(at, &mut seg).map_err(|e| {
            if e.kind() == io::ErrorKind::UnexpectedEof {
                bad("the file ends before the image does: not a whole copy")
            } else {
                e
            }
        })?;
        let len = u32::from_le_bytes([seg[4], seg[5], seg[6], seg[7]]) as usize;
        at = at
            .checked_add(SEGMENT_HEADER + len)
            .ok_or_else(|| bad("a segment runs past the flash"))?;
    }
    // The checksum byte closes a sixteen-byte block.
    at = (at + 1).div_ceil(16) * 16;
    if hash_appended {
        at += 32;
    }
    Ok(at)
}

/// Where the application descriptor sits in an image: after the image
/// header and the first segment's header.
const DESCRIPTOR_AT: usize = HEADER + SEGMENT_HEADER;
/// The descriptor's magic word, `ESP_APP_DESC_MAGIC_WORD`.
const DESCRIPTOR_MAGIC: [u8; 4] = 0xABCD_5432u32.to_le_bytes();
/// The ELF's SHA-256 within the descriptor: after the magic, the secure
/// version, two reserved words, and the version, project, time, date
/// and IDF strings.
const ELF_SHA_AT: usize = 4 + 4 + 8 + 32 + 32 + 16 + 16 + 32;

/// The SHA-256 of the ELF an image was made from, read through
/// `read_at` as [`image_length`] reads. esptool stamps it into the
/// descriptor; two builds of one version differ here when nothing
/// else tells.
pub fn image_identity<F>(mut read_at: F) -> io::Result<[u8; 32]>
where
    F: FnMut(usize, &mut [u8]) -> io::Result<()>,
{
    let mut first = [0u8; 1];
    read_at(0, &mut first)?;
    if first[0] != IMAGE_MAGIC {
        return Err(bad("not an application image"));
    }
    let mut magic = [0u8; 4];
    read_at(DESCRIPTOR_AT, &mut magic)?;
    if magic != DESCRIPTOR_MAGIC {
        return Err(bad("no application descriptor"));
    }
    let mut sha = [0u8; 32];
    read_at(DESCRIPTOR_AT + ELF_SHA_AT, &mut sha)?;
    Ok(sha)
}

/// The short form of an image's identity for the page and the status
/// lines: the first four bytes of its ELF SHA-256 as hex.
pub fn short_id(sha256: &[u8; 32]) -> String {
    sha256[..4].iter().map(|b| format!("{b:02x}")).collect()
}

/// A signed image carries a Secure Boot v2 signature block in the
/// 4 KB sector after the image, padded to a sector.
pub const SIGNATURE_SECTOR: usize = 4096;
/// The signature block's first byte.
pub const SIGNATURE_MAGIC: u8 = 0xE7;

/// Where the signature block of an image of `len` bytes sits.
pub fn signature_block_at(len: usize) -> usize {
    len.div_ceil(SIGNATURE_SECTOR) * SIGNATURE_SECTOR
}

/// The length of an image of `len` bytes with its signature block.
pub fn signed_length(len: usize) -> usize {
    signature_block_at(len) + SIGNATURE_SECTOR
}

/// Whether a file of `size` bytes is a whole copy of an image whose
/// header declares `len` bytes: the image alone, or the image with a
/// signature block.
pub fn is_whole(size: u64, len: usize) -> bool {
    size == len as u64 || size == signed_length(len) as u64
}

/// How many bytes of an upload are held back and judged before the
/// flash is touched: the image header, the first segment's header and
/// the descriptor's magic word. ESP-IDF's `esp_ota_begin` erases the
/// previous firmware's boot record the moment it is called, so it must
/// not be called for a file that is not an application image.
pub const HEAD_LEN: usize = DESCRIPTOR_AT + 4;

/// Whether the first [`HEAD_LEN`] bytes of an upload are an application
/// image's: the image magic, then the descriptor's magic word where the
/// descriptor sits. A whole-flash image (`.factory.bin`) starts with the
/// bootloader, which has the image magic but not the descriptor; it is
/// named, as the likeliest mistake.
pub fn judge_head(head: &[u8]) -> Result<(), String> {
    if head.len() < HEAD_LEN {
        return Err("not a firmware image".into());
    }
    if head.iter().all(|&b| b == 0xFF) {
        return Err("the file starts with blank flash: the .factory.bin is for USB; the page takes the .ota.bin".into());
    }
    if head[0] != IMAGE_MAGIC {
        return Err("not a firmware image".into());
    }
    if head[DESCRIPTOR_AT..DESCRIPTOR_AT + 4] != DESCRIPTOR_MAGIC {
        return Err("not an application image (the .factory.bin is for USB; the page takes the .ota.bin)".into());
    }
    Ok(())
}

/// The suffix of an image file on the card, as `scripts/image.sh`
/// names it; matched without regard to case, as FAT keeps names.
pub const CARD_SUFFIX: &str = ".ota.bin";

/// Whether a file name on the card's root is an image to apply.
pub fn is_card_image(name: &str) -> bool {
    name.len() > CARD_SUFFIX.len()
        && name
            .get(name.len() - CARD_SUFFIX.len()..)
            .is_some_and(|tail| tail.eq_ignore_ascii_case(CARD_SUFFIX))
}

/// The name an applied image file is given, so the next boot does not
/// apply it again and the owner sees it went: `x.ota.bin` →
/// `x.ota.applied`.
pub fn applied_name(name: &str) -> String {
    renamed(name, "applied")
}

/// The name a refused image file is given: `x.ota.bin` →
/// `x.ota.refused`; the status file says why.
pub fn refused_name(name: &str) -> String {
    renamed(name, "refused")
}

fn renamed(name: &str, outcome: &str) -> String {
    let stem = if is_card_image(name) {
        &name[..name.len() - 4]
    } else {
        name
    };
    format!("{stem}.{outcome}")
}

/// How a request carries the image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Framing {
    /// The body is the image.
    Raw,
    /// The body is a form; the image is the first part that is a file.
    Form {
        /// The boundary the form's parts are separated by, without the
        /// leading dashes.
        boundary: String,
    },
}

impl Framing {
    /// The framing a `Content-Type` header names. A form without a
    /// boundary is refused; anything that is not a form is the image.
    pub fn from_content_type(
        content_type: Option<&str>,
    ) -> Result<Self, String> {
        let Some(ct) = content_type else {
            return Ok(Self::Raw);
        };
        let mut fields = ct.split(';').map(str::trim);
        let kind = fields.next().unwrap_or("").to_ascii_lowercase();
        if kind != "multipart/form-data" {
            return Ok(Self::Raw);
        }
        for f in fields {
            if let Some(b) = f.strip_prefix("boundary=") {
                let b = b.trim_matches('"');
                if !b.is_empty() {
                    return Ok(Self::Form {
                        boundary: b.to_string(),
                    });
                }
            }
        }
        Err("a form with no boundary".into())
    }
}

/// Where a form's parsing stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Before the first boundary.
    Preamble,
    /// In a part's headers.
    Headers,
    /// In the file's bytes.
    Body,
    /// In a part that is not the file.
    Skip,
    /// The file's bytes are out; the delimiter's last two bytes are
    /// awaited.
    Closing,
    /// The file's closing boundary was seen.
    Done,
}

/// A form's body, split as it streams: the first part whose headers
/// name a file is the image, the rest is skipped.
#[derive(Debug)]
pub struct Form {
    delimiter: Vec<u8>,
    stage: Stage,
    pending: Vec<u8>,
    /// Bytes of the file passed on so far.
    pub passed: usize,
}

/// The most a part's headers may be before the form is refused.
const HEADERS_LIMIT: usize = 4096;

impl Form {
    /// A splitter for a form whose parts `boundary` separates.
    pub fn new(boundary: &str) -> Self {
        let mut delimiter = b"\r\n--".to_vec();
        delimiter.extend_from_slice(boundary.as_bytes());
        Self {
            delimiter,
            stage: Stage::Preamble,
            pending: Vec::new(),
            passed: 0,
        }
    }

    /// True once the file's closing boundary has been seen.
    pub fn done(&self) -> bool {
        self.stage == Stage::Done
    }

    /// Take `chunk` as it arrived; the file's bytes in it, if any, go
    /// to `out`, in order, each exactly once.
    pub fn feed<F>(&mut self, chunk: &[u8], out: &mut F) -> io::Result<()>
    where
        F: FnMut(&[u8]) -> io::Result<()>,
    {
        self.pending.extend_from_slice(chunk);
        loop {
            match self.stage {
                Stage::Preamble => {
                    // The first boundary has no CRLF before it; look for
                    // the delimiter without its first two bytes.
                    let first = &self.delimiter[2..];
                    let Some(at) = find(&self.pending, first) else {
                        let keep = first.len().saturating_sub(1);
                        let cut = self.pending.len().saturating_sub(keep);
                        self.pending.drain(..cut);
                        return Ok(());
                    };
                    self.pending.drain(..at + first.len());
                    self.stage = Stage::Headers;
                }
                Stage::Headers => {
                    // After a boundary: CRLF, then headers, then an
                    // empty line. "--" right after it is the form's end.
                    if self.pending.starts_with(b"--") {
                        return Err(bad("the form has no file"));
                    }
                    let Some(end) = find(&self.pending, b"\r\n\r\n") else {
                        if self.pending.len() > HEADERS_LIMIT {
                            return Err(bad("a part's headers run too long"));
                        }
                        return Ok(());
                    };
                    let head = String::from_utf8_lossy(&self.pending[..end])
                        .to_ascii_lowercase();
                    let is_file = head.contains("filename=");
                    self.pending.drain(..end + 4);
                    self.stage =
                        if is_file { Stage::Body } else { Stage::Skip };
                }
                Stage::Body | Stage::Skip => {
                    let in_file = self.stage == Stage::Body;
                    if let Some(at) = find(&self.pending, &self.delimiter) {
                        if in_file {
                            out(&self.pending[..at])?;
                            self.passed += at;
                        }
                        let after = at + self.delimiter.len();
                        if self.pending.len() < after + 2 {
                            // The two bytes after the delimiter say what
                            // follows; wait for them.
                            if in_file {
                                self.pending.drain(..at);
                                // Keep the delimiter unpassed: the file
                                // bytes before it are already out.
                                self.stage = Stage::Closing;
                            }
                            return Ok(());
                        }
                        let tail = &self.pending[after..after + 2];
                        if in_file {
                            self.stage = Stage::Done;
                            self.pending.clear();
                            return Ok(());
                        }
                        if tail == b"--" {
                            return Err(bad("the form has no file"));
                        }
                        self.pending.drain(..after + 2);
                        self.stage = Stage::Headers;
                        continue;
                    }
                    // No delimiter yet: everything but a delimiter's
                    // worth of tail is certainly content.
                    let keep = self.delimiter.len() - 1;
                    let cut = self.pending.len().saturating_sub(keep);
                    if in_file && cut > 0 {
                        out(&self.pending[..cut])?;
                        self.passed += cut;
                    }
                    self.pending.drain(..cut);
                    return Ok(());
                }
                Stage::Closing => {
                    // The delimiter is at the front; the file is out.
                    let after = self.delimiter.len();
                    if self.pending.len() < after + 2 {
                        return Ok(());
                    }
                    self.stage = Stage::Done;
                    self.pending.clear();
                    return Ok(());
                }
                Stage::Done => return Ok(()),
            }
        }
    }
}

fn bad(why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, why.to_string())
}

/// The first position of `needle` in `hay`.
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// What one slot of the flash holds, for the page and the status line.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Slot {
    /// The partition's label: `factory`, `ota_0`, `ota_1`.
    pub label: String,
    /// The version the image in it declares; empty when the slot is empty.
    pub version: String,
    /// The image's identity: the first bytes of its ELF sha256, as hex.
    /// Two builds of one version differ here when nothing else tells.
    pub id: String,
    /// The player runs from this slot.
    pub running: bool,
    /// The bootloader will take this slot next.
    pub boot: bool,
    /// The slot's state as the bootloader keeps it.
    pub state: String,
}

/// What the page says about the player.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Status {
    /// The player's name on the network.
    pub name: String,
    /// The running firmware's version.
    pub version: String,
    /// The running firmware's build id (`build:<git describe>`).
    pub build: String,
    /// Every slot, factory first.
    pub slots: Vec<Slot>,
    /// Whether the running image may be kept as the factory copy: it
    /// runs from an update slot and the bootloader holds it valid.
    pub may_keep: bool,
    /// The last update's outcome, for the page; empty when none.
    pub last: String,
    /// Where the player stands with releases, as [`manifest::Standing::line`]
    /// says it; empty when the player does not look.
    pub release: String,
    /// The version of a release that waits to be installed, when one does.
    pub may_pull: Option<String>,
}

impl Status {
    /// One line per slot, for the log and the status file.
    pub fn lines(&self) -> Vec<String> {
        self.slots
            .iter()
            .map(|s| {
                format!(
                    "{}: {}{}{}{}{}",
                    s.label,
                    if s.version.is_empty() {
                        "empty"
                    } else {
                        &s.version
                    },
                    if s.id.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", s.id)
                    },
                    if s.running { ", running" } else { "" },
                    if s.boot { ", next boot" } else { "" },
                    if s.state.is_empty() {
                        String::new()
                    } else {
                        format!(", {}", s.state)
                    }
                )
            })
            .collect()
    }

    /// The page: what runs, what the slots hold, a form to update, and
    /// a button to keep the running firmware as the factory copy. Drawn
    /// in the face's `evo-default` colours, so the page and the panel
    /// read as one player; one stylesheet, no script, so it works from
    /// any phone or laptop and weighs a few kilobytes.
    pub fn page(&self) -> String {
        let mut rows = String::new();
        for s in &self.slots {
            let (badge, badge_class) = if s.running && s.boot {
                ("running", "ok")
            } else if s.running {
                ("running, not next", "warn")
            } else if s.boot {
                ("next boot", "warn")
            } else {
                ("", "")
            };
            let state = match s.state.as_str() {
                "" => String::new(),
                "valid" => "<span class=\"tag ok\">confirmed</span>".into(),
                "pending" => "<span class=\"tag warn\">on trial</span>".into(),
                other => {
                    format!("<span class=\"tag fail\">{}</span>", escape(other))
                }
            };
            let what = if s.label == "factory" {
                "The copy the player falls back to"
            } else if s.running {
                "What is playing now"
            } else if s.version.is_empty() {
                "Free for the next update"
            } else {
                "The previous firmware, kept"
            };
            rows.push_str(&format!(
                "<div class=\"slot{}\"><div class=\"slot-head\"><span class=\"label\">{}</span>{}{}</div>\
                 <div class=\"version\">{}</div><div class=\"what\">{}</div></div>",
                if s.running { " current" } else { "" },
                escape(&s.label),
                if badge.is_empty() {
                    String::new()
                } else {
                    format!("<span class=\"tag {badge_class}\">{badge}</span>")
                },
                state,
                if s.version.is_empty() {
                    "<span class=\"muted\">empty</span>".to_string()
                } else if s.id.is_empty() {
                    escape(&s.version)
                } else {
                    format!(
                        "{} <span class=\"muted\">{}</span>",
                        escape(&s.version),
                        escape(&s.id)
                    )
                },
                what,
            ));
        }
        let keep = if self.may_keep {
            "<form method=\"post\" action=\"/factory\" class=\"row\">\
             <button class=\"secondary\" type=\"submit\">Keep this firmware as the factory copy</button></form>\
             <p class=\"note\">The factory copy is what the player returns to when both update slots fail. \
             Keeping a firmware there takes a few seconds and does not restart the player.</p>"
                .to_string()
        } else {
            "<p class=\"note\">The firmware that is running is the factory copy, or has not confirmed \
             itself yet. Once an update has run and proved itself, it can be kept here.</p>"
                .to_string()
        };
        let back = if self
            .slots
            .iter()
            .any(|s| s.running && s.label != "factory")
        {
            "<form method=\"post\" action=\"/boot-factory\" class=\"row\">\
             <button class=\"secondary\" type=\"submit\">Restart into the factory copy</button></form>\
             <p class=\"note\">For a firmware that runs but is not what you want. The update slots keep \
             their firmware; the next update goes on as before. On the player itself: hold a finger on the \
             panel for five seconds from the moment it lights after power-on.</p>"
                .to_string()
        } else {
            String::new()
        };
        let last = if self.last.is_empty() {
            String::new()
        } else {
            let class = if self.last.starts_with("failed") {
                "fail"
            } else {
                "ok"
            };
            format!(
                "<div class=\"last {class}\"><span class=\"muted\">Last update</span> {}</div>",
                escape(&self.last)
            )
        };
        let release = if self.release.is_empty() {
            String::new()
        } else {
            let install = match &self.may_pull {
                Some(v) => format!(
                    "<form method=\"post\" action=\"/pull\" class=\"row\">\
                     <button class=\"primary\" type=\"submit\">Install {}</button></form>",
                    escape(v)
                ),
                None => String::new(),
            };
            format!(
                "<section><h2>Releases</h2><p class=\"release\">{}</p>{install}\
                 <form method=\"post\" action=\"/check\" class=\"row\">\
                 <button class=\"secondary\" type=\"submit\">Check now</button></form>\
                 <p class=\"note\">The player looks for a newer release once a day while on the network \
                 (<code>update</code> on the card: <code>check</code>, <code>auto</code> or <code>off</code>). \
                 Installing one is the same as an update from a file: playback stops, the player restarts into \
                 it on trial.</p></section>",
                escape(&self.release)
            )
        };
        format!(
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
             <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
             <title>{name} · evo-esp32</title>\
             <style>{css}</style></head>\
             <body><main>\
             <header><div class=\"mark\"></div><div><h1>{name}</h1>\
             <div class=\"sub\">evo-esp32 {version} <span class=\"muted\">{build}</span></div></div></header>\
             {last}\
             <section><h2>Firmware on this player</h2><div class=\"slots\">{rows}</div></section>\
             {release}\
             <section><h2>Update</h2>\
             <form method=\"post\" action=\"/update\" enctype=\"multipart/form-data\" class=\"upload\">\
             <label class=\"file\"><input type=\"file\" name=\"firmware\" accept=\".bin\" required>\
             <span class=\"file-label\">Choose the <code>…ota.bin</code> file</span></label>\
             <button class=\"primary\" type=\"submit\">Update</button></form>\
             <p class=\"note\">Playback stops, the file is written to the free slot, and the player restarts \
             into it — about a minute in all. The new firmware runs on trial first: if it does not start \
             healthily, the player returns to this one by itself. Keep the player powered meanwhile.</p>\
             </section>\
             <section><h2>Factory copy</h2>{keep}{back}</section>\
             <footer>evo-esp32 · <a href=\"/status\">status as text</a></footer>\
             </main></body></html>",
            name = escape(&self.name),
            version = escape(&self.version),
            build = escape(&self.build),
            css = CSS,
        )
    }
}

/// The page's stylesheet: the face's `evo-default` palette (the panel's
/// twelve colours, from `evo-esp32-face`'s `theme.rs`), system fonts,
/// cards on a dark ground, one accent.
const CSS: &str = "\
:root{--bg:#000208;--fg:#e8f0f3;--card:#000514;--secondary:#000e21;--muted:#7d939e;\
--accent:#00cfd1;--accent-fg:#000208;--border:#102239;--ok:#4db956;--warn:#d9a514;--fail:#f14d4c}\
*{box-sizing:border-box}\
body{margin:0;background:var(--bg);color:var(--fg);font:16px/1.5 system-ui,-apple-system,Segoe UI,Roboto,sans-serif;-webkit-font-smoothing:antialiased}\
main{max-width:40rem;margin:0 auto;padding:1.5rem 1rem 3rem}\
header{display:flex;align-items:center;gap:1rem;margin:0 0 1.5rem}\
.mark{width:3rem;height:3rem;border-radius:50%;background:conic-gradient(var(--accent) 0 72%,var(--border) 72% 100%);position:relative;flex:none}\
.mark::after{content:\"\";position:absolute;inset:.45rem;border-radius:50%;background:var(--card)}\
h1{font-size:1.5rem;font-weight:600;margin:0;letter-spacing:.01em}\
h2{font-size:.8rem;font-weight:600;letter-spacing:.12em;text-transform:uppercase;color:var(--muted);margin:0 0 .75rem}\
.sub{color:var(--fg);font-size:.95rem}.muted{color:var(--muted)}\
section{margin:0 0 2rem}\
.slots{display:grid;gap:.6rem}\
.slot{background:var(--card);border:1px solid var(--border);border-radius:.75rem;padding:.8rem 1rem}\
.slot.current{border-color:var(--accent)}\
.slot-head{display:flex;align-items:center;gap:.5rem;flex-wrap:wrap}\
.label{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:.95rem;color:var(--fg)}\
.version{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:.9rem;margin:.3rem 0 .15rem}\
.what{color:var(--muted);font-size:.9rem}\
.tag{font-size:.72rem;letter-spacing:.06em;text-transform:uppercase;border-radius:999px;padding:.1rem .55rem;border:1px solid}\
.tag.ok{color:var(--ok);border-color:var(--ok)}.tag.warn{color:var(--warn);border-color:var(--warn)}.tag.fail{color:var(--fail);border-color:var(--fail)}\
.last{border-left:3px solid;border-radius:.25rem;padding:.5rem .8rem;margin:0 0 1.5rem;background:var(--card)}\
.last.ok{border-color:var(--ok)}.last.fail{border-color:var(--fail)}.last .muted{display:block;font-size:.75rem;letter-spacing:.08em;text-transform:uppercase}\
.upload{display:flex;gap:.6rem;flex-wrap:wrap;align-items:stretch}\
.file{flex:1 1 14rem;display:flex;align-items:center;min-height:2.8rem;background:var(--secondary);border:1px dashed var(--border);border-radius:.6rem;padding:0 1rem;cursor:pointer;color:var(--muted)}\
.file:hover{border-color:var(--accent)}.file input{position:absolute;opacity:0;width:0;height:0}\
.file:has(input:valid){color:var(--fg);border-style:solid}\
.file:has(input:valid) .file-label::after{content:\" — ready\";color:var(--accent)}\
code{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:.9em}\
button{font:inherit;font-weight:600;border-radius:.6rem;padding:.65rem 1.25rem;border:1px solid transparent;cursor:pointer;min-height:2.8rem}\
button.primary{background:var(--accent);color:var(--accent-fg)}button.primary:hover{filter:brightness(1.08)}\
button.secondary{background:var(--secondary);color:var(--fg);border-color:var(--border)}button.secondary:hover{border-color:var(--accent)}\
.row{margin:0 0 .5rem}\
.note{color:var(--muted);font-size:.9rem;margin:.75rem 0 0}\
.release{font-size:1.05rem;margin:0 0 .75rem}\
footer{color:var(--muted);font-size:.8rem;border-top:1px solid var(--border);padding-top:1rem}\
footer a{color:var(--muted)}footer a:hover{color:var(--accent)}\
a.back{color:var(--accent);text-decoration:none;font-weight:600}a.back:hover{text-decoration:underline}\
@media (prefers-color-scheme:light){:root{--bg:#f5f9fb;--fg:#0b1a24;--card:#ffffff;--secondary:#e9f0f4;--muted:#4b575f;--border:#c9d6de;--accent:#007daa;--accent-fg:#f5f9fb}}";

/// The page a browser sees after it asked for something: what happened,
/// and the way back. `back_in` seconds, when given, reloads the player's
/// page by itself — after an update, once the player has restarted.
pub fn outcome_page(
    name: &str,
    headline: &str,
    detail: &str,
    ok: bool,
    back_in: Option<u32>,
) -> String {
    let refresh = back_in.map_or(String::new(), |s| {
        format!("<meta http-equiv=\"refresh\" content=\"{s};url=/\">")
    });
    let wait = back_in.map_or(String::new(), |s| {
        format!(
            "<p class=\"note\">This page returns to the player by itself in about {s} seconds. \
             If it does not, open it again once the panel is back.</p>"
        )
    });
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">{refresh}\
         <title>{name} · evo-esp32</title><style>{CSS}</style></head>\
         <body><main>\
         <header><div class=\"mark\"></div><div><h1>{name}</h1><div class=\"sub muted\">evo-esp32</div></div></header>\
         <div class=\"last {class}\"><span class=\"muted\">{kind}</span> {headline}</div>\
         <p>{detail}</p>{wait}\
         <p><a class=\"back\" href=\"/\">Back to the player</a></p>\
         </main></body></html>",
        name = escape(name),
        headline = escape(headline),
        detail = escape(detail),
        class = if ok { "ok" } else { "fail" },
        kind = if ok { "Done" } else { "Not done" },
    )
}

/// HTML's five.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(boundary: &str, parts: &[(&str, &[u8])]) -> Vec<u8> {
        let mut b = Vec::new();
        for (head, body) in parts {
            b.extend_from_slice(
                format!("--{boundary}\r\n{head}\r\n\r\n").as_bytes(),
            );
            b.extend_from_slice(body);
            b.extend_from_slice(b"\r\n");
        }
        b.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        b
    }

    fn split(
        boundary: &str,
        body: &[u8],
        chunk: usize,
    ) -> io::Result<(Vec<u8>, bool)> {
        let mut f = Form::new(boundary);
        let mut out = Vec::new();
        for c in body.chunks(chunk.max(1)) {
            f.feed(c, &mut |b: &[u8]| {
                out.extend_from_slice(b);
                Ok(())
            })?;
        }
        Ok((out, f.done()))
    }

    #[test]
    fn the_content_type_names_the_framing() {
        assert_eq!(Framing::from_content_type(None), Ok(Framing::Raw));
        assert_eq!(
            Framing::from_content_type(Some("application/octet-stream")),
            Ok(Framing::Raw)
        );
        assert_eq!(
            Framing::from_content_type(Some(
                "multipart/form-data; boundary=----WebKitFormBoundaryx7"
            )),
            Ok(Framing::Form {
                boundary: "----WebKitFormBoundaryx7".into()
            })
        );
        assert_eq!(
            Framing::from_content_type(Some(
                "Multipart/Form-Data; boundary=\"q\""
            )),
            Ok(Framing::Form {
                boundary: "q".into()
            })
        );
        assert!(
            Framing::from_content_type(Some("multipart/form-data")).is_err()
        );
    }

    #[test]
    fn a_browsers_form_yields_the_file_whatever_the_chunking() {
        let image: Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect();
        let body = form(
            "----WebKitFormBoundaryAb12",
            &[
                ("Content-Disposition: form-data; name=\"note\"", b"hello"),
                (
                    "Content-Disposition: form-data; name=\"firmware\"; filename=\"evo.ota.bin\"\r\nContent-Type: application/octet-stream",
                    &image,
                ),
            ],
        );
        for chunk in [1usize, 7, 64, 4096, body.len()] {
            let (out, done) =
                split("----WebKitFormBoundaryAb12", &body, chunk).unwrap();
            assert_eq!(out, image, "chunk {chunk}");
            assert!(done, "chunk {chunk}");
        }
    }

    #[test]
    fn a_file_that_contains_the_boundarys_prefix_is_passed_whole() {
        let mut image = b"\r\n--XY".to_vec();
        image.extend_from_slice(b"\r\n--XYZ not the end \r\n--X");
        let body = form(
            "XYZW",
            &[("Content-Disposition: form-data; name=\"f\"; filename=\"a.bin\"", &image)],
        );
        for chunk in [1usize, 3, 5, 1000] {
            let (out, done) = split("XYZW", &body, chunk).unwrap();
            assert_eq!(out, image, "chunk {chunk}");
            assert!(done);
        }
    }

    #[test]
    fn a_form_without_a_file_is_refused() {
        let body = form(
            "b",
            &[("Content-Disposition: form-data; name=\"note\"", b"x")],
        );
        assert!(split("b", &body, 16).is_err());
    }

    #[test]
    fn the_image_length_is_read_from_its_header_and_segments() {
        // Two segments of 100 and 37 bytes, the checksum padded to a
        // sixteen-byte block, a SHA-256 appended.
        let mut img = vec![0u8; HEADER];
        img[0] = IMAGE_MAGIC;
        img[1] = 2;
        img[23] = 1;
        for len in [100u32, 37u32] {
            img.extend_from_slice(&0x4200_0000u32.to_le_bytes());
            img.extend_from_slice(&len.to_le_bytes());
            img.extend(std::iter::repeat_n(0xAB, len as usize));
        }
        let body = img.len();
        while (img.len() + 1) % 16 != 0 {
            img.push(0);
        }
        img.push(0x5A);
        img.extend_from_slice(&[0u8; 32]);
        let slot = [img.clone(), vec![0xFF; 500]].concat();
        let len = image_length(|at, buf| {
            buf.copy_from_slice(&slot[at..at + buf.len()]);
            Ok(())
        })
        .unwrap();
        assert_eq!(len, img.len());
        assert_eq!(len, (body + 1).div_ceil(16) * 16 + 32);
        assert!(image_length(|_, buf| {
            buf.fill(0);
            Ok(())
        })
        .is_err());
        // A copy cut short inside the segments says so plainly.
        let cut = &slot[..HEADER + SEGMENT_HEADER + 50];
        let short = image_length(|at, buf| {
            if at + buf.len() > cut.len() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "failed to fill whole buffer",
                ));
            }
            buf.copy_from_slice(&cut[at..at + buf.len()]);
            Ok(())
        })
        .unwrap_err();
        assert_eq!(
            short.to_string(),
            "the file ends before the image does: not a whole copy"
        );
    }

    #[test]
    fn the_image_identity_is_the_descriptors_elf_sha() {
        // An image whose first segment opens with the descriptor.
        let mut img = vec![0u8; HEADER];
        img[0] = IMAGE_MAGIC;
        img[1] = 1;
        img.extend_from_slice(&0x4200_0000u32.to_le_bytes());
        img.extend_from_slice(&256u32.to_le_bytes());
        let mut desc = vec![0u8; 256];
        desc[..4].copy_from_slice(&DESCRIPTOR_MAGIC);
        let sha: Vec<u8> = (0x48u8..0x68).collect();
        desc[ELF_SHA_AT..ELF_SHA_AT + 32].copy_from_slice(&sha);
        img.extend_from_slice(&desc);
        let read = |at: usize, buf: &mut [u8]| {
            buf.copy_from_slice(&img[at..at + buf.len()]);
            Ok(())
        };
        let got = image_identity(read).unwrap();
        assert_eq!(&got[..], &sha[..]);
        assert_eq!(short_id(&got), "48494a4b");
        let mut plain = img.clone();
        plain[DESCRIPTOR_AT] = 0;
        assert!(image_identity(|at, buf: &mut [u8]| {
            buf.copy_from_slice(&plain[at..at + buf.len()]);
            Ok(())
        })
        .is_err());
        // The head alone decides before any flash is touched.
        assert_eq!(judge_head(&img[..HEAD_LEN]), Ok(()));
        assert!(judge_head(&img[..HEAD_LEN - 1]).is_err());
        assert!(
            judge_head(b"PK\x03\x04 not an image at all, some bytes more")
                .is_err()
        );
        let why = judge_head(&plain[..HEAD_LEN]).unwrap_err();
        assert!(why.contains(".factory.bin"), "{why}");
        let blank = [0xFFu8; HEAD_LEN];
        let why = judge_head(&blank).unwrap_err();
        assert!(why.contains("blank flash"), "{why}");
    }

    #[test]
    fn a_signed_image_is_whole_with_its_block() {
        assert_eq!(signature_block_at(4096), 4096);
        assert_eq!(signature_block_at(4097), 8192);
        assert_eq!(signed_length(4_410_624), 4_415_488);
        assert!(is_whole(4_410_624, 4_410_624));
        assert!(is_whole(4_415_488, 4_410_624));
        assert!(!is_whole(4_410_000, 4_410_624));
        assert!(!is_whole(4_419_584, 4_410_624));
    }

    #[test]
    fn card_images_are_named_by_suffix_and_renamed_by_outcome() {
        assert!(is_card_image("evo-esp32-board-0.2.0.ota.bin"));
        assert!(is_card_image("FIRMWARE.OTA.BIN"));
        assert!(!is_card_image(".ota.bin"));
        assert!(!is_card_image("evo-esp32-board-0.2.0.factory.bin"));
        assert!(!is_card_image("evo-esp32-board-0.2.0.ota.applied"));
        assert_eq!(
            applied_name("evo-esp32-board-0.2.0.ota.bin"),
            "evo-esp32-board-0.2.0.ota.applied"
        );
        assert_eq!(
            refused_name("evo-esp32-board-0.2.0.ota.bin"),
            "evo-esp32-board-0.2.0.ota.refused"
        );
    }

    #[test]
    fn the_status_renders_its_lines_and_page() {
        let s = Status {
            name: "evo-player".into(),
            version: "0.2.0".into(),
            build: "build:v0.2.0".into(),
            slots: vec![
                Slot {
                    label: "factory".into(),
                    version: "v0.2.0".into(),
                    ..Default::default()
                },
                Slot {
                    label: "ota_0".into(),
                    version: "v0.2.0-3-gabc".into(),
                    id: "4812b4d0".into(),
                    running: true,
                    boot: true,
                    state: "valid".into(),
                },
                Slot {
                    label: "ota_1".into(),
                    ..Default::default()
                },
            ],
            may_keep: true,
            last: "written ota_0, confirmed after 71 s".into(),
            release: "0.3.0 available".into(),
            may_pull: Some("0.3.0".into()),
        };
        assert_eq!(
            s.lines(),
            vec![
                "factory: v0.2.0",
                "ota_0: v0.2.0-3-gabc (4812b4d0), running, next boot, valid",
                "ota_1: empty",
            ]
        );
        let page = s.page();
        assert!(page.contains("<title>evo-player · evo-esp32</title>"));
        assert!(page.contains("enctype=\"multipart/form-data\""));
        assert!(page.contains("action=\"/factory\""));
        assert!(page.contains("confirmed after 71 s"));
        assert!(page.contains("--accent:#00cfd1"));
        assert!(page.contains("class=\"slot current\""));
        assert!(page.contains("<span class=\"tag ok\">confirmed</span>"));
        assert!(page
            .contains("v0.2.0-3-gabc <span class=\"muted\">4812b4d0</span>"));
        assert!(!Status::default().page().contains("action=\"/factory\""));
        let done = outcome_page(
            "evo-player",
            "Written to ota_1",
            "Restarting.",
            true,
            Some(70),
        );
        assert!(done.contains("content=\"70;url=/\""));
        assert!(done.contains("class=\"last ok\""));
        let failed = outcome_page(
            "evo-player",
            "Not a firmware image",
            "<x>",
            false,
            None,
        );
        assert!(failed.contains("class=\"last fail\""));
        assert!(failed.contains("&lt;x&gt;"));
        assert!(!failed.contains("http-equiv"));
    }
}

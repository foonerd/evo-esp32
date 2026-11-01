# Changelog

All notable changes to evo-esp32 are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versions
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

- The HTTP client's send buffer holds a redirect's whole request line.
  GitHub answers a release-asset request with a redirect to a signed
  URL of about 990 characters; with a 1 KB send buffer, 64 bytes were
  left for the headers, the client logged `HTTP_HEADER: Buffer length
  is small to fit all the headers` on every update check and sent them
  in a second write, and a slightly longer token would have failed the
  check outright. The buffer is 2 KB.

- The shipped station list re-checked on 1 November: every one of the
  56 streams answers and every logo decodes, except KEXP's, whose site
  now serves a web page in place of its icons; the station stays, its
  logo is left out.

- Passwords on the card are sealed to the player. A password written
  in plain text in `evoconfig.txt` (`wifi_password`, `web_password`, a
  share's) is sealed at the next boot and the line rewritten as
  `sealed:1:…` — AES-256-GCM under a key the player makes for itself
  and keeps in its flash — so a card taken out of the player gives
  nothing away. To change a password, write it in plain text again. A
  card moved to another player, or a player flashed over USB from
  nothing, reports the password as sealed for another player and waits
  for it to be written again; the Wi-Fi join is not attempted with a
  password the player cannot open. `evostatus.txt` says what was
  sealed. A lock on the card, not a vault: the key is in the device.

## [0.2.6] - 2026-10-31

- PCM5122 boards with oscillators of their own play every rate. The
  HiFiBerry DAC+ Pro, the Allo Boss and the boards built to the Boss's
  overlay (InnoMaker's DAC Mini) carry two crystals, one per sample-rate
  family, switched through the DAC chip's own GPIO3 and GPIO6; one may
  be running at power-on, and the chip then followed that clock instead
  of the player's bit clock: right for one family (96 kHz played), noise
  for the other (every 44.1 kHz track). The bring-up now looks for a
  running oscillator and, finding one, starts the chip again with both
  switched off, so it runs its PLL from the bit clock as the other
  PCM5122 boards do. Boards without oscillators are untouched.

## [0.2.5] - 2026-10-31

- The WM8960 Audio HAT (Waveshare's; the ReSpeaker 2-Mics class) plays.
  A new `wm8960` driver brings the codec's DAC path up over I2C —
  clocked by its PLL from the HAT's own 24 MHz oscillator, 16-bit I2S,
  headphones and speakers at 0 dB with the level the sink's, like every
  HAT. The codec takes writes only, so the bus survey cannot see it; the
  player probes it with a reset at boot. It stops at 48 kHz: for a
  stream above that it mutes itself, the board plays on, and the face's
  strip says "HAT silent above 48 kHz".

- The codecs are told of a rate change while the I2S clocks are
  stopped. The WM8960's control interface takes the bit clock on the
  HAT's wires as noise (every write succeeded with the clocks stopped
  and none with them running), so the feeder holds the gap it opens
  for the rate change until the main task has written the codecs. The
  board's bus runs at 100 kHz, not 400: the WM8960 wants SCL low for
  1.3 µs, which 400 kHz does not give, and the bus leaves the board over
  wires; every part on it is within spec at 100.

- The "passed over unopened" line names the rate it judged by, from
  the card's facts, instead of "0 Hz" when the listing had none.

- The heap lives in PSRAM; the internal SRAM is the drivers'. The
  mid-size allocations (512 B to 16 KB) went to internal memory first
  for the decoder's sake when PSRAM ran at 40 MHz. At 80 MHz the decoder
  costs the same from either, and the 30 KB they held was the DMA-capable
  memory the card's driver and the panel's SPI master were failing to
  get: within minutes the largest DMA-capable block had drifted from
  31 KB to a few hundred bytes, the card could not read, every track
  failed to open, the player advanced through the library at three a
  second, and the SPI master's unchecked allocation ended in a panic.
  Every allocation now prefers PSRAM and falls back to internal; the
  panel's transfer buffer is allocated by DMA capability. Measured over
  an MP3 album and a 96 kHz FLAC: internal free 42–50 KB (was 16–23),
  the largest DMA-capable block 23 KB and flat (was draining).

- Three tracks failing to open within ten seconds hold the player with
  "tracks are failing: …" on the face instead of the next track; Play
  tries again. The heartbeat warns when the largest DMA-capable block
  is under 2 KB.

## [0.2.4] - 2026-10-31

- The volume follows the finger again while the decoder runs flat out
  (96 kHz). Since 0.2.2 the sink took its level only when the face
  painted, and the face does not paint while the decoder is short of
  lead, so a slide was heard up to a second late and the knob moved
  in jumps; the Faders face, where every slider repaint was the whole
  fader, was the worst. The sink now takes the level as the slide
  sets it, and when the output changes from anywhere else; a slider
  repaints only the band its knob crossed; and a slider's repaint
  passes the lead gate while a finger is on the face, since it is a
  few windows on the panel. Measured on the bench at 96 kHz: the
  level moves every 74–122 ms (median 90) against 500–1400 ms before.

- A slider holds the finger until it lifts. Once a slide has begun,
  every sample is read along the slider's axis wherever the finger has
  drifted — into the text, past either end — instead of freezing when
  it left the 12 px either side of the bar, which on a 4 px fader it
  did on most sweeps. A fader also takes a finger from 20 px either
  side (44 px across, a fingertip) rather than 12. The tracker's
  contract said this all along; the code now does it.

- A finger on the seek slider shows where it would land: the knob, the
  fill (the ring on the Disc) and the elapsed clock follow the finger
  live, and the seek is applied on the lift, as before. Until now the
  slider sat still through the drag and jumped on the lift. Moving
  backwards on the Disc's ring erases the arc passed instead of
  repainting the whole ring.

- Touch is read by a thread of its own, woken by the controller's
  interrupt, and the loop takes every report in order with the time it
  was read. Until now the loop read one report when its turn came —
  every 85–120 ms under a 96 kHz track, and not at all during a paint
  stage of up to 400 ms, so a quick tap could be lost and a slide was
  sampled coarsely. A tap now lands whatever the face is doing, and a
  slider can be set to the exact second or percent. The I2C bus is
  shared behind a mutex for it.

- The PSRAM runs at 80 MHz, not ESP-IDF's 40 MHz default for the C5
  (its own C5 example runs 80). The decoder's buffers, the sink's
  reservoir and every thread's stack are in PSRAM and stream through a
  32 KB cache, so the clock bounded the decoder. On the same 96 kHz
  FLAC the main task fell from 70–73% of the core to 52–55% and idle
  rose from 5–10% to 28–31%: the headroom the face and touch had lost
  since 0.2.0 is back. The boot memory test and the update's trial
  guard a module that cannot keep the clock.

- The player stands on its own hardware layer. The board crate
  (`boards/waveshare-esp32-c5-touch-lcd-28`), the chip drivers
  (`drivers/`: CST3530 touch, CH32V003 expander, ES8311 codec, ES9018K2M
  and PCM512x DACs) and the card, Wi-Fi and NVS modules of
  `platform/esp-idf` are this repository's, carried over from the
  Volumio ESP companion (MIT, same author) at the revision the firmware
  was built against, less that project's HAL traits. No crate of
  evo-esp32 depends on the companion any more, by git or otherwise; the
  two projects share a vendor's board and nothing else. The firmware's
  behaviour is unchanged.

## [0.2.3] - 2026-10-30

- The panel lights in 5 s from power or a restart, not 14: the face
  is painted before the card is read, saying "Reading the card…" on
  its strip until the first track takes over. Every rotation change
  restarts the player, so every one of them had the panel dark for ten
  seconds.

- The card is listed in half a second, not eight. Since 0.1.0 the
  listing opened every audio file for the rate and length its header
  states (so a track above the board's ceiling is greyed in the library
  and passed over unopened); since 0.1.5 ESP-IDF's FAT layer has built
  a map of a file's clusters at every read-only open (the fast seeks
  that keep a seek in 96 kHz FLAC from stalling the card), a walk of
  the FAT that cost ~50 ms a file — 125 files, 6 s, before the first
  frame. The listing no longer opens anything: the facts come from a
  cache, filled by a thread of its own once the first track plays,
  which opens each file the one way the layer builds no map for (for
  writing as well as reading; nothing is written) and reads 64 bytes.
  Under a 96 kHz FLAC the fill takes about two minutes for 125 files
  (the core is the decoder's); meanwhile a track above the ceiling is
  passed over at the open, as it always was when the listing did not
  know its rate. Audio starts at 6 s, where it did in 0.1.4.

## [0.2.2] - 2026-10-30

- A third style, **faders**: the disc's art without its ring; in
  portrait the progress is a fader down the left and the volume a fader
  down the right, each filling from its foot, the times above and below
  the one, the speaker and the level above and below the other; the
  text keeps to the middle and the transport has nothing within a
  thumb's reach. From the first owner's note that the disc's volume
  strip sits too near the pause button — at the default 40 % the knob is
  9 px above it. In landscape, where there is no side to spare, it is
  the disc's art with the card's strip. `style = faders` on the card;
  **Style ›** in Settings steps through the three.

- A rotation set during a new firmware's trial minute no longer throws
  the firmware away. The restart it needs counted as the trial failing
  and the bootloader returned to the firmware before; now the setting is
  written, the panel says so, and the restart waits for the gate's
  confirmation.


## [0.2.1] - 2026-10-30

- A PCM5122 HAT plays. A Pi HAT built on TI's PCM5121/PCM5122 (IQaudIO
  DAC+, HiFiBerry DAC+, Waveshare's PCM5122 Audio Board) has no clock
  of its own and plays nothing until it is told, over I2C, to run its
  PLL from the bit clock — the Linux `pcm512x` driver's sequence for a
  HAT without a master clock, now a driver in the companion repository.
  At boot the player lists every address on the header's bus that
  answers (the panel's own parts aside), in the log and in
  `evostatus.txt`, then brings up what it recognises: an ES9018K2M at
  0x48, a PCM512x at 0x4C or 0x4D by two of its reset values. The
  level stays the sink's; the DAC sits at 0 dB like the others.

- The bit clock is 64 times the sample rate: 16-bit samples in 32-bit
  slots, left-aligned and Philips-shifted, the frame every Pi HAT DAC
  takes. It was 32 fs (16-bit slots), which a PCM5122 accepts but an
  ESS Sabre in hardware mode read as wrong words — loud, distorted, and
  deaf to the volume. The on-board codec is told the slot width and
  multiplies the bit clock by 4 to its master clock instead of 8.

- A deferred release look waits for a quiet moment that is one. The gap
  between two tracks counted as quiet: the look was tried there, the
  memory floor failed again (the decoder's memory is not back in a
  gap), and the retry was pushed another ten minutes each time, so a
  player with music on never looked. Now a quiet moment is five seconds
  of nothing playing, a failed test never moves an earlier retry later,
  and tests while quiet are a minute apart.

## [0.2.0] - 2026-10-23

- The card's verdict is on the panel. An image on the card that is the
  firmware already running, or that is refused before the write (not
  whole, not an image, two `.ota.bin` files), was a line in
  `evostatus.txt` only; now the panel says **Done** or **Update failed**
  with the reason as it lights, until a tap. The rename to
  `.ota.applied` or `.ota.refused` failed where an earlier file of that
  name sat (FAT refuses a rename onto one), so the `.ota.bin` stayed and
  was judged again at every boot; the earlier file is replaced, and a
  rename that still fails says so. A copy cut short inside the image is
  refused as "the file ends before the image does: not a whole copy".

- After a USB flash to firmware signed with another key, the update
  slots hold whole images the new key refuses; the boot log called them
  unfinished writes. A whole image with a signature block that does not
  verify is now "signed with another key"; only a slot without one is
  an unfinished write.

- The slot table names each image's version from its descriptor, which
  ESP-IDF filled from `git describe` at configure time; esp-idf-sys
  runs that once and keeps it, so an image built later named a build
  weeks old while its own boot line said the truth. The descriptor's
  version is now the crate's, from the configuration, and `image.sh`
  refuses an image whose descriptor says otherwise.

## [0.1.7] - 2026-10-23

- An update takes the panel over. While the firmware is written the
  face is a message on a cleared screen — **Updating**, "Writing the new
  firmware. Do not switch off: the player restarts by itself when it is
  done" — then **Restarting**; a failure, a copy kept as factory, or the
  way back are messages the same way, an outcome staying up until a
  tap. Before, the state was a line on the face's strip, missed easily,
  the restart's never painted and the outcome gone as soon as music
  resumed. An update from the card at boot lights the panel for it,
  where it ran dark.

- A way back without a computer. A finger held on the panel for five
  seconds from the moment it lights after power-on returns the player
  to the factory copy; the page has **Restart into the factory copy**
  for the same. The update slots keep their firmware (an image the
  bootloader has no record of is kept when it verifies whole, blanked
  only when it does not), so the next update goes on as before. The
  board's BOOT button cannot serve: it is the chip's download strap.

- The player finds its own updates. Once a day on the network it reads
  the release manifest (`<stem>.manifest.json`, published beside the
  images; `update_url` on the card names another place) and shows
  where it stands in Settings (the **Update ›** row: "up to date",
  "0.3.0 available", "check failed: …") and on the page, which also has
  **Check now**. Two taps on the row, or **Install** on the page, fetch
  the release over HTTPS into the free slot through the same signed
  path as any update and restart the player into it on trial.
  `update = auto` on the card installs it by itself once nothing has
  played for a minute; `update = off` never looks. The manifest is
  advice, not trust: the image it names must be signed with the
  player's key like any other. The look waits for a quiet moment when
  internal memory is short (a 96 kHz FLAC playing), and says so.

- Updates are signed. Every image carries a Secure Boot v2 signature
  block (ECDSA P-256), appended by `scripts/image.sh` with the key
  `EVO_SIGNING_KEY` names, and the running firmware accepts a new image
  only if it was signed with the key of its own block: a file that is
  unsigned, signed with another key or changed in transit is refused
  after the write ("not signed with this player's key, or damaged") and
  the slot blanked. No eFuse is burned: the bootloader does not verify,
  so the board stays flashable over USB, and a USB flash is how a player
  moves to a firmware signed with another key. Releases are signed with
  the release key held in CI; a factory copy carries the block with it.

- A 96 kHz FLAC from the card no longer runs dry after a few hours up.
  The card's reader needs an 8 KB buffer in memory the card's DMA can
  reach; it was allocated at every track's start, and once internal
  memory had fragmented behind the network, the update page's server
  and hours of tracks the allocation failed into ordinary memory, where
  the driver copies every block through a bounce buffer: the reader at
  a third of the core instead of a tenth, and the music breaking up
  (955 dry runs in five minutes, measured). The buffer is now set aside
  three seconds into boot, before anything else can claim it, and lent
  to each track in turn. The heartbeat names the DMA-capable largest
  free block and warns while any reader is without it.

- The card is the third way to update. One `<name>.ota.bin` in the
  card's root is applied at boot, before anything plays: the face says
  **Updating**, the image is written to the free slot,
  the file is renamed `<name>.ota.applied`, and the player restarts into
  the new firmware on the same trial as an update from the page. A file
  that is the firmware already running is marked applied without a
  write; one that is not whole, or not an application image, or does
  not verify, is renamed `.ota.refused` with the reason in
  `evostatus.txt`; with more than one `.ota.bin` on the card none is
  touched and the status says so.

- A file that is not a firmware image costs nothing. The first bytes of
  an upload are judged before the update slot is opened: ESP-IDF's
  `esp_ota_begin` erases the previous firmware's boot record the moment
  it is called, so an earlier build that opened the slot first and
  judged after lost the "previous firmware, kept" to a wrong file (the
  slot was then blanked as an unfinished write). Now a `.factory.bin`
  is named as the likely mistake ("the .factory.bin is for USB; the
  page takes the .ota.bin"), any other file is refused as not a
  firmware image, the answer comes in a second or two however large the
  file, and both slots stay as they were.

- A main loop that stops turning restarts the player. The task watchdog
  now panics after five seconds instead of printing register dumps for
  ever with the panel frozen, and the panic leaves an ELF core dump of
  every task in a `coredump` partition (256 KB, carved from the unused
  `storage` region) for the bench to read back with the build's ELF.

- Updates over the network. A player on the Wi-Fi serves a page at
  `http://<name>.local/` with the firmware in each of its three slots
  and a form that takes the release's `.ota.bin`: playback stops, the
  file is written to the update slot not running, and the player
  restarts into it — about a minute in all. The new firmware boots on
  trial (ESP-IDF's rollback) and marks itself valid once the panel is
  lit, the sink has opened, Wi-Fi is up and the loop turns, else the
  bootloader returns to the firmware it had on the next reset; a file
  that is not a firmware image is refused before anything is written,
  a power cut during the write leaves the slot blank, and an update
  that breaks off resumes playback. A firmware that has proved itself
  can be kept as the factory copy — what the player falls back to when
  both update slots fail — from the page or `scripts/push.sh --factory`.
  `scripts/push.sh <player> <image>` does the browser's job from the
  command line. The page is open on the network unless the card sets
  `web_user` and `web_password`, when it asks for them (HTTP Digest:
  the password does not cross the network). The page, `/status` and the
  `firmware` rows of `evostatus.txt` name each slot's version and the
  first bytes of its ELF hash, which tell two builds of one version
  apart. The main loop idles while an image is written and the write
  breathes between sectors, so the watchdog is not tripped.

- The flash is laid out for it: three app slots of 5.3 MB (`factory`,
  `ota_0`, `ota_1`) below 16 MB, which is as far as the cache maps, and
  `otadata` for the bootloader's choice, `coredump` for a panic's dump;
  `scripts/image.sh` now writes
  `<stem>.factory.bin`, the whole flash for USB, and `<stem>.ota.bin`,
  the app for the page, and the release carries both. A player on 0.1.6
  or earlier takes the USB flash once, as its table has no update slots;
  the card keeps its settings.

- Album covers and station logos arrive within seconds again at 96 kHz.
  The tiers of 0.1.6 left the art worker at the lowest priority, below
  the idle-time work of the Volumio sessions, and a 96 kHz FLAC gave it
  so little of the core that a cover took one to two and a half minutes
  (0.1.5: nine to twenty seconds at the same material). It runs on the
  background tier with the rest of the deferred work: a cover now reads
  in under a second to eleven seconds at 96 kHz, by its size and what
  else the core carries; a station's logo takes the same path.

## [0.1.6] - 2026-10-22

- The face answers a touch while a station opens or plays, and the
  music keeps going while the face changes. The main loop — touch, face
  and the decoder's tick — ran below every thread the plugins start
  (ESP-IDF's default for `std` threads), so a thread with the core to
  itself held the loop off for its whole run: the TLS handshake of a
  station's open, and of every segment and playlist of an HLS station,
  each a second or two of this core. From 0.1.3, when the BBC's HLS
  stations joined the Radio list, that was a face that stalled for
  seconds and a touch that took five tries. Threads now stand in three
  tiers around the loop, by what they are for: the threads that feed
  the decoder and the sink (a file's read-ahead, a station's fetcher, a
  room's receiver) one above it, so their bytes land the moment they
  arrive; the loop; and everything else (a track's open, a share's
  listing, the search for servers, the sessions with Volumio peers) one
  below, running when the loop sleeps. Level with the loop, measured on
  the way, the kernel sliced the core between them and a paint stage
  took half a second while a station opened, with the music dropping
  out behind it. A plugin says which tier a thread is for through the
  SDK's `thread` module (its `std` feature); the distribution places it.
  The heartbeat's core line now names every thread (`read/6`,
  `fetch/6`, `open/4`, `peer/4`) instead of showing each as `main/5`.

- A seek in a file on a share no longer barks. After a seek the sink
  played the first frames at once — the ones the demuxer had read on
  its way to the target — and then waited on the share while the
  read-ahead ring, restarted at the new place, gathered its low-water
  mark: a burst, a gap, and then the music. A seek now gathers the
  same quarter second of lead a track start does before the feeder
  plays again, on every source. The decoder's wrapper also withholds
  the frames a seek, an open or a resync leaves without their main
  data (MPEG audio keeps up to 511 bytes of a frame in the frames
  before it, the bit reservoir), as Symphonia's own decoder and
  Android's do; those frames still feed the reservoir, so the next one
  plays whole.

- Peers of the Volumio protocol are written to from their own thread.
  The main loop posted every state push into the peers' sockets
  itself; a peer slow to read could hold the loop on a send. An outbox
  thread takes the posts.

- A turn of the loop that runs long names which part of the face
  phase took it: the state, the presence, the art or the paint (with
  the stage painted).

- The read-ahead ring is tuned per source. The card keeps small,
  paced chunks (its DMA reaches them; the pace shares the SPI bus
  with the panel). A file on a server reads the same 8 KB chunks — a
  64 KB reply overran the chip's Wi-Fi receive buffers and came at a
  third of the link's rate — with a deeper landing zone, paced after
  it so a listing or the next track's open is not queued behind a
  burst of reads. One request is in flight at a time: a link that
  answers in 14 ms carries 570 KB/s, so a 96 kHz FLAC over the network
  is at the edge of this chip's radio whatever the chunk; 44.1 and
  48 kHz play with room to spare.

- NFS version 4.1. An `nfs://` share is mounted by NFS 4.1 where the
  server speaks it, by version 3 where it does not: one TCP
  connection to the NFS port, no portmapper and no MOUNT, EXCHANGE_ID
  and CREATE_SESSION for a session with one slot, SEQUENCE at the
  head of every compound, RECLAIM_COMPLETE, the export reached from
  the server's pseudo file system by PUTROOTFH and LOOKUP, folders
  listed by READDIR asking for type, size and file handle per entry,
  files read by READ with the anonymous stateid (a reader that opens
  nothing may, RFC 8881 §8.2.3), the session and client id destroyed
  when the export is let go. AUTH_SYS as before; a server that wants
  Kerberos says so in the log. A server with no version 4 answers the
  first call with a version mismatch and is mounted by version 3 as
  in 0.1.4; one with 4.0 only likewise. A session or lease the server
  let go while the player was quiet is mounted again on the next
  listing. Measured on the desk against nfs-ganesha 4.3 (NFS 4.1 only,
  AUTH_SYS, numeric owners): list with handles, reads at the head,
  the middle, the tail and the end, a FLAC from 30 s lands to the
  second; the version 3 server (unfs3) still mounts by 3 through the
  mismatch.

- A track changes faster, and a pressed Next is a Next. A file on the
  card opened on the loop's thread — "a read away" — but an open is
  the file system mapping the file's clusters (fast seek) and the
  demuxer reading its metadata, and the loop, the face and the finger
  waited for it: 450 ms from Next to the new track in hand, a loop
  turn of 750–890 ms at a track start, and at a natural track end the
  DMA ran dry for 270–320 ms between the songs of an album. Every open
  now runs on the open thread, a card file as a stream always did (21
  ms from Next to the new track; no dry-out at a natural end). And a
  user's Next or Previous drops the old track's 600 ms tail instead
  of playing it out; a track that ends by itself keeps its tail, for
  a gapless album.

- A FLAC's embedded cover is no longer read at the open. A ripped
  FLAC carries its picture in a PICTURE block, half a megabyte and
  more; Symphonia 0.6 reads it whole at the probe (its limit for FLAC
  pictures is a `TODO`) and this player takes its art from the folder.
  The plugin reads the file's metadata headers — four bytes a block —
  and lays a one-byte patch over each picture's header as the file is
  read, so the demuxer sees padding and seeks past it; the file and
  its offsets are untouched. On the card and on a share alike. A line
  per open says what it cost (`open: 73 ms to the source (1 pictures
  read as padding in 1 ms), 149 ms to probe: the source moved 2
  times, 0 ms in its seeks, 129 ms in its reads`). What remains of a
  track start on this board is the SPI bus the panel and the card
  share: at a skip the probe's 150 ms of reads interleave with the
  new track's repaint and the folder's cover, about 500 ms in all.

- A NAS running an older rpcbind and mountd mounts. An RPC record
  over TCP is a four-byte marker and the record; the client wrote
  them in two sends, and a server built on an older libtirpc closes
  the connection on a marker that arrives alone (confirmed from a
  desk host: one send answers from any port, two sends get the
  connection closed). The marker and the record go in one send now;
  the SMB client's NetBIOS header and message likewise.

- Listing a share is a listing. The NFS and SMB listings no longer
  open and read every audio file for its duration and sample rate —
  a folder of forty tracks on a share over Wi-Fi cost forty opens
  and forty reads before it showed; the browse shows the names and
  the player learns the rest when a track is opened.

- A share's mount is kept and remembered. The player mounts a share
  once for the listing and once for the player and keeps both; a
  mount the server let go (a stale handle, a session the server
  expired, a connection it closed) is mounted again on the next call,
  once, with what the first mount learnt — the portmapper's ports
  and the root handle, so a remount is one connection and one
  GETATTR. The version a server does not speak is remembered too: a
  version 3 server is not asked for 4.1 on every mount.

- An NFS server with no portmapper, or a firewall in front of one, is
  still found: mountd is tried at 892, 20048 and 2049 when the
  portmapper does not answer or lists no MOUNT service, and the
  error says what was tried.

- Reserved source ports rotate. A server with a `secure` export
  wants a source port below 1024; the player took 1023 every time
  and a server holding the previous connection in TIME_WAIT refused
  it. The port now walks down from 1023 to 600 and round.

- The picture scan at an open is for FLAC only; an MP3, an AAC, a
  WAV or an Ogg is opened without it.

- The chip's transport sized for a share's reads, from its sources
  rather than its documentation. Every received frame passes through
  the TCP/IP task's own mailbox before anything else, and a frame it
  cannot take is freed without a word; at its default of 32 a reply
  longer than 32 segments lost its tail every time and was sent again
  after the server's timer. That mailbox and the socket's are 64 now,
  the receive window 64 KB (one whole NFS or SMB read, the largest
  without window scaling), the Wi-Fi driver's receive buffers 12
  static, 12 in the block-ack window and 64 dynamic (Espressif's floor
  for a PSRAM-first build is 16; 12 is what the SRAM budget beside an
  MP3 decoder allows), lwIP's retransmission floor 500 ms instead of
  1.5 s (a lost request is sent again at 0.5, 2.5 and 6.5 s, not 1.5,
  7.5 and 19.5), selective acknowledgements on. Measured from a
  server on the desk over 5 GHz: 8 KB reads at 360–440 KB/s, where the
  same path stalled for seconds before. Tried and set aside, with the
  numbers in the code: 64 KB reads into a 1 MB ring (fewer bytes and
  more of the core), and the main task above the reader threads (it
  polls while a ring is short, and starved them).

- A 96 kHz FLAC from a share is at this core's limit: decode takes
  60–65 % of it, the Wi-Fi driver, lwIP and the reader 20–25 %, and
  the decoder falls to 0.93 of real time — the DMA runs dry each
  second and the panel answers late. 44.1 and 48 kHz play with room.
  The lever is the decoder's cost, which is the next work.

- A share's connections keep alive, with the Linux NFS client's values
  (60 s idle, 60 s between probes, three probes): a server that went
  away, or a radio that came back on another path, fails the next
  read within minutes instead of hanging it for its timeout.

- `wifi_band` in evoconfig.txt: `auto` (the default: 5 GHz where the
  network is on it, either band where it is not), `5` (the same,
  said plainly) or `2.4`. Before, a network on both bands was joined
  on whichever the driver's first search found and kept there by the
  remembered access point — a coin toss between boots. Measured on the
  bench, the same radio, server and album: 360–440 KB/s over 5 GHz,
  26–54 KB/s over 2.4 GHz channel 11, where the player sat a third
  idle waiting for bytes. A network not on 5 GHz costs one search
  there at boot before the player falls back to either band. The
  companion's `WifiNet::join` takes the band (c73cb2b).

- A line per track from a server says what the link gave: `read:
  1248 KB from the server in 3317 ms of reads: 376 KB/s while
  reading` — the rate while the reader was in a read is the link's
  ceiling for one request in flight.

## [0.1.5] - 2026-10-22

- Fixed: a seek into a track on the card could slow the card for the
  rest of the track. The read-ahead ring restarted at the exact byte
  the decoder asked for — a FLAC frame begins at any byte — and from
  there the file system served every read as a partial block through
  its own buffer plus the remainder into the ring's buffer at an odd
  offset, which the card driver cannot address directly and copies
  block by block through a bounce buffer: a third of the core, until
  the track ended, three seeks in four. At 96 kHz the decoder then
  starved (the DMA ran dry several times a second) and, audio first,
  the face stopped painting: a tap waited 10–16 s for the track to
  end. In 0.1.4 since the ring (`ahead::ReadAhead`). The ring now
  restarts on a 512-byte boundary and the decoder's head starts inside
  it, so the card sees whole blocks from a block boundary after every
  seek; a unit test records what the file is asked. Measured on the
  board: the reader back at 4–8 % after a seek (was 30–34 %).

- Seeking a 96 kHz FLAC on the card is faster. Three things made a
  seek hold the loop for 1.6 s — and the finger with it. FatFs walked
  the file's cluster chain from the start for every one of the
  demuxer's 10–14 bisection probes: fast seek is on, with a 2 KB
  cluster map per open file (the default 64-word map failed on every
  file of a well-used card and fell back to the walk). The ring then
  refilled all 384 KB back to back, holding the SPI bus — the panel's
  bus too on this board — so the paint stage after the seek took
  450 ms instead of 90: the reader is eager only until 32 KB is ahead
  of the decoder and then takes one 8 KB chunk per 6 ms, 1.3 MB/s,
  twice what a 96 kHz FLAC needs. Measured on the board: the
  demuxer's seek 110–200 ms, the loop's turn 315–490 ms (was 660–830
  with fast seek alone, 1.6 s before). A line per seek says what it
  cost and where (`seek to 103075 ms took 179 ms: the source moved 13
  times, 0 ms in its seeks, 157 ms in its reads`).

- The face answers while the decoder runs flat out. The loop gives
  audio the core first: with less than 120 ms decoded in hand it
  decodes and paints nothing, which a 96 kHz track from an encrypted
  share holds for its whole length. Now a touch since the face last
  looked, a frame half painted, or a second without a paint lets a
  stage through whenever 60 ms is in hand (a stage is under 90 ms and
  the DMA ring stands behind the reservoir); and two seconds dark the
  face paints whatever the lead, so a player whose decoder cannot keep
  up can still be told to stop.

- The heartbeat says what a finger waits: the loop's longest turn in
  the period (the longest a touch report can wait to be read) and how
  many touches waited for the face, up to how long; a turn over 300 ms
  is logged with its phases (tick, touch and network, face, the rest).

- SMB 3.1.1. The SMB client offers dialect 3.1.1 beside 2.0.2 and 2.1,
  with the two negotiate contexts it takes (pre-authentication
  integrity by SHA-512 with a salt, encryption by AES-128-GCM), folds
  the negotiate and session-setup messages into the pre-authentication
  hash, derives the signing and sealing keys from the session key and
  that hash by SP800-108, signs with AES-CMAC where a 2.x session signs
  with HMAC-SHA256, and seals every message in a transform header when
  the session or the share demands encryption (Samba's `smb encrypt =
  required`, a NAS's "encrypt data" switch). Dialects 3.0 and 3.0.2 are
  not offered: a server is on 2.x or 3.1.1 with the player. A guest
  session has no key to seal with, and such a share says "access
  denied"; a 2.x server that demands encryption is told it needs SMB 3.
  All in software for now, RustCrypto, one module to swap for a chip's
  hardware when the measurement calls for it. Measured on the desk
  against Samba 4.17 with minimum dialect 3.1.1, signing mandatory and
  encryption required: list and play, a FLAC from 30 s lands to the
  second; the default and signing-mandatory servers and a Volumio's
  share now negotiate 3.1.1 too. RFC 4493 and NIST GCM vectors are
  unit tests. Measured on the board: sealing costs some 7 % of the
  core at 96 kHz. A 44.1 or 48 kHz track from an encrypted share
  plays with the usual headroom; a 96 kHz FLAC from one plays to the
  end without an underrun but with little to spare, and is the limit
  of this chip, whose AES block has no GCM mode to take the work.

- SMB1's old session setup proved. The `smb1://` path a server without
  extended security takes (NTLMv2 in the session setup's own fields)
  and SMB1's MAC signing were written to the specification in 0.1.4
  and not exercised; both now are, against Samba 3.6.6 as first
  shipped (guest, a user and password, signing mandatory, a wrong
  password refused). A Samba 3 that took the 2016 security updates
  refuses NTLMv2 outside NTLMSSP by policy, and the player's log now
  says so — "the server refuses NTLMv2 in the old session setup; a
  guest share, or SMB 2 if it has it" — where before it said "invalid
  parameter". NTLMv1 stays unspoken.

## [0.1.4] - 2026-10-21

- NFS exports in the library. A new source plugin,
  `plugins/org.evoframework.source.nfs`, lists the exports the card
  names (`share = nfs://host/export/folder`) and reads their files,
  with an NFS version 3 client written here in plain `std::net`:
  SunRPC over TCP with record marking, AUTH_SYS as an ordinary user,
  the portmapper asked for mountd and nfsd (or the URL's port for
  both, as a userspace server runs them), MOUNT3 mounting the longest
  path the server will give out and looking the rest up (a NAS
  exports a volume; the folder named below it is walked), FSINFO for
  the read size, READDIRPLUS with READDIR and LOOKUP where a server
  has no PLUS, READ by offset so a seek is the next read. An export
  marked `secure` — the usual default — takes mounts only from ports
  below 1024: the plugin takes a dialer from the distribution, and the
  firmware's binds a reserved port, which the chip may; a desktop
  without the privilege connects the ordinary way and such an export
  says "access denied". The Network page lists exports beside the
  media servers and SMB shares. The host tool's `--share` takes
  `nfs://` too. Measured on the desk against a userspace server
  (unfs3): list, reads at the head, the middle, the tail and the end;
  a FLAC opened from 30 s lands to the second. On the device, a
  Synology's export mounted from the folder named below the volume.

- SMB1 for the old box in the kitchen, by name only. A share written
  `smb1://host/share` is spoken in NT LM 0.12 (Unicode, 32-bit
  status, NTLMv2 under SPNEGO where the server has extended security
  and in the session setup's own fields where it has not, the MAC
  signature when the server requires it; TRANS2 FIND for the folders,
  gathered across however many replies a small server splits them
  into; READ_ANDX by offset, large reads where the server allows
  them). An `smb://` share never falls back to it: a server that
  closes on the SMB2 negotiate is reported as "an SMB1-only box? name
  it smb1://", and an `smb1://` share on a server without the old
  dialect is told so. Measured on the desk against a Samba speaking
  only NT1: guest and credentialed shares list and play, a FLAC
  opened from 30 s lands to the second.

- SMB shares in the library. A new source plugin,
  `plugins/org.evoframework.source.smb`, lists the shares the card
  names and reads their files, with a client for SMB 2.0.2 and 2.1
  written here in plain `std::net`: NTLMv2 under SPNEGO with the MIC
  over the exchange and the mechanism list, HMAC-SHA256 signing when
  the server requires it (and its replies verified), credits, the
  interim notices a slow server sends; nothing linked, the same code
  on the device and the host. SMB1 is never spoken on an `smb://`
  share, so an old box cannot be reached by a downgrade — `smb1://` is
  its own name (above), as `nfs://` is. The SDK gains the shape a share's
  file reads as (`ByteFile`: a length and reads by position) and the
  opener a distribution hands the decoder per scheme (`FileOpener`);
  the Symphonia plugin reads such a file through its read-ahead ring,
  and a seek costs nothing but the next read, since the protocol
  reads at an offset. A share's listing peeks each FLAC and WAV for its
  rate and length as the card's does, so the rate ceiling applies
  before a tap. The card's settings gain `share` (`smb://host/share/
  folder`, by name or address, `:port` where it is not 445),
  `share_user`, `share_password` (masked in `evostatus.txt`) and
  `share_domain`, and `share2` to `share9` with the same; a guest
  share needs no user. The Network page lists the shares beside the
  media servers found. The host tool gains `--share URL`,
  `--share-user`, `--share-password` and `--play-share`. Measured on
  the desk against Samba 4.17: a guest share, a share needing a user
  with signing mandatory, and a Volumio player's share each list and
  play; a FLAC opened from 20 s lands to the second; the
  specification's NTLMv2 vectors are unit tests, and a loopback
  server exercises the whole exchange in CI.

## [0.1.3] - 2026-10-21

- The network's media servers in the library. A new source plugin,
  `plugins/org.evoframework.source.dlna`, finds the DLNA (UPnP AV)
  media servers on the network and lists them as trees of folders and
  tracks: SSDP discovery on its own thread (a 2.5 s search every
  minute, every 15 s while none has answered), the device description
  read for the server's name and ContentDirectory control address,
  and `Browse` over SOAP for the folders, 200 objects a page, each
  item's title, artist, album, duration, art and the address of its
  first audio resource. It is written to the SDK's `source` contract
  like the files plugin — paths are `udn` or `udn|object-id` — so the
  decoder plays what it lists like a server's file (Stage 1): with
  the progress ring, seek, and the queue's modes. Plain Rust on
  `std`, nothing linked: the plugin runs on the device and the host
  alike. The firmware stocks a `dlna` shelf, and the library page's
  root is now the list of sources, Card and Network: Network lists
  the servers found, a server its folders, and the header carries the
  folder's name as the server listed it; the page asks again every
  2 s while the search is out and no server has answered. The host
  tool gains `--dlna NAME` (play the first track found walking that
  server's folders, folders called Music first) and `--iface ADDR`
  for a host with several interfaces. Measured on the desk against a
  MiniDLNA: three servers found in 2.5 s, a FLAC item opened from 20 s
  lands to the second; on the device, the three servers found 5 s
  after the Wi-Fi came up.

- Files on a server play and seek like the card's. A track reached
  over HTTP is a file when the server gives its length and does
  ranges (a DLNA server's item, a share behind a web server), and a
  station only when it does not: the Symphonia plugin reads such a
  file through the read-ahead ring, seeks it with `Range` requests
  (`netfile`: the connection is kept across seeks and serves a short
  hop forward by reading through; the file's first 64 KB and its last
  4 KB are kept, so the probe's look at the end and return to the start
  cost one short request, not two connections; a connection that ends
  early is taken up again at the place), and the tick waits on the ring
  rather than blocking the loop on the network. The ring keeps 128 KB
  behind the decoder for seeks back. `StreamOpener::open_at` is the
  contract (both HTTP openers honour it, accepting only a 206 for a
  range). A track now says whether it is live (`Track::live`) — a
  playlist entry with no length is, one with a length in seconds
  (`#EXTINF:243,` or `LengthN=`) is a file with that duration — and
  the face, the queue and the firmware go by that word rather than by
  the URI's scheme: a server's track gets the progress ring, seek and
  the queue's modes. The host tool gains `--start MS` (a seek at open)
  and a logger (`EVO_LOG=debug`); measured on the desk against nginx: a
  96 kHz FLAC, a 44.1 kHz FLAC and an MP3 each land on 25 s to the
  second, the plain open costing two requests and a seek one plus the
  decoder's bisection where the file has no seek table.

- HTTP Live Streaming. `plugins/org.evoframework.playback.symphonia`
  gains `hls` and `ts`: a station whose address answers with a
  playlist (by content type, or a `.m3u8` name) is read as one stream
  — a master playlist is followed to the audio variant nearest
  160 kb/s from below, the media playlist is walked from three
  segments behind the live edge and refreshed as the station goes on,
  each MPEG-TS segment is demuxed to its audio elementary stream (AAC
  in ADTS, or MPEG audio) and handed to the decoder as if the station
  were one long response, with the content type the segments carry
  and the variant's bitrate as `icy-br`. Segments in fragmented MP4
  (`#EXT-X-MAP`) are refused by name. The plugin wraps whatever
  transport the distribution gives it (`hls::Aware`), so the firmware
  and the host tool both play HLS with no change of their own. The
  shipped Radio list gains nine BBC national stations (1Xtra,
  6 Music, Asian Network, Radio 1 to 4, 4 Extra, 5 Live) beside the
  World Service, by their
  stable manifests on `a.files.bbci.co.uk`, 96 kb/s HE-AAC, which
  play anywhere; the Akamai pools they point into rotate, which is
  why the pools are not listed. The host tool plays the first track of
  the first folder when the directory given has none of its own (a
  directory holding one playlist plays that playlist's first station),
  and with Symphonia's AAC decoder (the `aac-symphonia` feature, the
  host's) the thread that opens a stream gets 256 KB of stack, since
  that decoder is built on the stack before it is boxed.
- Internal SRAM kept for the drivers. Three changes to where the
  player's memory comes from, so that Wi-Fi, lwIP and the DMA keep
  what they need while an MP3 plays: the allocator's medium band
  (512 B to 16 KB, the decoders' and the sink's working buffers) keeps
  a 32 KB floor of internal SRAM, and an allocation that would eat
  into it is placed in PSRAM first; the I2S channel is re-clocked in
  place on a change of sample rate (through the driver's channel API;
  the HAL's remake of the channel cost 33 KB of internal SRAM to
  fragmentation on every change, since the DMA ring came back in
  pieces); and Espressif's codecs take their working memory through
  the component's `media_lib_*` hooks, which the firmware now defines
  to follow the same floor — SRAM while the drivers' 32 KB would
  remain, PSRAM after (the MP3 decoder's 28 KB of state used to land
  whole in internal SRAM, outside any rule). Measured on the board
  with the card's queue: internal SRAM free during an MP3 track 14 KB
  → 35 KB, the largest free block 5 KB → 16 KB, no change to a 96 kHz
  FLAC track (46–48 KB); the MP3 decoder's share of the core 42 % →
  47 % with 4 KB of its state in SRAM and 23 KB in PSRAM (52 % with
  all of it in PSRAM). The heartbeat gains a `heap:` line: the largest
  free internal block, the medium band's split between SRAM and PSRAM,
  how many allocations the floor has diverted, and the codecs' split;
  each codec allocation is logged with its size and where it went.
- The core's headroom while a station plays. The face's cadence at
  rest is 100 ms rather than 40 ms (40 ms while a frame is being
  painted or for a second after a touch): each look reads the fabric's
  state, and at 40 ms that was 7–10 % of the core, now 4 %. A logo in
  PNG is decoded with a millisecond's pause every sixteen rows. The
  task watchdog watches the loop and the sink's feeder, which subscribe
  to it and reset it each turn, rather than the idle task: an HE-AAC
  station (the BBC's, 96 kb/s with SBR to 48 kHz) has Espressif's
  decoder at three quarters of the core — 104 KB of state, 90 KB of it
  in PSRAM whatever the placement rule — and a station's logo decodes
  beneath it on the art thread, so the idle task not running for five
  seconds is the schedule working, not a fault; it had fired at every
  BBC station's open. The loop not turning for five seconds is the
  fault, and is what is watched now. Measured on the board: HE-AAC main
  task 82 % → 77–79 %, idle 6–8 % → 10–11 %, real time throughout. The
  shipped list takes seven of the BBC's logos from TuneIn's 300-pixel
  PNGs over plain HTTP (the BBC's own are 600 pixels on transparency
  over TLS, 8–17 s to decode beside that decoder); Radio 4 and 4 Extra
  keep the BBC's, the only source verified for them.

## [0.1.2] - 2026-10-21

- The player as a Volumio device. `distributions/volumio`
  (`evo-esp32-volumio`) speaks what Volumio devices speak to each
  other, socket.io 2 over Engine.IO 3, on its HTTP long-polling
  transport in plain std: a server on port 3000 for the Volumio that
  finds the player (announced as `_Volumio._tcp` with `volumioName`,
  `UUID` and `type=device`), answering `getState` and
  `getMultiroomSyncOutput` and pushing both on every change, and a
  client session back to each Volumio that connects, so a leader's
  `enableMultiroomSyncOutput` and `disableMultiroomSyncOutput`
  broadcasts reach it; the firmware then points the Snapcast receiver
  at the leader's server, or back at the card's or the search's. The
  player appears in Volumio's Multiroom with its name, what plays, and
  no volume slider (the volume is the player's own). Its tile is the
  cover of what plays, served by the same server, or the evo tile
  when there is none. Payloads are read either way the reference
  implementations frame them; a peer's queue pushes are passed over
  unread. The player does not search for Volumio devices itself: a
  query for a service type one also advertises makes avahi (which
  Volumio runs) expire its record of it within two minutes (RFC 6762
  §10.5, passive observation of failure), measured on the bench.
  `CONFIG_LWIP_MAX_SOCKETS` goes to 16 for the held polls; the
  heartbeat gains the link's band, channel, signal and power-save
  state.

- The device as a client of a Snapcast server.
  `plugins/org.evoframework.snapcast.receiver` stocks the new
  `playback/snapcast` shelf: the binary protocol (`wire`), FLAC frames
  split and decoded one by one through Symphonia (`flac`), a thread
  that dials the server named, says hello as the device's MAC and
  hostname, probes the clock ten times quickly and then every second
  (median of the last fifteen), takes the codec header and the chunks
  into one ring in PSRAM sized for the server's buffer, and dials again
  with a lengthening pause when the server goes. The warden plays each
  chunk at the server's time against what the sink holds, pads the
  server's silences, drops or doubles a frame in fifty while the clocks
  lean, trims a chunk well behind, drops one 200 ms late, and applies
  the server's volume (squared) and mute. The firmware names the server
  from the card's new `snapserver` key or finds one by mDNS
  (`_snapcast._tcp`, asked again every thirty seconds until one answers,
  and again if a found one goes); the hand-over of the sink now serves
  two network sources (the Evo room first), the face says *Snapcast*,
  the heartbeat reports the session. `platform/esp-idf::mdns` gains a
  polled `Search`. The SDK gains the shelf.
- The sink's DMA ring is sized once for 96 kHz and the same at every
  rate (8 buffers of 1023 frames), so a reopen at a higher rate never
  asks internal SRAM for more than it gave back: the card's 96 kHz
  track resuming after a stream no longer fails with `ESP_ERR_NO_MEM`
  and restarts the player. The sink keeps account of what the DMA
  holds (frames handed over, less frames played at the rate from the
  feed that found the ring empty) and reports it with the reservoir in
  `held()`, so a receiver's schedule includes the ring; the account is
  good to one buffer, 21 ms at 48 kHz, from the start of a stream.
- Wi-Fi power save is off from the moment the driver starts (a
  mains-powered player; with it on, a probe's round trip wandered to
  1.9 s and a ping averaged 290 ms; off, a ping is 2 to 9 ms), and the
  TCP receive window is 32 KB (was four segments, 5.7 KB, which capped
  a stream at a few tens of kilobytes a second under such round trips);
  the segments wait in PSRAM.
- The network and the hand-over of the sink run on their own cadence,
  no longer inside the paint block a decoder short of lead skips: a
  96 kHz track at the core's limit no longer blinds the player to the
  network.
- Both receivers read the last frame's stamp before the clock when
  judging silence; the other order, with the client's thread storing a
  newer stamp between the two, read as a wrap and ended a custody with
  "stopped sending" while chunks flowed.
- The arrival stamp on a Snapcast message is taken once its header is
  in, not before the wait for it.
- 96 kHz FLAC from the card plays at real time with the network up
  again. The decoder read the file itself: a card on a serial bus hands
  blocks over as DMA transfers the caller waits out, and a destination
  in PSRAM (Symphonia's 64 KB stream buffer) is not DMA-capable on the
  C5, so the driver read it a block at a time through a bounce buffer;
  measured by the kernel's run-time counters, that cost a third of the
  core and the decoder 6% more waiting. The card is now read ahead on
  a thread of its own (`playback.symphonia::ahead`) into a 256 KB ring
  in bulk memory, in aligned 8 KB whole-block reads the driver takes
  straight: the reader costs 7%, the decoder 67%, and 14% of the core
  is idle where there was none; a seek within the ring is taken from
  it. The loop also no longer sleeps a millisecond after every tick
  while the decoder is short of lead (that was 7% of the core); the
  idle task gets its turn once a quarter second instead. The heartbeat
  gains a `core:` line, each task's share of the core since the last
  beat, with its priority.
- The status strip says when the link is one a stream will feel:
  *2.4 GHz Wi-Fi* on that band, *weak Wi-Fi* below -75 dBm on either,
  between the source and the stream so a narrow layout cuts the
  stream's detail rather than the word. The `Wi-Fi` row of Settings
  and the status file carry the signal with the band. Measured against
  one access point with power save off: round trips of 3-9 ms on 5 GHz
  and 25-125 ms on 2.4 GHz; a Snapcast stream clean on both, its clock
  holding to 0.6 ms on 5 GHz and wandering by 4 ms on 2.4 GHz.

## [0.1.1] - 2026-10-20

- The device as a room of an Evo multiroom group.
  `plugins/org.evoframework.multiroom.receiver` stocks the new
  `playback/receive` shelf: a listener on the Evo audio plane (TCP
  7331) from load, the handshake offering the plane's `bin1` frame
  format, sync probes every five seconds, frames queued with their
  arrival time (`bin1`, or an older source-host's JSON envelope decoded
  the same), and a warden that plays them 200 ms behind their stamp,
  anchored on the first frame's arrival as the Linux receiver does. The
  SDK gains the shelf, the `Receive` instruction, and the firmware a
  shared sink: the loop grants it to the decoder or the room, releases
  the queue when a source-host sends, resumes it when the room ends,
  and gives it back on Next or Previous (that connection is then not
  granted again). `platform/esp-idf` gains `mdns` (Espressif's
  responder, task and records in PSRAM): `evo-player.local` and the
  `_evo._tcp` service with the device's id, derived from the chip's
  MAC so it survives reflashes, first digit `f` so the plane's election
  never makes a receiver the source-host. The face shows the room; the
  `Evo id` row at the bottom of Settings shows the id a group adds.
  Bench: a stand-in source-host (`scripts/bench-source.py`) streamed
  25 s of `bin1` frames; four probes answered, the sink took the PCM,
  the queue resumed at the goodbye; about 4.5 KB of internal SRAM at
  idle for the listener and the announcement.
- A network stream is reached on a thread of its own. `take_custody`
  returns at once with the new `Status::Opening` ("connecting" on the
  face); the thread connects, buffers and probes (32 KB stack, in
  PSRAM); the tick that finds it done starts the sink, and one that
  finds it failed ends the custody with the reason, which now rides the
  `custody_released` happening (`Progress::Failed(reason)`). The loop
  that reads the touch and paints is never held for a connect. A pause
  while opening starts the stream held; a file opens on the spot. The
  firmware holds a station that could not be reached with the reason
  on the strip, as before.
- Fixed: the sink's feeder thread, when spawned, reset the main task's
  thread configuration to the default, so every thread a plugin
  spawned after it (the stream fetcher, the open thread, the room's
  listener) got its stack in internal SRAM, and children of those
  threads did not inherit the PSRAM placement either. The feeder's
  spawn gives the caller's configuration back and the main task's
  configuration is inherited down the tree; with a 44.1 kHz MP3
  playing, internal free went from 27 KB to 40 KB on the same build.

- AAC on the board, through Espressif's decoder. `platform/esp-idf`
  gains `aac`: `EspAac` wraps `esp_aac_dec` (AAC-LC and HE-AAC, the
  sample rate following the decoder's word, so an HE-AAC stream's
  doubled rate reaches the sink), rebuilding the ADTS header Symphonia's
  demuxer strips before each packet. The Symphonia plugin's `aac`
  feature is now the demuxer and content-type logic alone;
  `aac-symphonia` (default, host) adds Symphonia's decoder, and the
  firmware registers Espressif's instead, as it does for MP3. A stream
  whose decoded format differs from the demuxer's word (HE-AAC) restarts
  the sink at the new format with the position carried over. Measured
  on the bench: an AAC station takes about 54 KB of internal SRAM to
  open and 37% of the core (MP3: 5 KB and 28%). The `.aac` greying and
  the refusal go. `sdcard/Radio/Evo Selection.m3u` is now alphabetical
  and 47 stations: Absolute (four, AAC-LC 128k), talkSPORT, Times Radio,
  Virgin Radio UK, Capital Chill, KEXP and Radio Swiss Classic/Jazz
  (HE-AAC); Planet Rock, Kerrang!, Greatest Hits, Hits Radio, Magic and
  Magic Soul on their 112k MP3 feeds (Bauer's streams want the
  directory's `aw_0_1st.skey` parameter, without it they answer 500);
  and the BBC World Service, the one BBC station with a plain stream.
- Shuffle, repeat, and what plays at power-on. `distributions/queue`
  (`evo-esp32-queue`, no_std): the queue a distribution's loop owns,
  items in the given order with a play order over them, shuffled or
  not, repeat all (a fresh deal each lap when shuffled), one (a track
  that ends on its own plays again; the listener's next moves on) or
  off; previous and next wrap under all; a tap on a track makes its
  folder or playlist the queue from that track; turning shuffle on
  deals the tracks not yet played and off restores the order with the
  hand on the same track; seeded by the distribution (the firmware logs
  the seed). `evo-esp32-config` gains `shuffle`, `repeat` and `start`.
  Firmware: the loop's `tracks`/`next` pair gives way to the queue;
  Settings gains `Shuffle ›` and `Repeat ›`, written back to the card;
  `start = <path>` names what plays at power-on (a folder, a file, a
  playlist with `#Station`); a station named there waits for the
  network and plays when it is up, "waiting for the network" on the
  strip meanwhile; a `start` that is not on the card is reported in
  `evostatus.txt` and the card plays. Defaults: shuffle on, repeat all,
  the whole card. On the face: shuffle and repeat icons flank the
  transport (portrait) or sit on a row under it (landscape), lit in the
  accent when on, a `1` for repeat-one; one tap each, written back to
  the card; shown for files, not for a station. The layout's previous
  and next move in to make room in portrait.
## [0.1.0] - 2026-10-20

- PNG logos decoded a row at a time and shrunk on the way to twice the
  art square (`png::decode(png, under, target)`): a 981-pixel logo had
  cost six megabytes of PSRAM and eight seconds decoded whole. An
  interlaced PNG is decoded whole up to 512 a side and refused beyond.
  The heartbeat's companion `memory:` lines report PSRAM free and
  largest block as well as internal.

The first release: the player on the Waveshare ESP32-C5-Touch-LCD-2.8, flashable from the GitHub release, built there from the tag (`.github/workflows/release.yml`; a manual run files a draft). `.github/FUNDING.yml`.

- Radio, from the second bench. The ticker is gone: a tap on the text
  block of Now Playing raises a details sheet, a card over the middle
  with the whole title, artist, station and format wrapped and centred;
  a tap anywhere puts it down, the navigation works through it, a
  track change puts it down. Nothing on the face moves on its own.
  Playlists are folders of their own in the library (Radio › Evo
  Selection › …), their stations in the playlist's order; the queue
  after a tap is that playlist. A stream the build cannot decode is
  refused by the server's `Content-Type` before a byte is probed (AAC
  without the feature, playlists, HTML), and a live stream's probe is
  capped at 96 KB — the first bench hung the player for 58 s while
  Symphonia read a megabyte of an AAC stream looking for a format. A
  dropped connection is opened again by the fetch thread, three times
  with a doubling pause, the metadata interval re-read, before the
  station is given up (`StreamOpener` is now `Send + Sync`, shared with
  that thread); a station that ends for good holds with "the stream
  ended" on the strip, Play tries it again — a file's end still moves
  on. mbedTLS's AES is software now: the hardware path copies a record
  held in PSRAM through DMA-capable internal buffers it allocates per
  call, and when internal SRAM was briefly short (a logo arriving as a
  station started) the allocation failed, the TLS read with it, and the
  station died ten seconds in (`esp-aes: Failed to allocate memory`,
  twice in the bench). Measured: a station's open costs 5 KB of
  internal SRAM; with an MP3 station playing 17 KB are free, the MP3
  decoder's buffers the bulk. The host gains `--sheet`. The sample
  playlist for the bench (`target/`, not shipped) names an AAC stream
  and a dead host on purpose.
- Station logos, a name card, and a ticker. `sdk`: `Track` gains `art`,
  a picture's URI where the source knows one. `source.files`: an M3U's
  `tvg-logo="…"` (the IPTV attribute) names a station's picture, a URL
  or a path on the card relative to the playlist. `platform/esp-idf`
  (`png.rs`): PNG through the `png` crate, transparency composed onto
  the face's background. The firmware's art fetcher takes three
  sources (the track's picture on the card, on the web through the
  HTTP opener, or the folder's cover as before); a logo is set whole on
  a square of the background, a cover cropped to its centre; a theme
  change recomposes it. `face`: a station without a logo shows its
  name in the art square (disc label or card tile, up to three lines);
  a text line too long for its area scrolled as a loop (replaced by
  the details sheet in the entry above, after the bench); the status
  strip can carry a `notice`. `sdcard/Radio/Evo Selection.m3u` is now
  a UK selection (29 stations, with Radio Paradise's FLAC streams)
  drawn from radio-browser.info, every stream and logo checked.
- Flash: 32 MB declared (the C5's MMU reaches all of it), one 8 MB app
  partition (`partitions.csv`); the app was 3.02 MB of its 3 MB.
- Radio, from the first bench. A station that will not open stays on
  the face with the reason on the status strip ("could not connect",
  "not found (404)", "format not supported here") instead of the
  player moving on to the next station unasked; a file that will not
  open is still passed over. AAC is left out of the board's decoder
  set (a feature of the playback plugin the firmware disables; the
  host keeps it): Symphonia's AAC decoder is built on the stack and
  overflowed the main task's 32 KB at the first AAC station (a stack
  protection fault); a station whose address ends in `.aac` is greyed
  in the library. Threads the plugins spawn from the main task (a
  stream's fetcher) get their stacks in PSRAM; the HTTP client's send
  buffer is 1 KB. Internal SRAM with a station playing: about 22 KB
  free before the stack move, measured again next. The previous
  track's folder art is dropped when a stream starts.
- Internet radio. A playlist on the card (`.m3u`, `.m3u8`, `.pls`) lists
  stations; they appear in the library under the playlist's name and a
  tap plays one over Wi-Fi. `sdk`: `ByteStream` and `StreamOpener`, the
  contract by which a distribution opens `http://` and `https://` for
  the playback plugin. `plugins/org.evoframework.source.files`: reads
  playlists (`#EXTINF` titles, `FileN`/`TitleN` pairs, a BOM, a station
  without a title named by its host) into tracks with no length and the
  playlist as their album. `plugins/org.evoframework.playback.symphonia`
  (`stream.rs`): a fetch thread reads the stream into a 512 KB ring in
  PSRAM, gathering a second and a half at the server's bitrate before
  the first packet; the decoder's tick never waits on the network. The
  server's interleaved metadata (`icy-metaint`) is taken out on the way
  in and its `StreamTitle` becomes the track's artist and title as the
  songs change; `icy-name` is the album line. `platform/esp-idf`
  (`http.rs`): the opener over `esp_http_client` with the certificate
  bundle, redirects followed, a 10 s read timeout; `mbedTLS` allocates
  in PSRAM. The host distribution opens plain `http://` over a socket
  (SHOUTcast's `ICY 200 OK` accepted). Stations stay out of the boot's
  run through the card. The face drops the previous track's art when a
  stream starts. `sdcard/Radio/Evo Selection.m3u`: thirty-two stations
  to begin with. Decoded: MP3, FLAC, Ogg Vorbis (AAC on the host only;
  see the entry above). The app image was 3.02 MB of its 3 MB
  partition.
- The reservoir. `firmware/waveshare-esp32-c5-touch-lcd-28/src/sink.rs`:
  the decoder runs ahead of the ear into a 600 ms ring of decoded audio
  in PSRAM, and a feeder task at the top priority (above the Wi-Fi
  driver) carries it, scaled by the volume, into a DMA ring now sized
  only to outlast a stall of the chip (80 ms, 16 to 32 KB of SRAM in
  place of 45 to 65). Anything that holds the core for hundreds of
  milliseconds (the join's key exchange, a slow packet, a full repaint)
  no longer reaches the ear: measured on the board with a 96 kHz FLAC
  as the first track, the hinted join with its SAE exchange landing on
  it and the first frame painting, no dry spell; the same through a
  full 14 s search. A pause stops the feeder within the DMA ring (70
  ms, was up to 230) and what is held waits; a seek drops what is held
  and is not primed; a stream into an empty reservoir gathers 250 ms
  first (the whole lead at the first start after boot, when no one is
  waiting), one following another's tail starts at once; a rate change
  drains the tail, then the feeder remakes the channel. The dry
  detector moves to the feeder and counts only what the DMA ring could
  not cover. The loop puts audio first: below 120 ms of lead it decodes
  instead of painting, which at a 96 kHz boot lights the panel about a
  second later. A decode tick is bounded to 50 ms of audio so the loop
  reads the touch often. `sdk`: `FrameSink` gains `pause`, `resume` and
  `held` with defaults; `plugins/org.evoframework.playback.symphonia`
  pauses the sink and publishes the position being heard (what it
  wrote, less what the sink holds). Internal SRAM free at 96 kHz with
  the network up: 82 KB (was 58).
- Wi-Fi. `firmware/waveshare-esp32-c5-touch-lcd-28`: with `wifi_ssid` in
  `evoconfig.txt` the player joins the network on a thread of its own
  (`net.rs`), through the companion's `WifiNet::join`: a network seen
  before in about three seconds, a new one by a search of both bands in
  about fourteen; the music starts without waiting. `name` is the
  hostname the router is told. Settings gains a `Wi-Fi` row (address
  and band, or why not), `evostatus.txt` a `wifi:` line rewritten as the
  link comes and goes; a dropped link is rejoined. The companion is
  pinned at the commit that lifts the join into `WifiNet`.
- Internal SRAM. The Wi-Fi driver costs the ESP32-C5 about 110 KB of its
  one SRAM, half of it code it keeps resident; naively linked it left 3
  KB free. The firmware now: runs the driver's code from flash
  (`sdkconfig.defaults`, the IRAM optimisations off; a music stream
  does not need the throughput), puts the driver's and LwIP's pools in
  PSRAM (PSRAM is therefore required; the board has 8 MB), keeps few
  static buffers, places its own heap by size (`alloc.rs`: records and
  text in PSRAM, the decoder's and the sink's working buffers in SRAM,
  the stream's ring and the art in PSRAM; the card's listing alone had
  taken 60 KB of SRAM as small strings), moves the art fetcher's stack
  to PSRAM, and trims the main stack to 32 KB (a day's low-water mark
  was 40 KB of 48). The network thread starts after the DMA ring has
  its memory and runs below the main task. `memory:` lines at boot say
  what is left at each step; about 58 KB with a 96 kHz stream and the
  network up, 170 KB without the network. The decoder's share of the
  core is unchanged (measured on the same track with and without).
  Known: on a WPA3 network the join's SAE exchange can run the DMA dry
  once, for tens of milliseconds, if a 96 kHz stream is playing at that
  moment.
- `evoconfig.txt`. `distributions/config` (`evo-esp32-config`, `no_std`):
  the card file's grammar (one `key = value` a line, `#`/`;` comment
  lines, quotes keep spaces, an empty value is the default, a BOM and
  CRLF tolerated, a bad line reported and the rest applied), the typed
  `Settings` (`theme`, `style`, `rotation`, `volume`, `name`, `wifi_ssid`,
  `wifi_password`, and `dac`, `output` reserved for the register of DAC
  boards), `set` to rewrite one key in the file's text in place, and the
  status report's lines (the password shown as its length). The template
  is `sdcard/evoconfig.txt`. `firmware/waveshare-esp32-c5-touch-lcd-28`:
  the file is read after the card mounts; rotation goes to the panel's
  bring-up, theme and style to the face, the level to the sink;
  `evostatus.txt` is written with the build, what runs, what was read
  and what was not understood; a tap on `Theme ›` or `Style ›` in
  Settings changes the face and writes the key back. The keys the file
  may carry that this firmware does not act on yet are read and said so
  in the status. `Rotation ›` in Settings writes the next quarter turn
  and restarts the player, which comes up turned (the panel is oriented
  at bring-up). A style change refetches the album art at the new
  square's size (the card's is 180 px in portrait, the disc's 140).
- `distributions/face`: a drag on the library scrolls. The finger's
  travel arrives in small steps; it now accumulates until a row's worth
  is in, instead of each step being rounded to no rows.
- `distributions/face`: Settings scrolls as the library does (landscape
  shows five of its seven rows), and a tapped row is the fact under the
  finger wherever the list sits. The landscape card's volume is the
  full-width row under the art where the seek was, the seek and the
  times a row below; where the two rows' touch reaches meet, the nearer
  bar takes the finger. The portrait card's text band is a scrim: the
  picture shows through, dimmed, under the title, artist and album,
  instead of a solid band over its lower third; the band's text is
  painted with the art, so a cover arriving mid-track no longer wipes
  it. The host gallery renders a made-up cover in every frame.
- `plugins/org.evoframework.playback.symphonia`: a stream that is lost
  mid-decode is logged with the decoder's reason before custody is
  released; until now the reason went only into the track's title.
- `firmware/waveshare-esp32-c5-touch-lcd-28`: the boot tone is retired.
  Half a second of 1 kHz went straight to the I2S channel at boot as the
  bring-up's self-test, past the sink's level, so it played at 30% of
  full scale whatever the volume: the "deafening tone" at every start.
  The path it proved is proven by the music now.
- The rate ceiling is known before a track is opened. `sdk`: `Track`
  gains `sample_rate`, where the source read it. `plugins/org.evoframework.source.files`:
  a listing reads each FLAC's STREAMINFO (rate and length) and each
  WAV's `fmt ` chunk from the file's first 64 bytes. `firmware`: a track
  above the ceiling is passed over unopened (no custody taken, no face
  flicker), and the library greys such rows; a tap on one does nothing.
  An all-192 kHz album tapped in the library used to march through
  every track in silence, 1.8 s apiece.
- The Library and Settings screens behind the navigation. `distributions/face`:
  `Screen` (Now Playing, Library, Settings), `library::{Library, Entry,
  About, Fact}`; a list page is a header with a back chevron below the
  root, rows (a folder glyph or a note, the name; the track in hand in
  the accent), and a bar at the foot with the art's thumbnail, the title
  and play/pause. Painted in six stages; a scroll repaints the rows, a
  change of the track in hand the bar and the rows. `Face::hit` tells a
  touch apart per screen (`Hotspot::{Back, Row, List, BarPlay, Bar}`);
  `touch::Tracker` takes the hit function and turns a press on the list
  that travels up or down into `Intent::Scroll`. The navigation shows the
  screen in front. The host gallery renders both pages per rotation; the
  Now Playing frames are unchanged. `firmware/waveshare-esp32-c5-touch-lcd-28`:
  the list icon opens the card's root as the source lists it; a folder
  row opens the folder (the chevron goes back); a track row makes the
  folder the queue from that track on; the bar's button pauses and
  resumes; the gear shows the player's facts (version, build, board,
  theme, style, rotation, card).
- Album art. `distributions/face`: `art::Art` (a square of RGB565 at the
  art area's size) and `art::fit` (centre crop, box-filter scale, integer
  arithmetic); `Face::set_art` shows or drops a picture with a repaint of
  the art alone (`Repaint::Art`); the picture is drawn through the disc's
  circle or the card's rounded corners with the same anti-aliased edge
  as the stand-ins, one window a row, touching only the pixels the shape
  covers so the ring through the square's corners stays. The stand-ins
  remain when there is no picture. `platform/esp-idf`: `jpeg`, Espressif's
  `esp_jpeg` component (TJpgDec, Apache-2.0, in ROM on the C5) decoding
  at the largest of 1/2, 1/4 and 1/8 that keeps the picture at least the
  art's size; baseline JPEG only. `firmware/waveshare-esp32-c5-touch-lcd-28`:
  `art::Fetcher`, one worker thread below the main task's priority (the
  main task is raised to 2, the worker runs at 1), which on a change of
  album folder looks for `cover.jpg`, `folder.jpg`, `albumart.jpg` or
  `front.jpg`, reads it in 4 KB pieces with a 2 ms breath between (the
  card shares its SPI bus with the music), decodes, fits and hands the
  result back through a channel; the newest request wins, and the next
  track from the same folder keeps the art. Measured: an 87 KB, 600 px
  cover read in 560 ms, decoded at 1/4 in 594 ms, fitted in 26 ms, on
  the face 1.4 s into the track, with 96 kHz FLAC playing at half the
  core and no dry spell; a 1.7 MB, 1500 px cover at 1/8 in under a
  second. Internal SRAM free settles at 47 KB (was 76 KB): the worker's
  stack and the decoder's tables; the stack can move to PSRAM if that
  is ever needed.
- `plugins/org.evoframework.playback.symphonia`: ID3 tags are read. The
  crate was built without Symphonia's `id3v2` and `id3v1` readers, so the
  probe scanned every tagged MP3 from byte 0 for a frame sync; the
  UTF-16 byte-order marks inside an ID3v2 text frame (`FF FE`) parse as
  MPEG headers, and in two Queen files one of them held up as a Layer I
  frame, so the track was declared MP1 and refused ("unsupported audio
  codec"); every other MP3 paid a junk scan and lost its Xing header
  ("estimating duration from bitrate"). With the readers on, the tag is
  parsed and the stream starts at its first frame.
- `plugins/org.evoframework.playback.symphonia`: the stream's tags name
  the track. The title from the tags replaces the one the source gave (a
  file's stem); artist and album fill in where the source had none, the
  album artist standing in for a missing artist. ID3v2 on MP3, Vorbis
  comments on FLAC. (A WAV's INFO list is parsed by Symphonia's reader
  and then dropped before it reaches the caller, in
  symphonia-format-riff 0.6.1; not ours to fix.) The face shows the
  three lines as tagged: "Las Palabras De Amor", not "08 - Las Palabras
  De Amor".
- `plugins/org.evoframework.playback.symphonia`: MPEG audio (MP1, MP2,
  MP3) is seeked coarsely. Symphonia's accurate seek walks every frame
  from the start (or the current position, forward) to the target, which
  off the card ran at about 25 times real time: 3-13 s for a tap on the
  ring. Coarse has the reader estimate the byte position from the time
  and the stream's length, resync on the next frame and walk the last
  stretch. Exact on a constant bit rate; on a variable one the landing is
  off by the stream's drift from its mean rate. Other formats stay
  accurate (FLAC seeks by its table in 0.3 s). On the host: forward,
  back and near the end of a 4-minute MP3, each landing on the packet
  before the target with up to 76 ms to drop. On the board: twenty seeks
  in a minute, forward and back across whole tracks, none long enough to
  run the ring dry (under 231 ms each); the position is moving again
  within 70 ms of the tap.
- `distributions/face`: a full frame is painted in stages, one per
  `refresh` (`Repaint::Partial` until the last returns `Repaint::Full`;
  `refresh_whole` for a host that wants the frame in one call), so the
  caller's loop runs between them. The placeholder disc and the progress
  ring are composed per pixel and sent one window a row instead of as a
  stack of shapes each with its own edge windows; the frames are pixel
  for pixel what they were (the gallery compared). On the board a window
  costs about 70 µs: the disc as twelve rings took 433 ms, composed 70 ms;
  the ring with its accent 149 ms, composed 73 ms. The first frame is
  421 ms end to end, the longest stage 73 ms.
- `firmware/waveshare-esp32-c5-touch-lcd-28`: the face's stages run on
  consecutive passes of the loop with a decoder tick between, and the
  panel lights on the first complete frame. A track start no longer runs
  the DMA dry (it did by 500 ms, the face's first frame on top of a ring
  of 159-231 ms); five track starts in a capture, none dry. The first
  frame's stage times are logged with "panel lit".
- `firmware/waveshare-esp32-c5-touch-lcd-28`: the empty-card tone is
  retired. The firmware no longer writes `tone.wav` to a card with no
  music (it stayed on the card once music arrived and came round in the
  shuffle as a honk); such a card runs the face, which says "Nothing
  playing", and answers touch. The half-second self-test at boot stays.
  A `tone.wav` an earlier build left on a card is the card owner's to
  remove; the firmware does not delete files from a card.
- `distributions/face`: touch. `touch::Tracker` turns the panel's reports
  into intents against the layout: a tap (under 800 ms, under 12 px of
  travel) on a button is `Intent::Tap`; a finger on the volume strip or
  the seek ring is `Intent::Slide` with the fraction as it moves and
  `Intent::Commit` on lift, sticky to the slider it started on.
  `Layout::map_touch` takes the CST3530's native portrait frame to the
  layout's frame at each rotation. A status change alone (pause, resume)
  repaints only the transport (`Repaint::Transport`), not the frame.
- `firmware/waveshare-esp32-c5-touch-lcd-28`: the face is live. Taps on
  play pause and resume (or start the next track when nothing holds
  custody), next and previous skip, the volume follows the finger on the
  strip, and a lift on the ring seeks. The sink applies the level as a
  Q15 gain on the house curve (1..=100 as -49.5..0 dB in half-decibel
  steps), so the speaker and a HAT hear the same level; it starts at 40%
  instead of full. Measured on the board: pause and resume, skip, a
  slide from 29% to 57%, and a seek on 96 kHz FLAC back playing 0.3 s
  later; a seek on MP3 takes 3-13 s in proportion to the distance, since
  Symphonia's demuxer walks the frames to the target at about 25 times
  real time off the card (open).
- `firmware/waveshare-esp32-c5-touch-lcd-28`: the sink names each dry
  spell as it sees it (`sink: the DMA ran dry for N ms`, at the write
  that ends it), so a capture places it in time; the ten-second report
  stays as the tally. A gap the firmware announces (`sink::expect_gap`,
  before a pause and before a start from idle) is not a dry spell.
- `firmware/waveshare-esp32-c5-touch-lcd-28`: the build id. `build.rs`
  stamps `git describe` into the binary, the boot log's first line names
  it (`build:<describe>`), and `scripts/image.sh` refuses to call an image
  good unless it carries the id of the tree it is run in. Two bench runs
  on 19 October flashed a stale image; this closes that class.
- `distributions/face` (`evo-esp32-face`) replaces `distributions/panel`:
  the player's own face instead of the companion's remote-control pages.
  Now Playing from the playback and delivery projections, in the evo
  family's themes (evo-ui's `styles.css` tokens, OKLCH converted once to
  RGB565: Evo Default, Night Sky, Sunrise, Air, Liquid), in two styles (the disc,
  with the progress on a ring round the art; the card, with the text on a
  band over the art's foot) and at rotations 0, 90, 180 and 270 (portrait
  and landscape layouts; the display driver turns the pixels). Inter
  (OFL 1.1, `assets/fonts`) is rasterised by `build.rs` into a 4-bit glyph
  atlas at the face's five sizes; text and every shape (discs, rings, the
  arc, rounded corners, the triangles of the transport glyphs) are drawn
  as rows with anti-aliased edges blended against the colour under them,
  so no frame buffer is read back. A layout test proves no region overlaps
  another at any rotation or style; `Layout::hit` maps a point to the
  play, previous and next buttons, the seek strip, the navigation and the
  art, for touch. `no_std`. The host gains `--style`, `--theme` and
  `--gallery DIR` (every rotation, style and theme as PNGs); the firmware
  draws the disc in Evo Default. On the board: full frame 1, then 5% of
  the time drawing, internal SRAM 49 KB free.
- `firmware/waveshare-esp32-c5-touch-lcd-28`: a track whose stream rate
  is above the chip's ceiling (96 kHz; 192 kHz FLAC decoded at 0.4 of
  real time) is passed over as soon as the decoder reports its format.
- `scripts/image.sh`: removes the crate's previous image before writing
  and finds esptool as a module, on PATH or in ESP-IDF's python_env.

- `platform/esp-idf` (`evo-esp32-platform-esp-idf`): Espressif's fixed-point
  MP3 decoder (`esp_audio_codec` 2.6.2, fetched by the ESP-IDF component
  manager; Espressif Modified MIT, for Espressif chips only) as a Symphonia
  `AudioDecoder`, registered at the preferred tier so the demuxer, seeking
  and the position stay Symphonia's. The ESP32-C5 has no FPU: Symphonia's
  floating-point MP3 reached 0.555 of real time with the core saturated;
  Espressif's plays at 1.000 with the core 23-25% busy and the face on.
  Ear: "perfect".
- `org.evoframework.playback.symphonia`: `default_codecs()` and
  `PlaybackSymphonia::with_codecs`, so a distribution can prefer a platform
  decoder for a codec; `new` keeps Symphonia's own.
- `firmware/waveshare-esp32-c5-touch-lcd-28`: the Espressif decoder in the
  registry; a track the decoder refuses (a codec nobody stocks, an
  unreadable file) is logged and passed over instead of ending the run;
  the companion dependencies move to `ccc5d41`, whose board crate waits
  for the touch controller to answer after its reset: the codec written
  to in that window got an I2C timeout (20 ms on the bench, 10 ms was
  waited). Measured: 192 kHz/24-bit FLAC decodes at 0.38 of real time;
  above 96 kHz is beyond this chip.
- `firmware/waveshare-esp32-c5-touch-lcd-28`: the fabric on the board.
  The card mounted through the companion's platform crate; the catalogue
  compiled in by `build.rs` and checked against the plugins at start; the
  steward with the files source, the Symphonia decoder and the null
  output; the tracks at the card's root played once each through
  `sink::I2sSink`, I2S0 with its DMA as the buffer: the decoder offers
  frames, the sink takes what fits with a zero timeout and says how many,
  a frame cut by the DMA boundary carried into the next call, mono
  doubled into both slots, the channel rebuilt and the ES8311 told when a
  stream's rate differs. A card with no music gets a two-second `tone.wav`
  first. Half a second of 1 kHz at boot as the self-test; an ES9018K2M on
  the HAT's I2C brought up when one answers. Every happening logged; a
  heartbeat every ten seconds with the heap and the main stack's
  low-water mark. Bench: 2 s of WAV from the card decoded and played in
  2.07 s, custody taken and released, no watchdog, 45 KB of the main
  task's 48 KB stack never touched. Found on the way: ESP-IDF's `usleep()`
  spins below one scheduler tick, so at 100 Hz the idle task starved once
  the fabric went quiet; `CONFIG_FREERTOS_HZ=1000`. A full DMA is the
  HAL's `ESP_ERR_TIMEOUT` with nothing taken, which is the sink
  contract's "ask again", not a warning.
- `firmware/waveshare-esp32-c5-touch-lcd-28`: the whole card, and real
  FLAC. The listing walks folders depth first, one request to the files
  source per folder; FatFs hands names to Rust as UTF-8. The I2S ring is
  sized in time, a quarter second at the stream's rate within 64 KB of
  SRAM (11 buffers at 44.1 kHz, 16 at 96 kHz), after a 96 kHz/24-bit
  album came out choppy with the core 60% idle: six buffers were 53 ms
  of cover and the decoder's cycle (20 ms idle plus one packet) was
  longer. `opt-level = 3`. The heartbeat reports the core's busy share,
  internal SRAM free, the playback position against the clock, and dry
  spells the sink certainly had. Bench: 96 kHz FLAC at 1.000 of real
  time, 37-58% busy, no dry spell, 54 KB internal free.
- `distributions/panel` (`evo-esp32-panel`): the panel as a consumer,
  shared by the host and the firmware: the playback and delivery
  projections mapped to the companion's `PlayerState`, the resting face
  drawn on any RGB565 target, repainting the whole face only on a scene
  change, the text block when the ticker moves, the seek strip when the
  position has moved a second. The host's `panel.rs` is the PNG wrapper
  over it. 2 tests.
- `firmware/waveshare-esp32-c5-touch-lcd-28`: the panel lit. The board
  comes up through the companion's new `boards/` crate (expander, ST7789,
  CST3530, backlight; the I2C bus handed out for the codec and a HAT's
  DAC), the card read before the panel as the companion does; the shared
  face drawn every 40 ms from the projections, the backlight after the
  first frame. The tracks are shuffled, so a bench run meets every format
  on the card early. The heartbeat adds the drawing share and the count
  of full frames. Every companion dependency at one commit. Measured: the
  face costs 1-2% of the core; MP3 decodes at 0.555 of real time with the
  core 98% busy (Symphonia's MP3 is floating point and the C5 has no
  FPU), so MP3 is not playable on this chip as it stands.
- `scripts/image.sh` takes the newest esp-idf-sys build under `target/`
  when an sdkconfig change has left two.

- `firmware/waveshare-esp32-c5-touch-lcd-28`: the first firmware, and the
  first sound. std Rust on ESP-IDF v5.5.5 (the companion's pin and build
  files); the IO expander and the ES8311 through the companion's drivers
  by its tag v0.1.0; I2S0 as master in Philips format, 16-bit stereo, on
  the vendor BSP's pins (BCLK GPIO24, WS GPIO25, data GPIO26, no MCLK);
  a 1 kHz tone for two seconds every ten through the board's speaker,
  each logged with the time the write took. Bench: chip ID `83 11 01`,
  352 800 bytes in 1 972 ms against 2 000 expected. Found on the way: the
  C5 has no FPU, and 88 200 software sines took eight seconds and tripped
  the idle task's watchdog; the tone is one exact cycle, tiled. The three
  I2S signals are also mirrored through the GPIO matrix onto header pins
  a Pi-HAT DAC can reach (BCLK GPIO2, WS GPIO3, data GPIO11), with the
  console moved to the USB Serial/JTAG port so J6 is free; the speaker
  and the HAT play the same frames.
- `scripts/image.sh` from the companion, naming the image by the crate;
  `scripts/check.sh` builds every firmware crate with clippy and drops an
  esp-idf-sys CMake cache made against another ESP-IDF version; CI gains
  the companion's toolchain steps and uploads the image.

- `evo-esp32-sdk`: `contract::audio::playback::FrameSink`, where decoded
  frames go: the distribution owns the sink and hands it to the decoder;
  `start`, non-blocking `write` that says how much it took, `flush`,
  `stop`. Frames do not travel through the fabric's values.
- `org.evoframework.playback.symphonia` (Apache-2.0, std): the real
  decoder behind `playback/decode`, FLAC, MP3, WAV and PCM through
  Symphonia 0.6; a bounded slice per tick, the sink pacing; position from
  frames the sink accepted; seek to the frame, trimming after the
  reader's packet boundary; duration from the container. 6 tests through
  the steward.
- `evo-esp32-host`: the Symphonia warden replaces the simulated one; a
  paced sink (`sink.rs`) that holds 200 ms, drains at the sample rate and
  records to `--wav`; `--tone` writes a test tone. The check decodes the
  tone end to end and requires the recorded WAV to be the tone's size to
  the byte; FLAC and WAV come out bit-exact, MP3 as lossy allows. CI
  uploads the heard WAV.
- `evo-esp32-sdk`: `contract::audio`, the player's slot contracts from
  evo-device-audio's vocabulary: shelves `source/files`, `source/radio`,
  `playback/decode`, `delivery/output`, `metadata/tags`,
  `artwork/embedded`; `Track`; the verbs and their argument types; the
  playback warden's `Play` instruction, `Correction` and `State`;
  `to_value` / `from_value`. Presentation is a consumer, not a shelf.
- Plugins (Apache-2.0): `org.evoframework.source.files` (a directory on a
  host, std), `org.evoframework.playback.sim` (plays against the clock,
  `no_std`), `org.evoframework.delivery.null` (level and mute, no sink,
  `no_std`). 11 tests through the steward.
- `evo-esp32-host` (`distributions/host/`): the catalogue compiled in by
  `build.rs` and checked against the admitted plugins at start; a track
  listed, played to its end and released, projections and happenings
  printed; the panel as a consumer, drawing the companion's resting face
  from the playback and delivery projections through `companion-core`
  (by commit) and writing the last playing frame as a PNG. `check.sh`
  runs it once, keeps the PNG under `target/` (CI uploads it) and builds
  the two `no_std` plugins for bare-metal RISC-V.
- `evo-esp32-sdk` (Apache-2.0): the surface a plugin implements against.
  The catalogue as data (racks, shelves, shapes; validated), the plugin
  contract (describe, load, unload, health), respondents and wardens,
  subjects announced under an external addressing, happenings on a
  numbered stream, projections keyed to a rack or a subject, the context
  that is the only handle a plugin holds. `no_std` with `alloc`.
- `evo-esp32-steward`: admission against the catalogue with shape
  equality, subject tables keeping each plugin's contribution apart and
  composing them in admission order, a bounded happenings ring whose
  replay reports a gap, projections, custody of wardens with a tick the
  caller drives. `no_std` with `alloc`.
- `evo-esp32-catalogue`: parse and validate `catalogue.toml`, check a
  plugin set stocks it, emit the catalogue as Rust; a binary for a look by
  hand.
- `org-evoframework-example-echo` (Apache-2.0): the pedagogical plugin,
  driven through the steward and the catalogue tool in its tests.
- `scripts/check.sh`: format, clippy, tests, docs, the three `no_std`
  crates built for bare-metal RISC-V, shellcheck; refuses without the ESP
  toolchain once a firmware crate exists.
- The repository: what it is, how it fits with evo-core,
  evo-device-audio and volumio-esp-companion, the licence set (Business
  Source License 1.1 for the fabric, Apache 2.0 for the surface others
  implement), the trademark policy and the contribution flow.

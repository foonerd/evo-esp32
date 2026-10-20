# Changelog

All notable changes to evo-esp32 are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versions
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

# Changelog

All notable changes to stream-delay are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Fixed

- **Resume broadcasting aired what OBS sent while the stream was ended.**
  Resuming within the delay after **End stream** aired everything from the moment
  the stream was ended. Now only what OBS sends after resuming airs.
- **End stream on a slow or stalled upload.** It now cuts the connection at once,
  so video the computer had not sent yet is dropped instead of airing, and it no
  longer waits for a stalled connection (which could block resuming or new
  broadcasts for minutes).
- When the upload can't keep up, at most 8 MiB waits to be sent; the rest stays
  in the delay buffer (which has a memory cap) instead of an ever-growing queue.
- Saving two settings changes at the same moment could fail or leave the
  settings file with the older change.
- Links (dashboard, dock, overlay, OBS server) were broken when stream-delay
  listened on an IPv6 address.
- **End stream was undone when OBS reconnected by itself.** After a dropped
  connection OBS reconnects automatically, which resumed the broadcast. Now only
  stopping and starting the stream in OBS (or **Resume broadcasting**) does.
- `POST /api/v1/live` sent without a `Content-Type` header went live at once even
  when the body asked to air the buffer first. The body is now always read, and
  an invalid one is refused.
- *Allow control from other devices* took partial effect as soon as it was saved;
  like the listening address, it now applies at the next start.

### Security

- A wrong ingest key is refused only after a second, and keys are compared in
  constant time, so a network-reachable ingest key can't be guessed quickly.
- The dashboard removes its access token from the address bar once it has
  remembered it, so it doesn't show on stream or in screenshots.
- The backup of OBS's stream settings is kept in the keychain instead of the
  settings file: for a custom server it could include the server's password,
  which also showed in the settings API and diagnostics. Existing backups are
  moved at startup.
- The saved OBS WebSocket password is only sent to the OBS it was saved for, and
  OBS's original settings (with the stream key) are only restored to the OBS
  they came from. Before, connecting to another address sent it the saved
  password, and restoring would hand it the stream key.
- Importing the Twitch key from OBS checks that the destination really is
  Twitch, not just that its address mentions `twitch.tv`.
- Dock and overlay links no longer see the encoder's network address.
- *Download diagnostics* uses a single-use link, so the dashboard's access token
  no longer ends up in the browser's download history.
- The events WebSocket accepts messages of at most 64 KiB.
- Web pages can no longer be framed by other sites, and responses send
  `nosniff` and `Referrer-Policy: no-referrer`.
- Release builds: the update signing key and Apple credentials are only
  available to tag builds (through the `release` environment, see
  `docs/RELEASING.md`); dry runs sign with a throwaway key.

## [0.1.0] - 2026-09-23

The first public beta. Everything below is new.

### Relay

- Local RTMP server for OBS and other encoders, and an RTMP/RTMPS client to the
  destination (Twitch, YouTube, or any custom server). Media passes through
  unchanged; nothing is re-encoded.
- Supports legacy FLV and Enhanced RTMP (HEVC, AV1, VP9, Opus and others) and
  handles 32-bit timestamp wraparound on long streams.
- Keeps the destination connected when the encoder drops, for a grace period
  (30 s by default), and continues seamlessly when it comes back.
- Reconnects to the destination with backoff and resumes from the buffer, so
  viewers miss nothing.
- Ingest key for setups where the relay listens on the network. It is required
  then; if none is set, one is generated, saved and shown with the OBS server
  address.

### Delay

- Change the delay while live. **Rewind** jumps back instantly; **Mask** covers
  the change with an on-stream slate; **go live now**; **air up to now, then go
  live**;
  lowering the delay skips forward. Every change lands on a keyframe with
  continuous timestamps, including B-frame streams and open-GOP HEVC.
- Guarantee: once a delay is in effect, nothing airs sooner than that delay allows.
- **End stream**: ends the broadcast at once without airing what is in the delay
  buffer; **Resume** (or restarting the stream in OBS) starts a new broadcast.
- Optional rolling buffer (Setup): turn it off to keep only what the current
  delay needs; delay is then always added behind the Mask slate.
- A new stream never rewinds into the previous one, and the buffer is released
  when a broadcast ends.
- Presets, a start delay, a maximum delay (120 s by default) and a memory cap.
- Health warnings for long keyframe intervals, a slow uplink, Enhanced Broadcasting
  (multitrack) and not enough buffered history.

### Controls

- Web dashboard, OBS browser dock and a transparent overlay (delay badge,
  change pop-up, Mask slate), all served locally.
- Local HTTP and WebSocket API, protected by a per-install token, a `Host` check
  against DNS rebinding and a same-origin check. Dock and overlay links carry
  tokens that can only control the delay or only read the state; settings,
  stream keys, diagnostics and OBS need the dashboard link.
- `streamdelayd` command line: `run`, `urls`, `delay`, `live`, `state`,
  `diagnostics`.

### Desktop app

- Tray app for Windows, macOS and Linux: icon colored by state, presets in the
  menu, global hotkeys, start with the computer, single instance, updates.
- OBS setup wizard over obs-websocket: backs up OBS's stream settings, points OBS
  at stream-delay, optionally imports the Twitch key and adds the overlay, and
  restores everything with one click.
- Stream keys and passwords are stored in the OS keychain (in memory only with
  `--ephemeral`). A key typed into the destination URL is stored the same way,
  and changing the destination to another server forgets the saved key.
- Moves to another port automatically if 1935 or 7788 is taken by another
  program, and says so if another copy of stream-delay is already running.

### Support and hardening

- **Download diagnostics** (dashboard → Advanced, or `streamdelayd diagnostics`):
  version, settings, state and recent logs, with keys, passwords and tokens removed.
- Memory stays flat on long streams: buffered media is stored in shared 1 MiB
  blocks rather than one allocation per message, which fragmented the heap.
- Limits on memory use for untrusted RTMP input: before an encoder publishes it
  may only send messages up to 64 KiB, must publish within 15 s, and one address
  may hold at most four connections. Fuzzing of the RTMP, AMF0 and FLV parsers
  and the engine; chaos tests (destination resets and stalls, encoder
  crashes); a soak test that watches memory over hours.
- User guide at <https://commitandpray67.github.io/stream-delay/>.

### Known limitations

- Not yet tested widely on real Twitch streams; see `docs/testing.md`.
- Twitch Enhanced Broadcasting (multitrack) and the separate Twitch VOD audio
  track are not supported.
- One destination at a time; the buffer is kept in memory.
- Installers are not code-signed yet (SmartScreen and Gatekeeper warnings).

[Unreleased]: https://github.com/commitandpray67/stream-delay/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/commitandpray67/stream-delay/releases/tag/v0.1.0

# Changelog

All notable changes to stream-delay are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

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
- Optional ingest key for setups where the relay listens on the network.

### Delay

- Change the delay while live. **Rewind** jumps back instantly; **Mask** covers
  the change with an on-stream slate; **go live now**; **go live after it airs**;
  lowering the delay skips forward. Every change lands on a keyframe with
  continuous timestamps, including B-frame streams and open-GOP HEVC.
- Guarantee: once a delay is in effect, nothing airs sooner than that delay allows.
- Presets, a start delay, a maximum delay (120 s by default) and a memory cap.
- Health warnings for long keyframe intervals, a slow uplink, Enhanced Broadcasting
  (multitrack) and not enough buffered history.

### Controls

- Web dashboard, OBS browser dock and a transparent overlay (delay badge,
  change pop-up, Mask slate), all served locally.
- Local HTTP and WebSocket API, protected by a per-install token, a `Host` check
  against DNS rebinding and a same-origin check.
- `streamdelayd` command line: `run`, `urls`, `delay`, `live`, `state`,
  `diagnostics`.

### Desktop app

- Tray app for Windows, macOS and Linux: icon colored by state, presets in the
  menu, global hotkeys, start with the computer, single instance, updates.
- OBS setup wizard over obs-websocket: backs up OBS's stream settings, points OBS
  at stream-delay, optionally imports the Twitch key and adds the overlay, and
  restores everything with one click.
- Stream keys and passwords are stored in the OS keychain.
- Moves to another port automatically if 1935 or 7788 is taken.

### Support and hardening

- **Download diagnostics** (dashboard → Advanced, or `streamdelayd diagnostics`):
  version, settings, state and recent logs, with keys, passwords and tokens removed.
- Limits on memory use for untrusted RTMP input; fuzzing of the RTMP, AMF0 and
  FLV parsers and the engine; chaos tests (destination resets and stalls, encoder
  crashes); a soak test that watches memory over hours.
- User guide at <https://commitandpray67.github.io/stream-delay/>.

### Known limitations

- Not yet tested widely on real Twitch streams; see `docs/testing.md`.
- Twitch Enhanced Broadcasting (multitrack) and the separate Twitch VOD audio
  track are not supported.
- One destination at a time; the buffer is kept in memory.
- Installers are not code-signed yet (SmartScreen and Gatekeeper warnings).

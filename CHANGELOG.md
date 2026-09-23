# Changelog

All notable changes to stream-delay are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Changed

- **Stopping the stream in OBS ends the broadcast as soon as the rest has
  aired.** It used to stay connected for the whole grace period (30 s by
  default) with nothing to send, so every stream ended with 30 s of frozen
  video. The grace period now applies only when OBS crashes or loses its
  connection. Stopping and restarting OBS within 30 s therefore starts a new
  broadcast, as it does when streaming to Twitch directly.
- **Reconnecting to Twitch after a drop starts at once**, then after 0.5, 1, 2
  and 4 s and every 5 s after that (it used to wait 1 s first and up to 10 s
  later). Every second spent reconnecting added a second of delay. A refused
  stream (for example a wrong key) is still retried only every 10 s.
- Settings given on the command line apply to that run only: changing settings
  on the dashboard saves just what was changed there, never the command-line
  values, to `config.toml`.
- The dock no longer has a separate **Go live now** button: its **Live** preset
  does the same, and the dock always shows it, even without a 0 s preset. The
  dashboard, tray menu and hotkeys keep both go-live actions.
- API tokens must be at least 16 characters; a shorter `--token` or
  `STREAMDELAY_TOKEN` is refused at startup (generated tokens have 32).
- Settings from the config file and the command line (`--max-delay`, `--delay`,
  `--grace`) are checked at startup with the same limits as the dashboard; the
  grace period can be at most 600 s.
- The Docker instructions publish the dashboard port on the local machine only
  (`-p 127.0.0.1:7788:7788`); see the user guide before exposing it.

### Fixed

- **A finished stream could go live again later.** If Twitch could not be
  reached when OBS stopped (internet down, or a refused stream key),
  stream-delay kept retrying forever, and once Twitch was reachable again it
  started a new broadcast with a leftover frame of the finished stream (which
  can notify followers). It now gives up once the grace period is over and
  discards what never aired. A stream shorter than the delay still airs in
  full, on schedule.
- **A connection to Twitch that died without either side noticing** (after the
  computer switched networks, for example) froze the broadcast until the
  operating system gave up on it, which can take a quarter of an hour on macOS
  and Linux. stream-delay now reconnects once Twitch has taken no data for 20 s,
  and continues from the buffer.
- **`docker stop` (and systemd) did not stop `streamdelayd` cleanly:** the
  request was ignored, Docker killed it after 10 s, and Twitch was not told the
  broadcast had ended. It now ends the broadcast and exits, like Ctrl+C.
- **Running the OBS setup again after stream-delay's port changed** (as the
  troubleshooting guide suggests) replaced the backup of your original OBS
  settings with stream-delay's old address, so **Restore** could not bring them
  back. The backup is now kept.
- The OBS setup pointed an OBS on another computer at `127.0.0.1`, that is at
  itself. It now gives OBS this computer's address, or explains that
  stream-delay only accepts streams from this computer.
- With a maximum delay below 120 s (for example `--max-delay 60`), the default
  120 s preset failed when pressed and the delay settings could not be saved.
  Presets longer than the maximum are now left out, with a warning in the log.
- **End stream while stream-delay was still connecting to Twitch** let that
  connection finish and start the broadcast for a moment. The attempt is now
  dropped before the broadcast starts.
- **OBS's reconnect was refused after a network drop between two PCs**, for up
  to 30 s, because its old connection still looked open ("another encoder is
  already streaming"). A reconnecting encoder now takes over once the old
  connection has been silent for 2 s.
- Quitting stream-delay while live now waits until Twitch has been told the
  broadcast ended (at most a few seconds) instead of a fixed 0.3 s, and the
  desktop app asks before quitting while you are streaming.
- A broadcast whose encoder left no longer waits forever on a destination that
  stopped taking data.
- **A `--dest` for another server got the stream key saved for your usual
  destination** when no `STREAMDELAY_KEY` was given. The saved key now only goes
  to the destination it was saved for (or another server of the same service,
  such as Twitch over RTMPS).
- Memory stays flat at about the buffered data plus 12 MB. With glibc, freeing
  and reallocating the 1 MiB receive blocks kept 20-35 MB more (and the nightly
  soak test failed its memory check); full blocks are now reused.
- A destination URL with a non-ASCII character where `rtmp://` would end crashed
  the request that set it.
- When stream-delay gives up at the end of a stream because Twitch can't be
  reached, the dashboard says so, and an old connection error is no longer shown
  once the destination is working again.
- The desktop app re-registered all global hotkeys whenever any setting changed,
  so a hotkey pressed at that moment could be missed; it now does so only when
  hotkeys or presets change.
- Release builds: without an Apple certificate configured, the macOS build
  failed trying to import an empty one. It now builds unsigned, with a warning.
- The nightly fuzzing job never ran (the repository's `rust-toolchain.toml`
  overrode the nightly toolchain it needs), and one fuzz target no longer
  compiled. Both fixed; CI now checks the fuzz targets compile, and the engine
  target also covers End stream and the rolling-buffer switch.
- The desktop app kept only the current run's log; the previous one is now kept
  as `stream-delay.previous.log`, so the log of a crashed run survives the
  restart.

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
- Anyone who could reach the RTMP input could keep OBS from connecting by
  holding all 16 connection slots with idle connections. When the slots are
  full, the oldest connection that is not streaming is now closed to make room;
  IPv6 addresses count per /64 network against the per-address limit; and an
  address that sends five wrong ingest keys is ignored for a minute.
- An encoder could make stream-delay keep large receive buffers outside its
  memory limits by starting big messages and abandoning them; those buffers are
  now freed.
- Messages from the destination server are limited to 128 KiB (a hostile server,
  or anyone in the path of a plain `rtmp://` connection, could make stream-delay
  hold up to 64 MiB), and its error text is shortened before it is shown.
- Decoder configuration kept for splices is capped, so an encoder can no longer
  make stream-delay hold data outside the memory cap with many of them.
- GitHub Actions are pinned to commit hashes, so a moved tag of a third-party
  action cannot run in the release job, which holds the signing keys.
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

# stream-delay: project plan

An open-source, cross-platform app for **changing stream delay while live**.

> **Status:** M0–M5 are implemented: relay, delay engine, control API, web UI, OBS wizard, desktop app, and the M5 hardening (decoder limits, fuzzing, chaos and soak tests, diagnostics export, user guide). What's left before v1.0 is real-world testing on Twitch with the checklist in [`testing.md`](testing.md), a full 12 h soak run, and code signing. This document is the source of truth for scope, architecture and the roadmap. Changes go through PRs to this file, and significant decisions get an ADR in `docs/adr/`.
>
> Where the implementation differs from the plan:
> - The API is documented by hand in [`API.md`](API.md) rather than generated with utoipa/OpenAPI.
> - The OBS wizard accepts OBS 28+ (obs-websocket 5.0), not just what the `obws` crate checks by default.
> - End-to-end testing found that splices need PTS-aware placement and HEVC CRA→BLA rewriting ([ADR 0003](adr/0003-splice-safety.md)).

Working name: **stream-delay** (the binary is `streamdelay`). Pick a public brand name before v1.0, and avoid "Instant" so there is no trademark clash.

**Contents:** [Context](#context) · [Why a proxy](#why-a-proxy-that-works-on-encoded-data) · [How delay changes work](#how-delay-changes-work-the-core-idea) · [Architecture](#architecture) · [Testing](#testing-strategy) · [Roadmap](#roadmap-rough-effort-for-one-experienced-developer) · [Risks](#risks-and-mitigations) · [Verification](#verification-how-each-milestone-will-be-proven) · [References](#references)

## Context

[InstantDelay](https://github.com/Lypningeuh/Instantdelay-releases) lets a Twitch streamer add, remove or change stream delay **while live, without restarting OBS**. OBS locks its built-in delay when the stream starts, and Twitch's own delay setting can't be changed mid-stream either. InstantDelay is closed source, Windows-only and paid (30-day trial). Its release notes describe how it works:

- It runs a **local RTMP proxy**. OBS streams to `127.0.0.1` and the app forwards the stream to Twitch, with **no transcoding**.
- **"Instant Mode"** keeps a rolling buffer of 5–120 s so the delay can be toggled in under 100 ms.
- **"Overlay Mode"** (earlier called "Normal Mode") puts a custom overlay on screen while the delay changes.
- Other features: an OBS browser dock, an overlay browser source, a Stream Deck plugin (it talks to a WebSocket on localhost), and multistreaming (Twitch, YouTube, Kick, TikTok, custom).
- The fixes in its changelog point to the hard parts: handling RTMP boundaries and chunks, reconnects, buffer underruns, and securing the local API (origin checks and Private Network Access).

Open-source prior art exists but doesn't cover what we need. [InstantClone](https://github.com/Soulhackzlol/InstantClone) is written in Rust under GPL-3.0, runs only on Windows and Linux, and is a one-person hobby project. [DelayRelay](https://github.com/SunFIow/DelayRelay) is a minimal Node.js project under MIT. Both confirm the core technique: splice the buffered RTMP stream on keyframes and rewrite the timestamps.

**Goal:** build a free, GPL-3.0, **cross-platform (Windows, macOS, Linux)** app with polished setup that anyone can install. v1.0 targets Twitch first with one destination, though any RTMP or RTMPS URL will work.

**Decisions:**

- Build new: study the prior art, copy no code.
- Stack: a **Rust core with a Tauri v2 desktop app and a Svelte web UI**.
- License: **GPL-3.0-or-later**, so forks stay open.
- v1.0 scope: Twitch first, one destination, all three desktop OSes.

## Why a proxy that works on encoded data

- **Inside OBS, before encoding** (for example, Exeldro's Dynamic Delay filter): the buffer holds raw frames, which is gigabytes of RAM per 30 s at 1080p60. Audio has to be handled separately, and the approach only works in OBS.
- **A proxy after encoding** (our approach): the buffer holds compressed data, about 1 MB/s at 8 Mbps. It works with any encoder (OBS, Streamlabs, vMix, hardware encoders), costs almost no CPU, and changing the delay means moving a read cursor.

## How delay changes work (the core idea)

Every message OBS sends goes into a ring buffer, stamped with the time it arrived and indexed by keyframe. Each destination has an **output cursor**. A message is sent when `now >= arrival + delay`, which keeps the encoder's original pacing so there are no bitrate bursts. A delay change is a **splice**: the cursor jumps to a keyframe, and the output timestamps are shifted so they keep increasing smoothly. The Twitch connection is never interrupted.

```
Add 30 s ("Rewind", the default, instant):
  Streamer (live): ... 101 102 103 104 | 105 106 ...         pressed at t=104
  Viewers see:     ... 101 102 103 104 | 74 75 76 ... 104 105 ...   (the last 30 s repeats)
Go live (skip, at the next live keyframe, ≤ 1 GOP ≈ 2 s):
  Viewers at 170 → jump to live 200. Content 170–200 never airs.

Add 30 s ("Mask": the overlay browser source shows a slate S; pressed at t=102):
  Streamer (live): ... 101 | S 102–133 | 134 135 ...        slate hidden at the splice (t=134 = K + 30)
  Viewers see:     ... 101 | S 102–133 (live) | S 104–133 (replayed from keyframe K=104) | 134 135 ...
                   → about 2D of slate, no gameplay shown twice, then 30 s behind live
```

Delay modes. **v1** includes Rewind, Mask, the two go-live options and the arbitrary change; Hold comes later.

| Operation | Behavior | What viewers see |
|---|---|---|
| **Rewind** (add, default) | Jump back to the newest keyframe where `arrival ≤ now − D` | The last D seconds again. Nothing new leaks. |
| **Mask** (add, "Overlay Mode") | The overlay shows a slate at t0. Wait for a keyframe K after t0 plus a 0.5 s margin, wait until `now − K ≥ D`, splice to K, then hide the slate | Slate for about 2D; no repeated gameplay |
| **Go live now** | Splice at the next keyframe arriving from OBS | Skips the gap |
| **Go live after it airs** | Remember the live edge now; once that point has aired, go live | Sees everything up to the button press, then jumps |
| **Change D₁→D₂** | Rewind if D₂ > D₁; if D₂ < D₁, skip to a keyframe | As above |
| **Hold** (v2, experimental) | Repeat the last keyframe at 2 fps plus generated silent audio for D seconds | A frozen frame for about D |

Guarantee (and the key test invariant): **once `set_delay(D)` takes effect, nothing is sent less than D after it arrived** until the delay is lowered again. If the buffer holds less than D of history (for example, just after going live), the engine clamps to the oldest keyframe and the UI shows "only 12 s available". It never silently weakens the protection.

## Architecture

```
OBS / Streamlabs / any encoder
   │ RTMP  rtmp://127.0.0.1:1935/live/<anything>
   ▼
┌───────────────────── streamdelay core (Rust, one process) ────────────────────────┐
│ Ingest server → FLV inspector → Ring buffer + keyframe index → Output cursor(s) ──┼─→ Egress client → Twitch (RTMP/RTMPS)
│                                        ▲                     (splice + ts rewrite)
│ Control API (HTTP+WS, 127.0.0.1:7788) ─┘   obs-websocket client (setup wizard, overlay)
└──────────┬────────────────────────────────────────┬──────────────────────────────┘
   Tauri tray + window              Web UI: /dock (OBS dock), /overlay (browser source), /
                                    Stream Deck plugin, global hotkeys, HTTP for bots
```

### Repository layout (Cargo workspace plus a pnpm workspace for the UI and integrations)

```
crates/
  rtmp/          handshake, chunk codec (fmt 0–3, extended ts), control msgs, AMF0; no I/O ("sans-IO")
  flv/           tag inspection: keyframe, sequence header, codec; legacy + Enhanced RTMP (fourCC, multitrack)
  engine/        ring buffer, keyframe index, cursors, splice/timestamp logic; sans-IO, simulated clock
  relay/         tokio runtime: ingest server, egress client(s), reconnects, pacing, RTMPS (tokio-rustls)
  control/       axum HTTP + WebSocket API, auth, OpenAPI (utoipa), embedded UI (rust-embed)
  obs/           obs-websocket v5 client (obws crate): auto-config, backup/restore, overlay source creation
  config/        TOML config, keychain secrets (keyring crate), migrations
  streamdelayd/  headless binary: CLI + daemon (Linux servers, Docker, power users)
apps/desktop/    Tauri v2 shell: tray, window, setup wizard, autostart, single-instance, global shortcuts, updater
ui/              Svelte 5 + Vite + TS: routes /, /dock, /overlay, /setup; i18n; a11y
integrations/
  streamdeck/    Elgato Stream Deck plugin (@elgato/streamdeck SDK, TS) → WS API     (v1.x)
  companion/     Bitfocus Companion module                                        (v1.x)
tests/e2e/       ffmpeg → proxy → MediaMTX (docker) harness, packet/hash validators
fuzz/            cargo-fuzz targets (chunk decoder, AMF0, FLV)
docs/            PLAN.md, adr/, user guide (mdBook → GitHub Pages)
.github/         CI, release, issue templates
```

Main dependencies: tokio, bytes, tokio-rustls with webpki-roots, axum, serde/toml, tracing, keyring, obws, rust-embed, utoipa, proptest, and Tauri v2 with its plugins (tray, autostart, single-instance, global-shortcut, updater). We write the RTMP layer ourselves, about 2k lines of code. Existing crates (rml_rtmp, xiu) don't support Enhanced RTMP and don't let us mirror connect properties exactly. Owning this layer makes byte-for-byte passthrough and exact OBS parity possible.

### `rtmp` crate: protocol details that must be right

- **Handshake.** Use the simple C0/C1/C2 handshake on both sides (validate against Twitch in M1).
- **Chunking.** Keep per-chunk-stream header state and handle Set Chunk Size in both directions (OBS sends 4096; we send 4096 on egress). Also handle Abort, Window Ack Size, Acknowledgement (at the peer's window), Set Peer Bandwidth, and User Control (StreamBegin, Ping→Pong).
- **Extended timestamps.** These include the repeated field on type-3 chunks. Get this wrong and streams break after **4 h 39 m**. Internally, timestamps are unwrapped to u64; they are wrapped back to 32 bits on output.
- **Server state machine.** `connect` returns `_result` (NetConnection.Connect.Success). `releaseStream` and `FCPublish` get `_result` and `onFCPublish`. `createStream` gets `_result(1)`. `publish` gets `onStatus` NetStream.Publish.Start. `FCUnpublish`, `deleteStream` and `closeStream` mark the end of ingest.
- **Client state machine (egress).** Send `connect`, mirroring OBS's `fourCcList` and capability fields, then `releaseStream`, `FCPublish`, `createStream`, `publish(key,"live")`, and wait for Publish.Start. On shutdown, send `FCUnpublish` then `deleteStream`.
- **AMF0.** Support the full type set. AMF3 gets a clear error.
- **Stream key.** By default the key is stored in the app (the user never types it into OBS). A "passthrough" option forwards whatever key OBS sends. Keys are redacted everywhere in logs and the UI.

### `flv` crate: tag inspection (read only; payloads are never modified)

- **Legacy video:** frameType = `b0>>4`, codecId = `b0&0xF` (7 = AVC). AVCPacketType 0 is a sequence header, 1 is NALUs.
- **Enhanced RTMP** ([veovera spec](https://github.com/veovera/enhanced-rtmp)): `IsExHeader` is bit 7 and `PacketType` is `b0&0xF` (SequenceStart, CodedFrames, CodedFramesX, Multitrack, …). The fourCC values are hvc1, av01, vp09 and avc1. Enhanced audio uses soundFormat 9 with fourCC Opus, fLaC, ac-3 or mp4a.
- **Multitrack** (Twitch Enhanced Broadcasting) is detected and flagged in v1, with no support promised; see v2.
- **Cached for re-sending:** the latest sequence header for each track and `@setDataFrame onMetaData`.

### `engine` crate (sans-IO, the heart of the app)

```rust
Engine::on_ingest(now, MediaMsg)                         // append, index keyframes, evict whole GOPs past capacity
Engine::on_command(now, Command) -> Result<Ack, Error>   // SetDelay{secs,mode}, GoLive{Now|AfterAir}, Preset(n)
Engine::poll_output(now, &mut Vec<OutMsg>) -> Option<Instant>  // due msgs per cursor + next wake-up time
Engine::snapshot() -> State                               // for API/UI
```

Splicing to keyframe K:

- `ts_offset = max(last_out_ts over all tracks) + ~1 frame − K.ts_in`.
- Audio with `ts_in < K.ts_in` is dropped after the splice, so audio and video stay in sync.
- The composition-time offset (B-frames) sits inside the payload and needs no change.

The old segment may stop partway through a GOP. The decoder discards the few dangling B-frames, which is standard splice behavior. A "splice at GOP boundary" option exists if Twitch testing shows artifacts. Output timestamps start at 0 for each egress session.

Buffering:

- The buffer is RAM-only in v1: capacity = `max_delay` (default 120 s), with a RAM cap. At 8 Mbps × 120 s that is about 120 MB.
- A disk-backed ring comes in v1.x, allowing delays up to 15 minutes.

### `relay` crate: runtime and resilience

- **Tasks.** One ingest listener (a single publisher; a second one is rejected with a clear error). One engine task: a `select!` loop over ingest messages, commands and the next-send timer. One egress writer per destination, with TCP_NODELAY and keepalive.
- **Egress reconnect.** Back off exponentially (1→10 s). Then redo the handshake, re-send metadata and sequence headers, and resume from the cursor's keyframe. **Viewers lose nothing**; the delay grows by the length of the outage. An optional policy later skips back to the target delay.
- **Encoder reconnect bridging.** While OBS reconnects, keep Twitch connected and keep draining the buffer (a 30 s grace period by default). When OBS is back, splice its new session in at the first keyframe. If the codec config changed, send new sequence headers first. If the grace period expires, end the broadcast cleanly.
- **Health.** Track ingest and egress bitrate, the egress send backlog (a sign of an upload bottleneck), measured GOP length (warn if over 2 s, since Twitch requires 2 s), fps, buffered seconds and MB, and frames dropped at splices.

### `control` crate: local API (versioned; OpenAPI published)

```
GET  /api/v1/state                PUT /api/v1/delay {seconds, mode:"rewind"|"mask"}
POST /api/v1/live {when:"now"|"after-air"}      POST /api/v1/presets/{n}
GET/PUT /api/v1/config (key is write-only)      WS /api/v1/events (state + stats at 4 Hz)
GET /healthz    static: /  /dock  /overlay  /setup
```

Security is part of v1, because a malicious web page could otherwise reach localhost:

- Bind to 127.0.0.1 only.
- Generate a random 128-bit token on first run. It is required on the API and WebSocket and is embedded in the dock and overlay URLs.
- Allow only known `Host` header values, which blocks DNS rebinding.
- Check `Origin`; CORS is off by default.
- LAN access (phone or second PC) is opt-in only and still needs the token.
- RTMP ingest also binds to localhost by default, with an optional ingest key for two-PC setups.

### UI: one Svelte bundle embedded in the binary, three surfaces

- **/dock** (an OBS Custom Browser Dock, about 300×400): a big state badge (LIVE / DELAY 30s / ADDING… / GOING LIVE…), preset buttons (0/15/30/60/120), a custom value, a Rewind/Mask toggle, "Go live now" and "Go live after it airs" buttons, a buffer-fill bar, health, and warnings.
- **/overlay** (a transparent browser source): an optional delay badge, a transition popup, and a full-screen mask slate. In Mask mode the core tells this page over WebSocket to show or hide the slate, so **Mask mode needs no obs-websocket**. An overlay builder covers text, image, colors and animation.
- **/** (dashboard, inside the Tauri window): the setup wizard, destinations, live graphs, logs, settings, and export of a log bundle with secrets redacted.
- i18n from day one (English first; community translations through Weblate), keyboard and screen-reader accessibility, and light/dark themes.

### OBS integration

- **Manual path** (always works, on any OS and with any encoder): Settings → Stream → Custom, server `rtmp://127.0.0.1:1935/live`, any key. Add the dock under Docks → Custom Browser Docks, and the overlay as a Browser Source. The UI provides copy buttons and per-OS screenshots in the docs.
- **Automatic path** (obs-websocket v5, built into OBS 28+): the user enters host, port and password once. The app calls `GetStreamServiceSettings` and backs up the result, which lets it **import the existing Twitch key into the keychain** with consent. It then calls `SetStreamServiceSettings(rtmp_custom, …)` and `CreateInput(browser_source)` to add the overlay. A **"Restore my original OBS settings"** button undoes this. The app refuses while OBS is streaming (checked with `GetStreamStatus`).
- **Hotkeys:** global shortcuts in the Tauri app for presets, go live and toggle. On Wayland they go through the xdg portal, falling back to the dock or Stream Deck. A native OBS plugin (C++ dock, OBS hotkeys, vendor requests) is a v2 option.
- **Port conflicts:** detect whether 1935 is taken, pick another port, and have the wizard update OBS.

### Desktop app and distribution

- Tauri v2 embeds the core as a library, so it all runs in one process.
- The tray icon is colored by state (green = live, amber = delayed). Its menu has presets, go live, open dashboard, copy dock URL, and quit.
- Autostart and single-instance are optional.
- `streamdelayd` runs headless for Linux servers and Docker, for example as a delay box on a VPS.
- **Releases** are built with tauri-action:
  - Windows: MSI/NSIS.
  - macOS: universal DMG.
  - Linux: AppImage, deb, rpm, and a Flatpak later.
  - Also: headless binaries, a Docker image on ghcr, and SHA256SUMS.
- **Updates:** signed Tauri updater manifests hosted on GitHub Releases.
- **Code signing:** SignPath Foundation (free for open source) on Windows, and Apple Developer ID plus notarization on macOS (about $99/yr, funded through GitHub Sponsors or Open Collective).
- **Later packaging:** winget, Homebrew cask, Flathub.
- **Privacy:** no telemetry, no accounts, no servers of our own. Keys live in the OS keychain (fallback: a config file with 0600 permissions).

## Testing strategy

1. **Unit tests (sans-IO, most coverage):**
   - Chunk codec round-trips, including interleaved chunk streams, chunk-size changes, and extended timestamps on type-3 chunks.
   - AMF0 against golden byte vectors.
   - FLV and Enhanced RTMP parsing against captured OBS samples (H.264, HEVC, AV1, AAC, Opus).
   - Ring buffer eviction.
2. **Property tests (proptest) on the engine** with a simulated clock, random streams (random GOP/fps, audio jitter) and random commands. Invariants:
   - Output timestamps strictly increase per track.
   - Audio/video drift across splices stays within 1 frame.
   - Every splice lands on a keyframe.
   - After a rewind, the effective delay is within [D, D+GOP].
   - **No message is sent sooner than the active delay allows (no leaks).**
   - Memory stays within its bound.
3. **Golden replays:** captured OBS FLV dumps run deterministically through the engine.
4. **End-to-end in CI (Linux):**
   - ffmpeg `testsrc2`+`sine` (`-g 60 -f flv`) → proxy → MediaMTX in Docker, standing in for Twitch.
   - Scripted API delay changes.
   - `ffprobe -show_packets` checks monotonic DTS; `ffmpeg -v error -f null -` confirms no decode errors across splices.
   - A sink harness hashes payloads to prove byte-for-byte passthrough and to measure the delay exactly.
5. **Long-stream and chaos tests:**
   - Start timestamps near 0xFFFFFF (ffmpeg `-output_ts_offset`) to hit the 4 h 39 m wrap in seconds.
   - toxiproxy for egress drops, latency and resets; kill OBS mid-stream.
   - A 12 h soak test with random toggles every 1–5 min, with memory staying flat.
6. **Fuzzing:** `cargo-fuzz` on the chunk, AMF0 and FLV decoders, since they handle untrusted network input.
7. **Real-world compatibility matrix** (manual or nightly; checklist in `docs/testing.md`):
   - OBS 30–32 on Windows, macOS and Linux.
   - Encoders: x264, NVENC, QSV, AMF, Apple VT.
   - Codecs: H.264, HEVC, AV1; AAC and Opus.
   - Destinations: Twitch with `?bandwidthtest=true` appended to the key (nothing goes public), YouTube unlisted, Kick (RTMPS).
   - Viewer checks: Twitch player in low-latency and normal modes, plus a transcoded rendition.
8. **CI:** a GitHub Actions matrix (windows, macos-arm64, ubuntu) runs `fmt`, `clippy -D warnings`, `cargo test`, and vitest plus lint for the UI, with the e2e job on Linux. `cargo-deny` checks licenses (GPL-compatible) and advisories. Actions are pinned and dependencies updated with Renovate.

## Roadmap (rough effort for one experienced developer)

| Milestone | Scope | Exit criteria |
|---|---|---|
| **M0 Foundations** (~1 wk) | Workspace, CI, LICENSE, README, CONTRIBUTING, CODE_OF_CONDUCT, SECURITY.md, issue templates, ADR-001 (post-encode proxy), ADR-002 (splice strategy) | CI green on 3 OSes |
| **M1 Transparent relay** (~3 wk) | `rtmp` + `flv` crates, ingest server, RTMP/RTMPS egress, zero-delay passthrough, `streamdelayd --dest … --key-env …` | 2 h OBS → proxy → Twitch bandwidth test with no errors; ffprobe-clean at MediaMTX; byte-for-byte hashes match |
| **M2 Delay engine** (~3 wk) | Ring buffer, keyframe index, Rewind, Go-live now/after-air, ts rewrite, sequence-header cache, property tests, minimal HTTP API + `streamdelay set 30` CLI | Invariants pass; real Twitch 0→30→60→0 transitions look clean in the player (**validate this first: it's the biggest risk**) |
| **M3 Control surfaces** (~2–3 wk) | axum API + WS + auth hardening, Svelte dock/overlay/dashboard, Mask mode, presets | Full control from the OBS dock; overlay badge and mask work end to end |
| **M4 Desktop app** (~3 wk) | Tauri tray app, setup wizard (obs-websocket auto-config + restore + key import), keychain, global hotkeys, installers for 3 OSes, signed updater | A non-technical tester installs it and goes live with delay in < 10 min on each OS |
| **M5 Hardening → beta** (~3 wk) | Egress reconnect with buffer bridging, encoder reconnect bridging, health warnings, fuzzing, soak/chaos tests, log bundle, docs site (quick start, per-OS OBS guides, how the modes look to viewers, troubleshooting, API reference) | 12 h soak clean; public beta, then **v1.0** |
| **v1.x** | Stream Deck plugin + Companion module; multistreaming with **a separate delay per destination** (one cursor per egress); disk-backed buffer (up to 15 min); scene rules ("scene *Game* → 60 s, *Just Chatting* → go live after it airs"); SRT ingest; docs for Streamer.bot/Firebot/Touch Portal | — |
| **v2 (research)** | Hold mode (frozen frame + silence); Twitch Enhanced Broadcasting multitrack passthrough (proxy `GetClientConfiguration`); Twitch VOD audio track; native OBS plugin; optional FFmpeg-based smooth catch-up | — |

## Risks and mitigations

- **How Twitch ingest and players react to splices** (rewound content, dangling B-frames): test on a real Twitch bandwidth-test stream early in M2. The fallback is to splice only at GOP boundaries.
- **Custom RTMP servers turn off OBS's Twitch-only features** (Enhanced Broadcasting, VOD track): document this clearly; it is scheduled for v2.
- **Getting the protocol exactly right** (extended timestamps, acks, connect properties): fuzzing, golden captures, the long-stream test and the compatibility matrix.
- **Memory at high bitrate or long delay:** a RAM cap, a visible budget in the UI, and the disk ring in v1.x.
- **Code-signing cost and Wayland hotkey limits:** sponsorship funding; the dock and Stream Deck as alternatives.
- **Legal:** no InstantDelay branding and no code copied from anyone. InstantClone is GPL-3.0 and therefore license-compatible if we ever borrow with attribution, but the default is a clean implementation.

## Open questions (decide before v1.0)

- Public brand name (and a matching domain/org).
- Default for splicing: immediate (fastest) or at the old segment's GOP boundary (cleanest). Decide from M2 Twitch testing.
- Whether "go live after it airs" should become the default behavior of the dock's main "Go live" button.
- Where the community lives: GitHub Discussions only, or Discord as well.

## Verification (how each milestone will be proven)

- `cargo fmt --check && cargo clippy --all-targets -D warnings && cargo test --workspace`
- E2E: `docker run -p 1936:1935 bluenviron/mediamtx` as the sink, then `streamdelayd --ingest 127.0.0.1:1935 --dest rtmp://127.0.0.1:1936/live/test`, then `ffmpeg -re -f lavfi -i testsrc2=size=1280x720:rate=30 -f lavfi -i sine -c:v libx264 -g 60 -c:a aac -f flv rtmp://127.0.0.1:1935/live/x`. Script `curl -X PUT :7788/api/v1/delay -d '{"seconds":10}'` and go-live calls, then check the sink with `ffprobe -show_packets` (monotonic DTS) and `ffmpeg -v error -i rtmp://127.0.0.1:1936/live/test -f null -` (no decode errors).
- Real world: OBS → proxy → Twitch with `?bandwidthtest=true`, watched in the Twitch Inspector, while running through the compatibility checklist.

## References

- InstantDelay releases and changelog: <https://github.com/Lypningeuh/Instantdelay-releases/releases>
- InstantClone (Rust, GPL-3.0): <https://github.com/Soulhackzlol/InstantClone>
- DelayRelay (Node.js, MIT): <https://github.com/SunFIow/DelayRelay>
- Enhanced RTMP specification: <https://github.com/veovera/enhanced-rtmp>
- obs-websocket v5 protocol: <https://github.com/obsproject/obs-websocket/blob/master/docs/generated/protocol.md>
- obws (Rust obs-websocket client): <https://crates.io/crates/obws>
- Exeldro Dynamic Delay (pre-encode alternative): <https://github.com/exeldro/obs-dynamic-delay>
- Twitch: How to add stream delay: <https://help.twitch.tv/s/article/how-to-add-stream-delay>

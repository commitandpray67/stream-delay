# stream-delay

**Change your stream delay while you're live, without restarting OBS.** Free, open source (GPL-3.0), and built for Windows, macOS and Linux.

> **Status: beta candidate (milestones M0–M5 of [`docs/PLAN.md`](docs/PLAN.md)).** Everything is built and tested automatically, including end to end with ffmpeg, under network faults, and in hours-long soak runs. It has not yet been battle-tested on real Twitch streams, so test with `?bandwidthtest=true` before relying on it, and please share results ([`docs/testing.md`](docs/testing.md)).

**[User guide](https://commitandpray67.github.io/stream-delay/)** (source in [`docs/book`](docs/book/src/SUMMARY.md)) · [Changelog](CHANGELOG.md) · [API](docs/API.md)

## What it does

OBS locks its stream delay when you go live, and Twitch's delay can't be changed mid-stream either. stream-delay sits between your encoder and Twitch as a small local relay:

```
OBS  →  rtmp://127.0.0.1:1935/live  →  stream-delay  →  Twitch
```

- **Add delay instantly** when you need protection from stream snipers. Viewers briefly see the last few seconds again, and nothing new leaks.
- **Mask mode:** an on-stream slate covers the switch, so no gameplay is shown twice.
- **Go live** instantly, or **after what's already buffered has aired**, so chat can catch up with you.
- **No transcoding.** Video and audio pass through byte for byte, with almost no CPU use.
- **Controls:** an OBS dock, a browser-source overlay (badge and slate), global hotkeys, a local HTTP/WebSocket API, and later a Stream Deck plugin.
- **Private by design:** no accounts, no telemetry, no servers of our own. Your stream key stays in your OS keychain.

## How it works (short version)

Everything OBS sends goes into a rolling buffer, indexed by keyframe. The output to Twitch reads from a cursor into that buffer. Changing the delay moves the cursor to a keyframe and rewrites timestamps so they keep increasing, which means the connection to Twitch never drops. See [How delay changes work](docs/PLAN.md#how-delay-changes-work-the-core-idea).

## Try it

Build from source (Rust stable, Node 20+ with pnpm):

```sh
pnpm -C ui install && pnpm -C ui build   # web UI, embedded into the binary
cargo run --release -p streamdelayd -- run
```

`streamdelayd run` prints the OBS server address and links for the dashboard, OBS dock and overlay:

1. Open the **dashboard** link. Under **Setup**, choose Twitch and paste your stream key (add `?bandwidthtest=true` to test privately).
2. In OBS: **Settings → Stream → Custom…**, Server `rtmp://127.0.0.1:1935/live`, any stream key. Or let the Setup page configure OBS for you through obs-websocket.
3. Add the **dock** URL under **Docks → Custom Browser Docks** and the **overlay** URL as a Browser source.
4. Start streaming, then change the delay from the dock, the dashboard, hotkeys or the CLI (`streamdelayd delay 30`, `streamdelayd live --after-air`).

**Desktop app:** [`apps/desktop`](apps/desktop) wraps the same core in a tray app with global hotkeys, autostart and installers for Windows, macOS and Linux.

**Server or second PC:** run the container (`docker run -p 1935:1935 -p 7788:7788 -e STREAMDELAY_INGEST_KEY=… ghcr.io/commitandpray67/stream-delay`) or `streamdelayd run --ingest 0.0.0.0:1935 --ingest-key … --allow-lan`.

The HTTP/WebSocket API (for Stream Deck, Streamer.bot, scripts) is documented in [`docs/API.md`](docs/API.md).

## Roadmap

1. **M0:** foundations and CI.
2. **M1:** transparent RTMP/RTMPS relay.
3. **M2:** delay engine.
4. **M3:** dock, overlay and API.
5. **M4:** desktop app and installers.
6. **M5:** hardening, diagnostics and the user guide. Next: public beta, then **v1.0**.
7. **Later:** Stream Deck, per-destination multistream delay, long disk-backed delays, and scene-based automation.

Details are in [`docs/PLAN.md`](docs/PLAN.md#roadmap-rough-effort-for-one-experienced-developer).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Design decisions are recorded in [`docs/adr/`](docs/adr/).

## License

[GPL-3.0-or-later](LICENSE). stream-delay is not affiliated with InstantDelay, Twitch or OBS Project.

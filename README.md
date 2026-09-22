# stream-delay

**Change your stream delay while you're live, without restarting OBS.** Free, open source (GPL-3.0), and built for Windows, macOS and Linux.

> **Status: planning.** There is no code yet. The full design and roadmap are in [`docs/PLAN.md`](docs/PLAN.md).

## What it will do

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

## Roadmap

1. **M0:** foundations and CI.
2. **M1:** transparent RTMP/RTMPS relay.
3. **M2:** delay engine.
4. **M3:** dock, overlay and API.
5. **M4:** desktop app and installers.
6. **M5:** hardening, then beta, then **v1.0**.
7. **Later:** Stream Deck, per-destination multistream delay, long disk-backed delays, and scene-based automation.

Details are in [`docs/PLAN.md`](docs/PLAN.md#roadmap-rough-effort-for-one-experienced-developer).

## Contributing

The project is at the planning stage. Feedback on the plan is welcome: open an issue or a PR against `docs/PLAN.md`.

## License

[GPL-3.0-or-later](LICENSE). stream-delay is not affiliated with InstantDelay, Twitch or OBS Project.

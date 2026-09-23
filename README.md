# stream-delay

**Change your stream delay while you're live, without restarting OBS.** Free and open source (GPL-3.0), for Windows, macOS and Linux.

**[Download](https://github.com/commitandpray67/stream-delay/releases/latest)** · **[User guide](https://commitandpray67.github.io/stream-delay/)** · [Quick start](https://commitandpray67.github.io/stream-delay/quick-start.html) · [Changelog](CHANGELOG.md) · [API](docs/API.md)

> **Beta.** Every change is tested automatically, end to end with ffmpeg, under network faults and in long soak runs, but stream-delay has seen little use on real Twitch streams so far. Try it with a private test stream first (add `?bandwidthtest=true` to your Twitch stream key), and please [tell us how it went](docs/testing.md).

## What it does

OBS locks its stream delay when you go live, and Twitch's delay can't be changed mid-stream either. stream-delay sits between your encoder and Twitch as a small local relay:

```
OBS  →  rtmp://127.0.0.1:1935/live  →  stream-delay  →  Twitch
```

- **Add delay instantly** when a stream sniper shows up. Viewers briefly see the last few seconds again, and nothing new leaks.
- **Mask mode:** an on-stream slate covers the change, so no gameplay is shown twice.
- **Go live** at once, or **after what's buffered has aired**, so chat can catch up.
- **Starts and ends like streaming straight to Twitch.** Stop streaming in OBS and the delayed rest airs, then the broadcast ends. If OBS crashes or loses its connection, stream-delay keeps the broadcast open for 30 s so OBS can pick up where it left off. **End stream** cuts it off at once without airing the buffer.
- **Rides out network trouble.** If the connection to Twitch drops, stream-delay reconnects at once and continues from its buffer, so viewers miss nothing.
- **No re-encoding.** Video and audio pass through byte for byte, with almost no CPU use.
- **Control it your way:** an OBS dock, a browser-source overlay (delay badge and slate), global hotkeys, a tray menu, the command line, and an HTTP/WebSocket API for Stream Deck, Streamer.bot and scripts.
- **Private:** no accounts, no telemetry, no servers of our own. Your stream key stays in your OS keychain.

It works with any encoder that can stream RTMP (OBS, Streamlabs, vMix, hardware encoders).

## Get started

1. **Install** the desktop app from the [latest release](https://github.com/commitandpray67/stream-delay/releases/latest): the `.exe` or `.msi` on Windows, the `.dmg` on macOS, or the AppImage, `.deb` or `.rpm` on Linux. The installers aren't code-signed yet, so Windows and macOS warn on first launch; [Installing](https://commitandpray67.github.io/stream-delay/install.html) shows how to get past that.
2. **Add your stream key** on the dashboard's **Setup** tab, which opens on first launch.
3. **Point OBS at stream-delay.** The Setup tab can do it for you through obs-websocket (and import the key OBS already has). Or, in OBS, go to **Settings → Stream → Custom…**, set the server to `rtmp://127.0.0.1:1935/live`, and use any stream key.
4. **Add the dock and overlay** to OBS with the links on the Setup tab, and set OBS's keyframe interval to 2 s.
5. **Start streaming** as usual, and change the delay from the dock, the tray or a hotkey.

The [quick start](https://commitandpray67.github.io/stream-delay/quick-start.html) walks through each step.

> v0.1.0 has Windows and Linux installers and the Docker image. The macOS app and the standalone `streamdelayd` downloads come with the next release; until then, build them [from source](#build-from-source).

## Servers, Docker and second PCs

`streamdelayd` is the same relay without the desktop window. The dashboard, dock and overlay work the same.

```sh
streamdelayd run                  # prints the OBS server address and the dashboard, dock and overlay links
streamdelayd delay 30             # control a running instance
streamdelayd live --after-air
streamdelayd end
```

With Docker:

```sh
docker run -d --name stream-delay --restart unless-stopped \
  -p 1935:1935 -p 127.0.0.1:7788:7788 -v stream-delay:/data \
  -e STREAMDELAY_INGEST_KEY=choose-a-secret \
  ghcr.io/commitandpray67/stream-delay
```

To run stream-delay on another computer than OBS, see [Installing: headless and two-PC setups](https://commitandpray67.github.io/stream-delay/install.html#headless-servers-second-pc-advanced-users).

## How it works

Everything OBS sends goes into a rolling buffer, indexed by keyframe. The output to Twitch reads from a cursor into that buffer. Changing the delay moves the cursor to a keyframe and rewrites timestamps so they keep increasing, so the connection to Twitch never drops. See [How delay changes work](docs/PLAN.md#how-delay-changes-work-the-core-idea) and [the design notes](https://commitandpray67.github.io/stream-delay/design.html).

## Build from source

You need Rust (stable) and Node.js 22 with pnpm.

```sh
pnpm -C ui install && pnpm -C ui build   # the web UI, embedded into the binary
cargo run --release -p streamdelayd -- run
```

The desktop app is in [`apps/desktop`](apps/desktop), which explains how to build it. [CONTRIBUTING.md](CONTRIBUTING.md) covers tests, fuzzing and the code layout.

## Roadmap

- **Done:** the RTMP/RTMPS relay, the delay engine, dock, overlay and API, the desktop app and installers, and hardening. v0.1.0 is the first public beta.
- **Next:** testing on real streams, signed installers for Windows and macOS, then **v1.0**.
- **Later:** a Stream Deck plugin, per-destination delay for multistreaming, long disk-backed delays, and scene-based automation.

Details are in [`docs/PLAN.md`](docs/PLAN.md#roadmap-rough-effort-for-one-experienced-developer).

## Contributing and security

Contributions are welcome: see [CONTRIBUTING.md](CONTRIBUTING.md). Design decisions are recorded in [`docs/adr/`](docs/adr/). Please report security problems privately, as described in [SECURITY.md](SECURITY.md).

## License

[GPL-3.0-or-later](LICENSE). stream-delay is not affiliated with Twitch or the OBS Project.

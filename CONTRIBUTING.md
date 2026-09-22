# Contributing to stream-delay

Thanks for helping. This project aims to be a dependable tool that streamers can trust while they are live, so correctness and clarity matter more than speed.

## Before you start

- Read [`docs/PLAN.md`](docs/PLAN.md) for the architecture and roadmap.
- For anything larger than a small fix, open an issue or discussion first so we can agree on the approach.
- Significant design decisions are recorded as ADRs in [`docs/adr/`](docs/adr/).

## Development setup

You need:

- Rust (stable, see `rust-toolchain.toml`)
- Node.js 20+ and pnpm 9+ (for the web UI and desktop app)
- ffmpeg (for the end-to-end tests)
- On Linux, for the desktop app: `libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev libxdo-dev libssl-dev`

Common commands:

```sh
cargo fmt --all                 # format
cargo clippy --all-targets      # lint (CI treats warnings as errors)
cargo test                      # unit, property and integration tests
pnpm -C ui install && pnpm -C ui build   # build the web UI embedded into the binary
tests/e2e/run.sh                # ffmpeg -> streamdelayd -> ffmpeg end-to-end check
```

## Code layout

| Path | What lives there |
|---|---|
| `crates/rtmp` | Sans-IO RTMP: handshake, chunk codec, AMF0, publish sessions |
| `crates/flv` | Keyframe and codec-config detection (legacy FLV and Enhanced RTMP) |
| `crates/engine` | The delay engine: buffer, splicing and timestamp rewriting (sans-IO) |
| `crates/relay` | tokio runtime: ingest server, egress client, reconnects |
| `crates/config` | Config file and secret storage |
| `crates/obs` | obs-websocket integration (setup wizard) |
| `crates/control` | Local HTTP/WebSocket API and the embedded web UI |
| `crates/streamdelayd` | Headless binary and CLI |
| `ui/` | Svelte web UI: dashboard, OBS dock, overlay |
| `apps/desktop` | Tauri desktop app |

## Guidelines

- Keep protocol and engine code sans-IO so it can be tested deterministically.
- Every behavior change needs a test. Engine changes should keep the property tests in `crates/engine/tests` passing, especially the "no content is sent before its delay" invariant.
- Never log stream keys, tokens or passwords. Use the redacted forms.
- Keep dependencies GPL-3.0 compatible (`cargo deny check` enforces this).
- Write commit messages in the imperative mood ("Add X", "Fix Y").

## License

By contributing you agree that your contributions are licensed under GPL-3.0-or-later.

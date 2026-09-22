# stream-delay desktop app

A tray app that runs the stream-delay relay, opens the dashboard in a native window and adds global hotkeys. It bundles the same core as `streamdelayd`; the UI is the web UI from [`ui/`](../../ui), served by the app on `http://127.0.0.1:7788`.

## What it does

- **Tray icon** colored by state: green = live, amber = delayed, blue = changing, grey = offline. The menu has the delay presets, "Go live after it airs", quick links (dashboard, OBS setup, copy the OBS server/dock/overlay URLs), "Start with my computer" and Quit.
- **Dashboard window** opens on first launch at the Setup tab: paste your stream key, then let stream-delay configure OBS through obs-websocket (with a one-click restore), or follow the manual steps.
- **Global hotkeys**, configurable in the dashboard under Advanced. Defaults:

  | Hotkey | Action |
  |---|---|
  | Ctrl+Alt+Shift+1 … 5 (Cmd on macOS) | Presets 1–5 (Live, 15 s, 30 s, 1 min, 2 min) |
  | Ctrl+Alt+Shift+L | Go live now |
  | Ctrl+Alt+Shift+A | Go live after it airs |

  On Linux they need X11 (or XWayland); under a pure Wayland session use the OBS dock or the tray.
- Closing the window keeps the relay running in the tray. Quit from the tray menu ends the broadcast cleanly.
- **Single instance:** launching it again focuses the running app.
- **Ports:** OBS streams to `rtmp://127.0.0.1:1935/live`. If port 1935 is taken, the app moves to 19350 (then 29350) and remembers it; the Setup tab always shows the address to use.
- **Secrets** (stream key, OBS password) are stored in the system keychain (macOS Keychain, Windows Credential Manager, Secret Service on Linux), with a private file as fallback.
- **Logs:** `stream-delay/logs/stream-delay.log` in the config directory (Linux `~/.config`, macOS `~/Library/Application Support`, Windows `%APPDATA%`).

## Building

Prerequisites: Rust (stable), Node.js 20+ with pnpm, and on Linux:

```sh
sudo apt-get install libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev libxdo-dev libssl-dev
```

Then:

```sh
pnpm -C ui install && pnpm -C ui build        # the web UI the app serves
pnpm -C apps/desktop install
pnpm -C apps/desktop tauri dev                 # run in development
pnpm -C apps/desktop tauri build               # installers in target/release/bundle/
```

`cargo build -p streamdelay-desktop` also works once the UI is built. The desktop crate is excluded from the workspace's default members, so `cargo test` at the root does not need WebKit.

Icons are generated from `make_icons.py` (`pnpm -C apps/desktop icons`).

## Releases and updates

Tagged releases build Windows (NSIS/MSI), macOS (universal DMG) and Linux (AppImage/deb/rpm) installers with signed auto-update manifests; see [`docs/RELEASING.md`](../../docs/RELEASING.md).

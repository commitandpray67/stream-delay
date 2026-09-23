# Installing

Download from the [releases page](https://github.com/commitandpray67/stream-delay/releases).
Each release lists SHA-256 checksums in `SHA256SUMS.txt`.

## Windows

Download the `.msi` or the `-setup.exe` installer and run it.

Until the project has a code-signing certificate, Windows SmartScreen may say
"Windows protected your PC". Click **More info → Run anyway**.

## macOS

Download the `.dmg` (a universal build for Apple Silicon and Intel), open it and
drag stream-delay to Applications.

Until the app is notarized by Apple, macOS blocks the first launch. Right-click
(or Control-click) the app, choose **Open**, and confirm. You only need to do this
once.

## Linux

Pick one:

- **AppImage:** `chmod +x stream-delay_*.AppImage` and run it.
- **Debian/Ubuntu:** `sudo apt install ./stream-delay_*.deb`
- **Fedora/openSUSE:** `sudo dnf install ./stream-delay-*.rpm`

The tray icon needs a desktop with StatusNotifier/AppIndicator support (GNOME
needs the *AppIndicator* extension). On Wayland, global hotkeys depend on your
desktop; the OBS dock and the API always work.

## Headless (servers, second PC, advanced users)

`streamdelayd` is the same relay without the desktop window. The web dashboard,
dock and overlay work exactly the same.

```sh
streamdelayd run                  # uses and creates the config file
streamdelayd urls                 # print the dashboard, dock and overlay links
streamdelayd delay 30             # control a running instance
streamdelayd live --after-air
streamdelayd diagnostics -o diag.json
```

Run `streamdelayd run --help` for all options, such as `--dest`,
`--key-env`, `--max-delay` and `--ephemeral`.

### Docker

```sh
docker run -d --name stream-delay --restart unless-stopped \
  -p 1935:1935 -p 7788:7788 -v stream-delay:/data \
  -e STREAMDELAY_INGEST_KEY=choose-a-secret \
  ghcr.io/commitandpray67/stream-delay
docker logs stream-delay          # shows the dashboard link with its token
```

The container listens on all interfaces, so it requires an ingest key: OBS must
stream with that key (Settings → Stream → Stream Key), and nobody else can
publish to your relay. Set it with `STREAMDELAY_INGEST_KEY`; without it,
stream-delay generates one, saves it in `/data/config.toml` and prints it next to
the OBS server address in `docker logs`. Keep port 7788 private (firewall or VPN); it is protected by
the token in the dashboard link.

### Two-PC setups

On the streaming PC, run stream-delay with `--ingest 0.0.0.0:1935 --ingest-key …`
(or the Docker image). In OBS on the gaming PC, use
`rtmp://<streaming-pc-ip>:1935/live` and the ingest key. If you leave out
`--ingest-key`, one is generated; `streamdelayd urls` and the Setup tab show it. To control it from
another device, enable *Allow control from other devices* on the Advanced tab and
restart stream-delay.

## Where things are stored

| | Linux | macOS | Windows |
|---|---|---|---|
| Settings | `~/.config/stream-delay/config.toml` | `~/Library/Application Support/dev.stream-delay.stream-delay/config.toml` | `%APPDATA%\stream-delay\stream-delay\config\config.toml` |
| Desktop app log | `logs/stream-delay.log` next to the settings file | same | same |
| Stream key, OBS password, OBS settings backup | OS keychain (Secret Service) | Keychain | Credential Manager |

If no keychain is available (or with `--no-keychain`), secrets go to a file next
to the settings that only your user can read.

## Updating and uninstalling

The desktop app checks for updates at start and from the tray menu (**Check for
updates**) once releases are signed. Uninstall it like any other app; delete the
settings folder above to remove your settings, and the `dev.stream-delay` entries from
your keychain.

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
`--key-env`, `--max-delay` and `--ephemeral`. Options given on the command line
apply to that run only: changing settings on the dashboard saves those changes,
not the command-line values. A `--dest` for another server never gets the stream
key saved for your usual destination; pass its key with `STREAMDELAY_KEY`.

### Docker

```sh
docker run -d --name stream-delay --restart unless-stopped \
  -p 1935:1935 -p 127.0.0.1:7788:7788 -v stream-delay:/data \
  -e STREAMDELAY_INGEST_KEY=choose-a-secret \
  ghcr.io/commitandpray67/stream-delay
docker logs stream-delay          # shows the dashboard link with its token
```

`docker stop` ends a running broadcast cleanly, like Ctrl+C in a terminal.

The container listens on all interfaces, so it requires an ingest key: OBS must
stream with that key (Settings → Stream → Stream Key), and nobody else can
publish to your relay. Set it with `STREAMDELAY_INGEST_KEY`; without it,
stream-delay generates one, saves it in `/data/config.toml` and prints it next to
the OBS server address in `docker logs`.

Port 7788 is the dashboard and control API. The command above makes it reachable
only from the machine running Docker. To use the dashboard from other devices on
your home network, publish it with `-p 7788:7788` instead. Never make it
reachable from the internet: it is plain HTTP, and the dashboard link carries the
token that controls everything. On a remote server, reach it through an SSH
tunnel (`ssh -L 7788:127.0.0.1:7788 you@server`, then open the link on your
computer) or a VPN.

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
| Desktop app log | `logs/stream-delay.log` next to the settings file (the previous run's in `stream-delay.previous.log`) | same | same |
| Stream key, OBS password, OBS settings backup | OS keychain (Secret Service) | Keychain | Credential Manager |

If no keychain is available (or with `--no-keychain`), secrets go to a file next
to the settings (`secrets.toml`) that only your user can read. Each secret is kept
in one place only. Should that file ever be damaged, stream-delay moves it to
`secrets.toml.damaged` (so nothing in it is lost for good) and starts a new one;
enter your stream key again.

## Updating and uninstalling

The desktop app checks for updates when it starts, and when you click **Check for
updates** in the tray menu or on the dashboard's **Advanced** tab; it asks before
installing one. (`streamdelayd` and Docker don't update themselves: the button
links to the latest release instead.) Uninstall it like any other app; delete the
settings folder above to remove your settings, and the `dev.stream-delay` entries from
your keychain.

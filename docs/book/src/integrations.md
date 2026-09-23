# Hotkeys, Stream Deck and scripts

## Global hotkeys (desktop app)

They work while any window has focus, including your game.

| Action | Default |
|---|---|
| Preset 1 … 5 (0, 15, 30, 60, 120 s) | `Ctrl+Alt+Shift+1` … `5` |
| Go live now | `Ctrl+Alt+Shift+L` |
| Air up to now, then go live | `Ctrl+Alt+Shift+A` |
| End stream (buffer is not aired) | none; set one on the Advanced tab |

On macOS, `Cmd` replaces `Ctrl`. Change or disable them on the dashboard's
**Advanced** tab; names look like `CmdOrCtrl+Alt+Shift+1`. If another program
already uses a combination, stream-delay logs a warning and skips it.

On Linux with Wayland, global hotkeys depend on your desktop environment. If they
don't work, use the OBS dock, the tray menu, or bind a key in your desktop's
settings to a command such as `streamdelayd delay 30`.

## Tray menu (desktop app)

The icon color shows the state (green: live, amber: delayed). The menu has your
presets, the go-live actions, **End stream now** and **Resume broadcasting**, links to the dashboard and OBS setup, **Copy**
entries for the server address, dock and overlay URLs, **Start with my
computer**, and **Check for updates…**. Closing the dashboard window keeps
stream-delay running; use **Quit stream-delay** in the tray to stop it.

## Command line

`streamdelayd` controls a running instance (desktop app or headless). It reads the
address and token from the settings file.

```sh
streamdelayd delay 30           # rewind to a 30 s delay
streamdelayd delay 20 --mask    # cover the change with the slate
streamdelayd live               # go live now
streamdelayd live --after-air   # air up to now, then go live
streamdelayd end                # end the broadcast; nothing buffered airs
streamdelayd resume             # broadcast again
streamdelayd state              # current state as JSON
```

## Stream Deck, Streamer.bot, Firebot, Touch Portal and scripts

Anything that can send an HTTP request with a method, a header and a JSON body can
control stream-delay. For Stream Deck, use a plugin such as *Web Requests* or
*API Ninja*; Streamer.bot, Firebot and Touch Portal have HTTP request actions.

Get the token from the dashboard link (**Advanced** tab, or tray → *Copy OBS dock
URL*: it's the part after `token=`). Then, for example:

| Button | Request |
|---|---|
| Delay 30 s | `PUT http://127.0.0.1:7788/api/v1/delay`, body `{"seconds": 30}` |
| Mask 30 s | `PUT http://127.0.0.1:7788/api/v1/delay`, body `{"seconds": 30, "mode": "mask"}` |
| Preset 3 | `POST http://127.0.0.1:7788/api/v1/presets/2` (0-based) |
| Go live | `POST http://127.0.0.1:7788/api/v1/live`, body `{"when": "now"}` |
| Air up to now, then go live | `POST http://127.0.0.1:7788/api/v1/live`, body `{"when": "after-air"}` |
| End stream | `POST http://127.0.0.1:7788/api/v1/stream/end` |
| Resume | `POST http://127.0.0.1:7788/api/v1/stream/resume` |

Send the header `Authorization: Bearer <token>` and, with a body,
`Content-Type: application/json`. Tools that can't set headers can add
`?token=<token>` to the URL instead.

To show the state on a button or in a bot, read `GET /api/v1/state`, or subscribe to
the WebSocket at `ws://127.0.0.1:7788/api/v1/events?token=<token>`. Everything is
documented in the [API reference](api.md).

A native Stream Deck plugin and a Bitfocus Companion module are planned.

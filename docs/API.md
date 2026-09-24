# Local API

stream-delay serves an HTTP and WebSocket API on `http://127.0.0.1:7788` (configurable). The dashboard, the OBS dock, the overlay, the `streamdelayd` CLI and third-party tools (Stream Deck, Streamer.bot, Touch Portal, scripts) all use it.

## Authentication

Every `/api` request needs a token. The install's admin token is generated on first run and stored in `config.toml`; the dock and overlay links carry tokens derived from it that can do less:

| Link | Scope | Allowed |
|---|---|---|
| Dashboard | `admin` | Everything. |
| Dock | `control` | `GET /api/v1/state`, the events WebSocket, and the delay control endpoints below. |
| Overlay | `read` | `GET /api/v1/state` and the events WebSocket. |

The state seen with dock and overlay tokens leaves out the encoder's address (`ingest.peer` is `null`) and what the destination replied to a failed connection (`egress.last_error` is `null`).

A valid token without enough scope gets `403 Forbidden`; a missing or wrong token gets `401 Unauthorized`. For a Stream Deck or another controller, use the dock link's token. The derived tokens stay the same as long as the admin token does.

Pass the token in one of these ways:

- `Authorization: Bearer <token>` (preferred)
- `X-Stream-Delay-Token: <token>`
- `?token=<token>` (for browser sources and WebSockets, which cannot set headers)

Requests must also use a loopback `Host` (`127.0.0.1`, `localhost` or `[::1]` with the right port), and browser requests from another origin are refused. Both checks stop malicious web pages from controlling your stream. LAN access can be enabled in settings (it takes effect at the next start); the token is still required.

Run `streamdelayd urls` to print links that already contain their tokens.

## Delay control

| Method and path | Body | Effect |
|---|---|---|
| `PUT /api/v1/delay` | `{"seconds": 30, "mode": "rewind" \| "mask"}` | Set the delay. `mode` defaults to the configured default. `0` removes the delay. |
| `POST /api/v1/live` | `{"when": "now" \| "after-air"}` | Remove the delay at the next keyframe, or after everything buffered so far has aired. No body means `now`. A body is read as JSON whatever its `Content-Type`, and an invalid one is refused with `400`. |
| `POST /api/v1/presets/{index}` | none | Apply a configured preset (0-based). |
| `POST /api/v1/cancel` | none | Cancel a pending change (for example a mask in progress). |
| `POST /api/v1/stream/dump` | `{"mode": "rewind" \| "mask"}` (optional) | Throw away everything that has not aired yet, so it never does, and keep broadcasting with the same delay. `rewind` (the default mode): viewers see the last stretch again, then the stream continues from what was recorded after the dump; this needs the buffer to reach back about twice the delay. `mask`, or not enough buffer, or the rolling buffer off: the overlay slate covers the stream while the delay builds back up (without the overlay in your scenes, viewers see you without delay meanwhile). `400` when there is no delay. While an `after-air` end is airing, the broadcast ends at once instead. |
| `POST /api/v1/stream/end` | `{"when": "now" \| "after-air"}` | End the broadcast. `now` (also with no body): at once, and everything still in the delay buffer is thrown away and never airs. `after-air`: once what stream-delay has received so far has aired; nothing received after the request airs (`ending` is `true` until then). Either way nothing more is sent until `resume` or a new encoder stream. A body is read as JSON whatever its `Content-Type`; an invalid one is refused with `400`. Returns the state. |
| `POST /api/v1/stream/resume` | none | Broadcast again after `end`, from content received from now on, with the current delay. While an `after-air` end is still airing, cancel it instead: the broadcast carries on. Returns the state. |

With the rolling buffer off (`delay.keep_buffer: false` in the settings), a delay
increase always uses `mask`, whatever mode is requested.

Commands return an acknowledgement:

```json
{ "target_ms": 30000, "effective_ms": 31200, "pending": false, "history_short": false }
```

- `effective_ms` can exceed the target by up to one keyframe interval, because changes land on keyframes.
- `pending` means the change completes later: a Mask fill, or waiting for a keyframe to go live.
- `history_short` means less of the stream was buffered than requested, so the delay is shorter than asked.

## State

`GET /api/v1/state` returns the full state:

```json
{
  "delay": {
    "phase": "delayed",
    "target_ms": 30000, "effective_ms": 31200, "max_delay_ms": 120000,
    "history_ms": 64000, "buffered_bytes": 48000000,
    "mask_visible": false, "history_short": false,
    "ingest": { "active": true, "video_codec": "avc", "audio_codec": "aac",
                "bitrate_kbps": 6100, "fps": 60.0, "gop_ms": 2000,
                "enhanced": false, "multitrack": false },
    "output": { "connected": true, "splices": 3, "dropped_frames": 12, "sent_bytes": 123456789 },
    "warnings": []
  },
  "ingest": { "listen": "127.0.0.1:1935", "connected": true, "peer": "127.0.0.1:53546", "app": "live", "last_error": null },
  "egress": { "status": "live", "destination": "rtmp://live.twitch.tv/app", "last_error": null,
              "bitrate_kbps": 6150, "backlog_bytes": 0, "reconnects": 0 },
  "ended": false,
  "ending": false
}
```

`ended` (top level) is `true` after `POST /api/v1/stream/end` until the broadcast
resumes; `ending` is `true` while an `after-air` end is still airing. `phase` is
one of the following:

| Phase | Meaning |
|---|---|
| `offline` | No encoder is connected. |
| `live` | No delay. |
| `delayed` | A delay is in effect. |
| `adding` | Mask mode (or a dump under the slate) is filling the buffer while the slate is up. |
| `going-live` | Waiting to remove the delay. |
| `reducing` | Waiting for a keyframe to shorten the delay. |

## Events (WebSocket)

`GET /api/v1/events?token=<token>` upgrades to a WebSocket that sends JSON messages:

- `{"type": "config", "config": {...}}` on connect and whenever settings change. With the dashboard token this is the same body as `GET /api/v1/config` (with `"scope": "admin"`). Dock and overlay tokens get only what those pages display: `{"scope": "control" | "read", "config": {"delay": {...}, "overlay": {...}}, "urls": {"obs_server": "..."}, "version": "...", "ui_build": "..."}`. `ui_build` names the script the built-in pages start from; the pages reload themselves when it changes (after an update, OBS may still run the old ones).
- `{"type": "state", "state": {...}}` on connect and whenever the state changes (up to 4 times per second).
- `{"type": "overlays", "count": 1}` on connect and whenever it changes: how many overlay pages are connected. The overlay page adds `&role=overlay` to the socket's address to be counted (its preview on the dashboard does not), so the dock can warn when nothing would cover a Mask change or a dump.

The server ignores what clients send, apart from closing the socket; messages over 64 KiB close the connection.

## Settings

| Method and path | Body | Effect |
|---|---|---|
| `GET /api/v1/config` | none | Settings (never including tokens or secrets), link URLs, the stream key OBS should use (`urls.obs_key`), and whether a destination stream key is saved. |
| `PUT /api/v1/config` | Any of `destination`, `delay`, `overlay`, `hotkeys`, `grace_seconds`, `allow_lan` | Partial update. Destination changes apply immediately; `restart_required` says when a restart is needed. A stream key in the destination URL (`rtmp://host/app/<key>`) is moved to the keychain and removed from the URL. If the settings file cannot be written, nothing changes and the answer is `500`. |
| `PUT /api/v1/destination/key` | `{"key": "live_..."}` | Store the stream key in the OS keychain, for the server of the destination in the settings file. |
| `DELETE /api/v1/destination/key` | none | Forget the stored key. |

The saved stream key is stored together with the server it was saved for, and is only ever sent to that server (RTMP and RTMPS, or regional servers of the same service, count as one). Changing the destination to another server also forgets it; if the keychain refuses to remove it, the change is refused (`500`) and nothing changes.

## OBS setup (obs-websocket)

| Method and path | Body | Effect |
|---|---|---|
| `GET /api/v1/obs/status` | none | Whether OBS is reachable, streaming, and already pointed at stream-delay. |
| `POST /api/v1/obs/connect` | `{"host": "127.0.0.1", "port": 4455, "password": "..."}` | Test and save the obs-websocket connection. An empty password keeps the saved one, but only for the same host and port: the saved password is never sent to another address, and is forgotten once another OBS is connected without one. |
| `POST /api/v1/obs/configure` | `{"import_key": true, "add_overlay": true}` | Back up OBS's stream settings (to the keychain, as they hold the stream key and any server password), point OBS at stream-delay, and optionally import the Twitch key and add the overlay. |
| `POST /api/v1/obs/restore` | none | Put OBS's original stream settings back. Only works with the OBS they were read from (`409 Conflict` otherwise). |

OBS counts as on this computer when its address is `localhost`, a loopback address,
or one of this computer's own network addresses (OBS's WebSocket settings show the
latter). For an OBS on another computer, `configure` needs stream-delay's RTMP input
to accept streams from the network (`--ingest 0.0.0.0:1935`), else it answers
`409 Conflict`.

## Updates

| Method and path | Body | Effect |
|---|---|---|
| `POST /api/v1/updates/check` | none | Dashboard token only. In the desktop app, checks for an update and asks before installing it: `{"checking": true}`. Other builds (`streamdelayd`, Docker) answer `{"checking": false, "releases": "<url of the latest release>"}`. |

## Health check

`GET /healthz` needs no token and returns `{"status": "ok", "app": "stream-delay", "version": "0.2.0"}`.
The desktop app uses it to tell when another copy of stream-delay already holds its
ports.

## Diagnostics

| Method and path | Body | Effect |
|---|---|---|
| `GET /api/v1/diagnostics` | none | A JSON file for bug reports: version, OS, settings, state and the last 2000 log lines. Stream keys (including one in the destination URL), the ingest key, passwords, all API tokens, `live_…` keys, `token=` values and your home directory path are removed. Sent as a download (`Content-Disposition: attachment`). |
| `POST /api/v1/diagnostics/link` | none | `{"url": "/diagnostics/<code>"}`: a link that downloads the same file once, within a minute, without a token. For browsers, which keep the address of every download. |

The dashboard's **Advanced** tab has a *Download diagnostics* button, and
`streamdelayd diagnostics -o diagnostics.json` saves the same file from a running
instance.

## Examples

```sh
TOKEN=...   # from `streamdelayd urls`
curl -X PUT  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
     -d '{"seconds": 45}' http://127.0.0.1:7788/api/v1/delay
curl -X POST -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
     -d '{"when": "after-air"}' http://127.0.0.1:7788/api/v1/live
```

With the CLI (it reads the token from the config file):

```sh
streamdelayd delay 45
streamdelayd delay 30 --mask
streamdelayd live --after-air
streamdelayd dump                # throw away what has not aired
streamdelayd end --after-air     # end once the buffer has aired
streamdelayd state
```

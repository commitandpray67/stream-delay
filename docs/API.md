# Local API

stream-delay serves an HTTP and WebSocket API on `http://127.0.0.1:7788` (configurable). The dashboard, the OBS dock, the overlay, the `streamdelayd` CLI and third-party tools (Stream Deck, Streamer.bot, Touch Portal, scripts) all use it.

## Authentication

Every `/api` request needs the install's token, which is generated on first run and stored in `config.toml`. Pass it in one of these ways:

- `Authorization: Bearer <token>` (preferred)
- `X-Stream-Delay-Token: <token>`
- `?token=<token>` (for browser sources and WebSockets, which cannot set headers)

Requests must also use a loopback `Host` (`127.0.0.1`, `localhost` or `[::1]` with the right port), and browser requests from another origin are refused. Both checks stop malicious web pages from controlling your stream. LAN access can be enabled in settings; the token is still required.

Run `streamdelayd urls` to print links that already contain the token.

## Delay control

| Method and path | Body | Effect |
|---|---|---|
| `PUT /api/v1/delay` | `{"seconds": 30, "mode": "rewind" \| "mask"}` | Set the delay. `mode` defaults to the configured default. `0` goes live. |
| `POST /api/v1/live` | `{"when": "now" \| "after-air"}` | Drop the delay at the next keyframe, or after everything buffered so far has aired. |
| `POST /api/v1/presets/{index}` | none | Apply a configured preset (0-based). |
| `POST /api/v1/cancel` | none | Cancel a pending change (for example a mask in progress). |

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
              "bitrate_kbps": 6150, "backlog_bytes": 0, "reconnects": 0 }
}
```

`phase` is one of the following:

| Phase | Meaning |
|---|---|
| `offline` | No encoder is connected. |
| `live` | No delay. |
| `delayed` | A delay is in effect. |
| `adding` | Mask mode is filling the buffer while the slate is up. |
| `going-live` | Waiting to go live. |
| `reducing` | Waiting for a keyframe to shorten the delay. |

## Events (WebSocket)

`GET /api/v1/events?token=<token>` upgrades to a WebSocket that sends JSON messages:

- `{"type": "config", "config": {...}}` on connect and whenever settings change (the same body as `GET /api/v1/config`).
- `{"type": "state", "state": {...}}` on connect and whenever the state changes (up to 4 times per second).

## Settings

| Method and path | Body | Effect |
|---|---|---|
| `GET /api/v1/config` | none | Settings (never including the token or secrets), link URLs, and whether a stream key is saved. |
| `PUT /api/v1/config` | Any of `destination`, `delay`, `overlay`, `hotkeys`, `grace_seconds`, `allow_lan` | Partial update. Destination changes apply immediately; `restart_required` says when a restart is needed. |
| `PUT /api/v1/destination/key` | `{"key": "live_..."}` | Store the stream key in the OS keychain. |
| `DELETE /api/v1/destination/key` | none | Forget the stored key. |

## OBS setup (obs-websocket)

| Method and path | Body | Effect |
|---|---|---|
| `GET /api/v1/obs/status` | none | Whether OBS is reachable, streaming, and already pointed at stream-delay. |
| `POST /api/v1/obs/connect` | `{"host": "127.0.0.1", "port": 4455, "password": "..."}` | Test and save the obs-websocket connection. |
| `POST /api/v1/obs/configure` | `{"import_key": true, "add_overlay": true}` | Back up OBS's stream settings, point OBS at stream-delay, and optionally import the Twitch key and add the overlay. |
| `POST /api/v1/obs/restore` | none | Put OBS's original stream settings back. |

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
streamdelayd state
```

# Security and privacy

stream-delay handles your stream key and runs a control API on your computer, so
it is built to be safe by default:

- **Your stream key** is stored in the OS keychain (Credential Manager, Keychain,
  or Secret Service), or in a file only your user can read if there is no keychain.
  The API never returns it, and it is removed from logs and diagnostics.
- **The control API and web pages listen on `127.0.0.1` only**, unless you enable
  LAN access.
- **Every API call needs a random token** created on first run. Only the
  dashboard link can change settings, stream keys and OBS. The dock link can
  only change the delay, and the overlay link can only show it, so a leaked
  dock or overlay link cannot reveal or redirect your stream key. Still, don't
  show them on stream.
- **Your stream key only goes where you set it:** if you change the
  destination to a different server, the saved key is forgotten.
- **Web pages you visit can't control your stream:** requests must name this
  computer in the `Host` header (which blocks DNS rebinding) and cross-origin
  browser requests are refused.
- **The RTMP input also listens on `127.0.0.1` only** by default. When it can be
  reached from your network, an ingest key is required so nobody else can
  stream to it; if you don't set one, stream-delay generates one and shows it
  with the OBS server address. A wrong key is only refused after a second, so
  keys can't be guessed quickly.
- **Hostile network input:** the RTMP, AMF0 and FLV parsers have hard limits on
  memory use, and are fuzzed and property-tested so malformed data can't crash
  them. Until an encoder has started publishing it may only send small
  messages, must publish within 15 seconds, and one address can hold at most
  four connections, so strangers cannot exhaust memory or lock OBS out.
- **`--ephemeral` runs** keep secrets in memory only.
- **No telemetry, no accounts, no servers of our own.** stream-delay only connects
  to the destination you choose, to OBS if you set up obs-websocket, and to GitHub
  to check for updates.

{{#include ../../../SECURITY.md}}

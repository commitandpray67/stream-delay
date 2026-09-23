# Security and privacy

stream-delay handles your stream key and runs a control API on your computer, so
it is built to be safe by default:

- **Your stream key** is stored in the OS keychain (Credential Manager, Keychain,
  or Secret Service), or in a file only your user can read if there is no keychain.
  The API never returns it, and it is removed from logs and diagnostics.
- **The control API and web pages listen on `127.0.0.1` only**, unless you enable
  LAN access.
- **Every API call needs a random token** created on first run. The dock and
  overlay links carry it, so don't show those links on stream.
- **Web pages you visit can't control your stream:** requests must name this
  computer in the `Host` header (which blocks DNS rebinding) and cross-origin
  browser requests are refused.
- **The RTMP input also listens on `127.0.0.1` only** by default. When you open it
  to your network, set an ingest key so nobody else can stream to it.
- **Hostile network input:** the RTMP, AMF0 and FLV parsers have hard limits on
  memory use, and are fuzzed and property-tested so malformed data can't crash
  them.
- **No telemetry, no accounts, no servers of our own.** stream-delay only connects
  to the destination you choose, to OBS if you set up obs-websocket, and to GitHub
  to check for updates.

{{#include ../../../SECURITY.md}}

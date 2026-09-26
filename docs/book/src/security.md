# Security and privacy

stream-delay handles your stream key and runs a control API on your computer, so
it is built to be safe by default:

- **Your stream key** is stored in the OS keychain (Credential Manager, Keychain,
  or Secret Service), or in a file only your user can read if there is no keychain.
  The API never returns it, and it is removed from logs and diagnostics. The
  backup of OBS's stream settings made by the OBS setup, which holds the key and
  any server password, is stored the same way.
- **The control API and web pages listen on `127.0.0.1` only**, unless you enable
  LAN access.
- **Every API call needs a random token** created on first run. Only the
  dashboard link can change settings, stream keys and OBS. The dock link
  controls the stream: it changes or removes the delay, dumps the buffer, ends
  the stream and resumes it. The overlay link can only show the delay. So a
  leaked dock or overlay link cannot reveal or redirect your stream key, but
  anyone with the dock link can control your broadcast: keep it as private as
  the dashboard link, and don't show either on stream.
- **Your stream key is sent encrypted:** Twitch and YouTube are reached over
  RTMPS. Their RTMP addresses are there for networks where RTMPS doesn't get
  through, marked *unencrypted*.
- **Your stream key only goes where you set it:** if you change the
  destination to a different server, the saved key is forgotten, as it is when
  you change from an encrypted (RTMPS) address to an unencrypted (RTMP) one,
  even on the same service. Likewise the
  saved OBS WebSocket password is only ever sent to the OBS it was entered for,
  and OBS's original settings are only restored to the OBS they came from.
- **Web pages you visit can't control your stream:** requests must name this
  computer in the `Host` header, which blocks DNS rebinding, and cross-origin
  browser requests are refused. Without LAN access that means `localhost` or
  `127.0.0.1`; with it, also an IP address, a local name (`nas`,
  `gaming-pc.local`, names under `.home.arpa`, `.internal` or `.lan`), or a
  name you list under `allowed_hosts` in the `[api]` section of the settings
  file, such as one on your own DNS.
- **The RTMP input also listens on `127.0.0.1` only** by default. When it can be
  reached from your network, an ingest key of at least 16 characters is
  required so nobody else can stream to it; if you don't set one, stream-delay
  generates one and shows it with the OBS server address. A wrong key is only
  refused after a second, an address that sends five wrong keys has to wait a
  minute, and while wrong keys come from many addresses at once, each gets one
  try every 10 minutes. These limits are for other devices: connections from
  this computer are not limited. A tunnel or proxy on this computer that
  forwards the input (an SSH tunnel, ngrok, a reverse proxy) makes every
  encoder look local, so nothing slows down guessing: expose the port directly
  instead, and keep the generated key.
- **Hostile network input:** the RTMP, AMF0 and FLV parsers have hard limits on
  memory use, and are fuzzed and property-tested so malformed data can't crash
  them. Until an encoder has started publishing it may only send small
  messages and must publish within 15 seconds. One address (for IPv6, one /64
  network) can hold at most four connections, and when all connection slots are
  taken the oldest one that is not streaming is closed to make room, so strangers
  cannot exhaust memory or lock OBS out.
- **API tokens** must be at least 16 characters; a generated one has 32. A
  shorter one set with `--token` or `STREAMDELAY_TOKEN` is refused at startup.
- **`--ephemeral` runs** keep secrets in memory only.
- **Logs:** `streamdelayd run` prints the links' tokens and the OBS key only to
  a terminal, not to `docker logs` or a service's log; `streamdelayd urls`
  shows them. The diagnostics file for bug reports leaves out keys, passwords,
  tokens and public IP addresses.
- **No telemetry, no accounts, no servers of our own.** stream-delay only connects
  to the destination you choose, to OBS if you set up obs-websocket, and to GitHub
  to check for updates.

{{#include ../../../SECURITY.md}}

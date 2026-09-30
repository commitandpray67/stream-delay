# Changelog

All notable changes to stream-delay are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.3.2] - 2026-09-30

### Added

- `streamdelayd health` exits with 0 when an instance is running and answering
  (no token needed), and the Docker image uses it as its health check, so
  `docker ps` shows `healthy`.
- **When the connection to the destination comes back** (Delay settings tab,
  `delay.after_reconnect`): *Resume where it left off* (`keep`, the default,
  as before: viewers miss nothing and the delay grows by the outage), or *Go
  back to the delay set* (`restore`: at the first keyframe old enough, so
  viewers miss what was recorded while it was down). Takes effect without a
  restart.
- `--token-file` (or `STREAMDELAY_TOKEN_FILE`) gives `streamdelayd run`,
  `streamdelayd urls` and the client commands their API token from a file (a
  Docker or systemd secret, for example). Like `STREAMDELAY_TOKEN`, it keeps the
  token out of the command line, which other users of the computer can see;
  `--token` still works but is not recommended.

### Changed

- Twitch and YouTube are reached over RTMPS, which encrypts the stream key.
  Settings on their earlier default RTMP addresses move to it, keeping the saved
  key. The RTMP addresses are still there, as *Twitch (RTMP, unencrypted)* and
  *YouTube (RTMP, unencrypted)*, for networks where RTMPS doesn't get through.
- When the RTMP input can be reached from other devices, its ingest key must have
  at least 16 characters (a generated one has 32); a shorter one is refused at
  startup. The example key in the Docker instructions, `choose-a-secret`, was
  too short: leave `STREAMDELAY_INGEST_KEY` unset to use a generated key.
- While wrong ingest keys come from many addresses at once (20 in a minute), each
  address gets one try every 10 minutes. Addresses that sent no wrong key, like
  your encoder's, are not held up. The key comparison no longer reveals the key's
  length through its timing.
- With LAN access (always on in the Docker image), the dashboard and API only
  answer requests that name this computer by an IP address, a local name (`nas`,
  `gaming-pc.local`, names under `.home.arpa`, `.internal` or `.lan`) or a name
  listed in the new `allowed_hosts` setting under `[api]`. Other names are
  refused, which blocks DNS rebinding in LAN mode too.
- `streamdelayd run` prints the links' access tokens and the OBS key only to a
  terminal, not to `docker logs` or a service's log. `streamdelayd urls` shows
  them (`docker exec <container> streamdelayd urls`).
- The diagnostics file for bug reports masks public IP addresses, such as a
  server's or those of whoever connected to it, also where other text is joined
  to them (`could not reach 8.8.4.4:1935: timed out`, `ip:8.8.4.4`).
- A destination that refuses the stream (a wrong stream key, for example) is
  tried again after 10 s, 30 s, 1 min and 2 min, then every 5 min, rather than
  every 10 s for as long as OBS streams, and the status says when. A new key or
  destination is tried at once.
- Installing an update while you are streaming asks first, as quitting does, and
  ends the stream cleanly before installing. On Windows the broadcast was cut
  off, and what was in the delay buffer lost.
- Removing the saved stream key on the Setup tab takes a second click, and
  while you are streaming it says that this ends the broadcast: without its
  key, the destination refuses the stream.
- A dump no longer airs you live when nothing covers the stream. It now does
  the first of three things that can happen: a **replay** of the last stretch
  (Rewind, with enough buffer), the overlay slate covering the stream while the
  delay builds back up (**cover**, only while OBS has the overlay on stream), or
  else a **hold**: viewers see the last frame they saw, still, until what you
  do after the dump has the full delay. Before, a Mask dump, or a Rewind dump
  without enough buffer, with the rolling buffer off, while the destination was
  briefly disconnected, or when the connection did not answer the dump within
  2 s, showed the slate whether or not an overlay was there to show it, and
  aired the stream almost live under it. The dock says which one a dump will
  do before you confirm, and which one it did; the tray and hotkeys show it as
  a notification, and `streamdelayd dump` prints it. The API answers with
  `"dump": "replay" | "cover" | "hold"`; `"allow_uncovered": true` asks for the
  slate with no overlay connected. A new phase, `holding`, shows while a hold
  lasts.
- While the slate covers a dump, the sound of what it covers (which airs almost
  live) is left out until the delay is back. **Leave out the sound while the
  slate covers a dump** on the Delay settings tab (`delay.mute_under_slate`)
  turns this off.

- The Mask slate appears at once instead of fading in over 0.35 s, and what OBS
  sends counts as covered by it only once the overlay says it has drawn the
  slate, and 1.5 s after that (was 0.5 s after it was asked for, whether or not
  any overlay drew it: shorter than the fade plus an encoder's look-ahead, so
  the first frames after the switch could show gameplay through a half-drawn
  slate). The margin is a new setting, **Slate margin** on the Delay settings
  tab (`delay.mask_margin_ms`, 500 to 5000 ms); raise it for encoders with
  look-ahead. Mask changes and dumps show the slate about a second longer. If
  no overlay says it drew the slate within 2 s, a Mask change goes ahead with a
  warning that viewers may have seen some of the stream twice, and a dump holds
  the last frame instead.
- The overlay in OBS tells stream-delay whether OBS has it on stream (in the
  live scene), and only then can a dump use the slate. OBS doesn't say so for
  an overlay that is already on stream when OBS starts or the overlay is
  refreshed: hide and show it once (the eye icon); the dock says when that is
  needed. The `overlays` event gains `active`, the number of overlays on
  stream; the state gains `slate_change`. Anyone with the overlay link can now
  report these, so keep it as private as the dock link.

- Building from source needs Rust 1.89 or newer. The declared minimum was 1.88,
  but two dependencies already needed 1.89; CI now checks the minimum. Builds
  and releases use the exact Rust version in `rust-toolchain.toml`.
- The Docker images' base images are pinned by digest, so a rebuild uses the
  same ones until an update is reviewed.

### Fixed

- An encoder sending its decoder configuration again, or a new one (after a
  resolution change, say), could leave a keyframe still in the delay buffer
  without it: after the destination reconnected, or a delay change went back to
  it, the picture could not be decoded until the next configuration.
- The record of which decoder configuration the destination has kept a copy of
  each, outside the RAM cap: an encoder with many tracks could make it hold
  hundreds of MiB.
- Connecting to the destination tried its addresses one after another, so one
  that never answers (IPv6 on a network where it doesn't work) used up the whole
  10 s every time and the stream never got through. They are now tried in turn
  250 ms apart, the first to answer winning.
- The OBS setup took any server with "twitch.tv" anywhere in it for Twitch, and
  sent its key there. It now goes by the server's host name.
- The OBS status on the dashboard showed a login, query or stream key that OBS's
  server URL contained. A URL whose host cannot be told apart from its login (a
  password with a `/`, `?` or `#` in it) is shown as `rtmp://…`.
- An error in the settings file could quote a value in backticks, such as a
  stream key typed into `key_mode`, or the part of a value after a quote
  character in it. Values are left out of these errors entirely.
- The user guide said the dock link could only change the delay. It controls the
  stream (the delay, Dump buffer, End stream, resuming), so keep it as private as
  the dashboard link.
- A stream started with no delay (or a short one) showed "Delayed 2 s" and the
  overlay's "Stream delay: 2 s" for the whole broadcast: connecting to Twitch
  takes a moment after OBS starts sending, and the broadcast kept that lag. It
  now catches up at the next keyframe, a second or two after it starts.
- Over RTMPS, a destination refusing the stream key showed "read failed: peer
  closed connection without sending TLS close_notify" and was tried again
  every few seconds: Twitch refuses a key by closing the connection without
  ending TLS first. It now reads "the destination closed the connection (check
  the stream key)", and the waits between tries grow as for any refusal. A
  connection closed that way while streaming reads "the destination closed the
  connection".
- Asking for a delay a hair below the one in effect could leave the change
  pending for good, with "Changing delay…" in the dock until Cancel or another
  preset. A rewind rounds back to a keyframe, so asking for 5 s can give 6.001 s,
  and asking for 6 s after that did it. Within half a second now counts as there.
- After a dump, a delay change could settle several seconds over the one asked
  for (12 s asked, 15.6 s given): the dump leaves a gap in the buffer, and the
  keyframe the change landed on was the last one before it. The delay now comes
  down to the one asked for once the stream after the gap is old enough. Other
  delay changes are unchanged. A change soon after a dump or several encoder
  restarts in a row can still end up a few seconds over; pressing the preset
  again usually fixes it.
- On Windows, a dump that reset the connection to a destination barely taking
  data could count one message more as delivered than the destination had (its
  last few bytes were missing), and a rewind dump could then air it after the
  dump. Windows counts what it probes a closed window with as sent; up to a
  network packet more now counts as not yet acknowledged then.
- Cancel, while the slate covered a dump, took the slate down and aired what
  was recorded after the dump almost live, uncovered (or, before an overlay
  confirmed the slate, sent nothing at all until the delay was back). A dump
  building the delay back up (its replay, slate or hold) can no longer be
  cancelled, and the dock offers Cancel only for a change it stops; the state
  gains `cancellable`.
- Setting a delay during a dump rewound into what had been recorded since the
  dump and aired it sooner than either delay: pressing a longer preset while
  the last frame held aired the stream about 10 s behind live. It now sets
  how long the dump waits. Pressing the same preset during a hold no longer
  ends the still frame (the stream had no video until the delay was back), a
  replay that runs out before what comes next is due holds its last frame, and
  the same Mask change pressed twice no longer puts the slate up anew.
- When the connection to the destination dropped during a dump's hold, the
  new connection got nothing until the delay was back (a destination can take
  that long a silence for the end of the stream). The last frame now holds on
  the new connection too, after the stream's metadata and decoder
  configuration.
- With B-frames (the default in OBS), a dump's hold started with the held
  frame timed before frames already sent: the stream's presentation times
  went back, which players and transcoders take as a broken stream. The held
  frame now comes after everything already sent.
- With HEVC from an encoder whose open GOPs have decodable leading pictures
  (RADL), the frames right after a delay change could be presented before, or
  at the same time as, frames already sent. Those leading pictures are now
  left out after a change, as the undecodable ones (RASL) already were.
- A dump waits for the connection to the destination to say what it has sent
  (up to 2 s when the connection is stuck). Going live in that time, or
  setting a delay under half a second, left nothing to dump. The dump was
  then refused, and what the connection had thrown away was sent again: what
  going live skipped aired after all, in a burst, with timestamps going back.
  Nothing is sent again now: the stream goes on live, or from the next
  keyframe.
- `streamdelayd urls` showed the token and ingest key saved in the settings
  file even when `run` was given others: with `STREAMDELAY_INGEST_KEY` in
  Docker it said the OBS key was "any", and with `STREAMDELAY_TOKEN` its links
  were refused. It now reads the same variables (and takes the same options).
  An empty `STREAMDELAY_TOKEN` counts as unset, instead of keeping the client
  from using the settings file's token.
- `streamdelayd urls` always gave `127.0.0.1` in its links and OBS server
  address, so with the API or the RTMP input on an IPv6 address (`[::1]`, or
  `[::]` on Windows) or on one address of the computer (`192.168.1.5`) they
  did not connect. It now gives the addresses `run` prints. Nothing changes
  for the default settings or the Docker images.
- The custom delay field in the dock and on the Control tab, once emptied
  again after typing, counted as 0: Set (or Enter) removed the delay at once.
  Set now does nothing until a number is typed.
- The overlay's title and subtitle were refused as too long when written in
  another script (Cyrillic, Greek, Japanese) at about half the characters
  the fields allow: the server counted bytes. It counts characters now.
- Two actions could be given the same hotkey, and the desktop app then gave
  the key to one of them without saying so: with End stream now and Dump
  buffer on one key, pressing it ended the stream. Saving hotkeys like that
  is now refused, naming both actions (however the keys are written).
- Building the container image from source (`docker build .`, with
  `Dockerfile`) failed since the desktop app joined the Rust workspace: the
  build context left the app out, and Cargo could not load the workspace.
  The release workflow now builds that image too.
- On the Delay settings tab, an emptied "Wait for OBS to reconnect" field
  was saved as 0 s: from the next start, stream-delay no longer waited for
  OBS to come back after a crash or a dropped connection. Saving now asks
  for a number in every field.
- The slate took the overlay's background color as set, alpha included
  (`#0e0e1080`, which the API and `config.toml` accept; the color picker
  does not offer it), so it could be see-through. What airs under the slate
  is nearly live, and showed through. The slate now always uses the color
  without its alpha; the badge keeps it.
- The overlay in OBS could report the slate as painted before it had the
  overlay settings it draws the slate with (if they had not arrived), so the
  slate counted as covering the stream while it was not on screen. It now
  reports only a slate it has drawn.
- After stream-delay restarted while OBS kept the overlay open (quit and
  started again, not an update, which reloads it), the overlay did not
  confirm the slate for a change numbered as one it had confirmed before
  the restart, so that change's slate counted as unconfirmed. It now
  confirms the slate anew after every reconnection.
- The desktop app's update question said whether installing ends the stream
  only as it was when the question came up. Left open (it appears at
  start) until after a stream started, Install ended the stream without
  that warning. The question is now asked again, with the warning, if a
  stream started meanwhile.
- With the dashboard's API on an address other than `127.0.0.1` (an IPv6
  address such as `[::1]`, or one address of the computer), starting the
  desktop app while it was already running did not find the running copy: it
  moved to other ports and saved them, which broke the OBS server address and
  the dock link. It now looks for the running copy where the settings say the
  API listens.
- Opening the desktop app again while it ran (which only brings up its
  dashboard) replaced `stream-delay.previous.log`, the log of the run before
  that the troubleshooting page says to attach after a crash, with the running
  copy's log, which then carried on under that name. Opening it again now
  leaves the logs alone.
- With passthrough (the Setup tab's *Use the stream key entered in OBS
  instead*), setting up OBS automatically replaced OBS's stream key with
  stream-delay's own (`streamdelay`), which passthrough then sent to the
  destination, and the destination refused the stream. OBS now keeps its own
  key; if it has none, the setup says so and changes nothing. Running the
  setup again after stream-delay's port changed no longer replaces the backup
  of OBS's original settings with its settings for stream-delay.
- After setting up OBS on the Setup tab moved a Twitch key in (which makes
  Twitch the destination, with the key stored), the tab went on showing the
  destination and passthrough setting from before, and saving it put them
  back: with passthrough, OBS's key was then stream-delay's own, forwarded to
  Twitch, which refused the stream. The tab now follows the saved
  destination, keeping only what is being typed.
- Once an ingest key is required (the RTMP input reachable from the network),
  an OBS set up before, with the key for this computer only, was shown as set
  up, and setting it up again changed nothing, while stream-delay refused its
  stream. It is now shown as not set up, and setting it up again gives OBS the
  ingest key.
- In the desktop app, the dashboard's links to the user guide, the source code
  and the latest release open in your web browser.
- With a proxy set in the environment (`HTTP_PROXY`, `HTTPS_PROXY` or
  `ALL_PROXY`, as in some company networks, or passed into containers by
  Docker) and no `NO_PROXY` for this computer, `streamdelayd` sent its
  requests to stream-delay on this computer through the proxy: the proxy saw
  the access token, and the commands, and the Docker image's health check,
  failed where the proxy could not reach back. Requests to `localhost` or a
  loopback address now always go directly, as in browsers.
- With more than five presets (added on the Delay settings tab), giving a
  hotkey on the Advanced tab to a preset past the sixth while the ones between
  had none made the tab refuse to save anything, with a message about
  `null`. The ones between are now saved without a hotkey.
- With the RTMP input listening on every interface (as in the Docker image),
  the dashboard's Control tab said to stream to `rtmp://0.0.0.0:1935/live` (or
  `rtmp://[::]:1935/live`), which OBS on Windows cannot connect to. It now
  gives the server address the Setup tab and the dock give.
- The dashboard's Copy buttons said "Copied" when nothing was: over plain
  HTTP from another device, where browsers have no clipboard API, the
  dashboard link (a hidden field) could not be copied at all. It is now
  copied from a field made for it, and a copy that fails says so.
- What the dock and the Control tab said a dump did stayed until the next
  click there: "The slate covers the stream while the delay builds back up"
  was still shown long after the delay was back, even once it had been
  removed from the dashboard. It now goes once the delay is back (a replay:
  once the stretch replayed has aired), the delay is changed from anywhere, or
  the stream ends. Likewise "the delay is shorter than asked" goes once it no
  longer is.
- The Setup tab said to find the stream key in the Twitch Creator Dashboard,
  and offered Twitch's `?bandwidthtest=true`, whatever the service. It now
  names the service chosen, and offers the bandwidth test only for Twitch.
- With a destination but no stream key for it (as installed, after **Remove
  saved key**, or after saving another server, which forgets the key), the
  destination's status was `idle`, waiting for a stream, while OBS was
  streaming: the dashboard said *Waiting for OBS*, and the dock *idle*
  without saying that a key was needed. The state's `egress.status` is now
  `disabled` whenever there is no key to send with (the destination is still
  given), the dashboard says *No stream key*, and the dock asks for one.
- With passthrough where an ingest key is required (stream-delay listening on
  your network, as in Docker), setting up OBS on the Setup tab gave OBS its own
  stream key, which stream-delay then refused, and said OBS was set up; *Set
  up OBS by hand* said to use your real key too. OBS has to stream with the
  ingest key there, and passthrough forwards that key to the destination. The
  wizard now changes nothing and says so, OBS counts as set up only with the
  ingest key, and the Setup tab explains it. The dashboard's settings gain
  `ingest_key_required`.
- For a dump that held the last frame, the tray, hotkeys and `streamdelayd
  dump` said "No overlay is connected", also with an overlay connected that
  OBS had not said was on stream (after OBS starts, until the overlay is hidden
  and shown once). They now say that no overlay is on stream.
- On the Setup tab, saving another setting while typing the OBS address for
  *Connect to OBS* (saving the destination, or the rolling buffer switch) put
  the saved address back in the fields, so Connect went to the old one. What
  is typed is now kept.
- The Overlay tab showed a color set as `#rgb` or with an alpha (`#rrggbbaa`,
  both taken in `config.toml`) as black, and picking another dropped the
  alpha. It now shows them, and a color picked keeps the alpha there was.
- With the API on a loopback address other than `127.0.0.1` or `[::1]`
  (such as `127.0.0.2`) and without LAN access, every request was refused as
  naming an unexpected host, the dashboard's own included. Any loopback
  address is now accepted.
- Dock and overlay links got the destination URL (without its key or query)
  in the state, though the settings they get leave the destination out. It
  is now left out of their state too.
- A custom destination URL with an IPv6 address and no brackets
  (`rtmp://::1/app`) was read as host `::` and port 1, text after the closing
  bracket (`rtmp://[::1]x/app`) was dropped, and port 0 was taken. Each is now
  refused with a message saying what is wrong.
- A destination URL in `config.toml` that does not parse (saved by an older
  version, such as one of those above, or edited by hand) kept stream-delay
  from starting. It now starts without sending anywhere, says so in the log,
  and the Setup tab shows the URL and what is wrong with it when it is saved.
  One given on the command line is still refused at once.
- A destination URL in `config.toml` that does not parse, edited by hand
  with the stream key after the application, was shown as it was in the
  dashboard and the diagnostics file: only a URL that parses has its key
  moved to the secret store. It is now shown like OBS's server address,
  without a login, a query or anything after the application.
- Editing the destination URL as the Setup tab shows it (with `?…` for a
  hidden query, or `/…` for a hidden stream key) but keeping the `…` saved
  `…` in place of the credentials or key. Such a URL is now refused, with a
  message asking for what was hidden.
- With multitrack video that has more than one track in a message, the size
  of the first track's data was read as its composition time, which is used to
  keep timestamps valid where the delay changes. (Multitrack video is still
  not supported, and a warning says so.)
- From an encoder that does not repeat the extended timestamp on the later
  chunks of a message (timestamps past about 4.6 hours), a chunk whose data
  happened to start with those same four bytes lost them, and the connection
  broke. The input now learns which kind of encoder it has from the first
  such chunk. A second `publish` on one connection, and a `createStream` answer
  with an unusable stream id (0, a fraction, NaN), are now refused with a
  message that says so.
- Behind a reverse proxy that adds HTTPS, every change from the dashboard was
  refused as cross-origin (only `http://` pages counted as this server's). A
  page from the same host over HTTPS now counts, and `api.allowed_origins`
  lists others, for a proxy that rewrites the Host.
- When a delay came out shorter than set for lack of buffer, the warning put
  how much is buffered now (which keeps growing) next to the delay fixed at
  the time, ending up at "Only 45 s buffered, so the delay is 12 s instead of
  30 s". It now keeps the numbers of the moment, and once enough is buffered
  says so, with **Set 30 s again** in the dock. The state gains
  `full_delay_ready`.
- Wrong ingest keys could lock the streamer's own encoder out: five from one
  address refused every connection from it for a minute (ten minutes while
  keys came from many addresses), and a fifth connection from one address was
  refused. Behind Docker's port forwarding, a proxy or carrier-grade NAT, the
  encoder shares its address with everyone else. A correct key is now never
  refused: past its tries, an address's wrong keys are answered after 10 s
  (30 s while keys come from many addresses) instead of being turned away, and
  a new connection closes the oldest idle one from its address (one that was
  refused first) instead of being refused itself. The dashboard shows how many
  wrong keys came in the last 10 minutes, and from where.
- An ingest key that is a pattern (`aaaa…`, `abab…`, `abcd…`) is refused like a
  short one, and one weaker than about 80 bits gets a warning on the dashboard
  (it still works, so settings from earlier versions keep starting).
- After the connection to Twitch dropped, the delay stayed longer by the
  outage, but the dock and the overlay's badge still showed the delay set. They
  now show the real one whenever it is more than keyframe rounding explains,
  the dock warns, and the dock and tray offer **Back to 30 s** (your delay),
  which skips ahead at the next keyframe. The state gains `excess_ms`.
- Cancelling a change (such as removing the delay after what is buffered has
  aired) that a connection outage had stretched past the maximum delay left
  the delay there; it now comes back down to the maximum at the next
  keyframe, as after any other outage.
- After a connection outage at a high delay (about 9 s or more at the 120 s
  preset), the output never recovered: each keyframe group was thrown out of
  the buffer just before it aired, so viewers saw a few frames every couple of
  seconds until the delay was changed. Content that has not aired is now kept
  however old it is, and a delay grown past the maximum by an outage comes back
  down to the maximum at the next keyframe.
- A memory cap smaller than the delay at the stream's bitrate did the same, or
  kept the stream from starting at all. A delay the cap cannot hold is now
  refused (`PUT /api/v1/delay` answers 400); when the bitrate rises later, the
  delay comes down to what memory holds, with a warning in the dock. The Delay
  settings tab warns when the cap is small for the maximum delay.
- RTMP timestamps wrap to 0 after 2^32 ms, about 49.7 days (sooner for encoders
  whose timestamps start high). If audio, video or data stepped back slightly
  just after its wrap, it was taken for a jump of 49.7 days ahead and stayed
  that far ahead of the rest, so the destination lost audio/video sync until
  the encoder reconnected. A step back is now a step back.

## [0.3.1] - 2026-09-25

### Changed

- A stream key saved for an RTMPS address is no longer used after the
  destination changes to RTMP, even on the same service (Twitch, YouTube):
  enter it again. RTMP would send it unencrypted.
- With stream-delay listening on every IPv6 interface (`[::]`), the dashboard,
  dock, overlay and OBS links use `[::1]` instead of `127.0.0.1`, which an
  IPv6-only socket (the default on Windows) doesn't accept.
- Release dry runs, which build the installers and install and start them on
  every OS, run for every push to `main` that changes more than documentation.
- The updater's public key is committed (`apps/desktop/updater.pub`), and every
  release is built and checked with it, so a signing key that no longer matches
  it fails the release instead of shipping updates installed copies refuse. The
  `TAURI_UPDATER_PUBKEY` repository variable is no longer used.

### Fixed

- Decoder configuration kept for splices takes at most an eighth of the RAM cap:
  an encoder sending many large configuration messages could hold up to 64 MiB
  whatever the cap. The state's `buffered_bytes` now includes it.
- The dashboard's overlay preview used the dashboard's token; it uses the
  overlay link's, which can only read.
- The container image is pushed only for a published full release whose
  `release-checks.txt` names the tag's commit, also when the workflow is run by
  hand, and only if each Linux archive matches its one entry in
  `SHA256SUMS.txt` (an archive without an entry was let through).
- Release builds skipped the job that adds `SHA256SUMS.txt` and
  `release-checks.txt` although every check passed (GitHub skips a job when any
  job before it was skipped, here the dry-run build a release doesn't make), so
  the container image, which is checked against `SHA256SUMS.txt`, was not pushed
  either. A test now runs the release workflow's jobs as GitHub would, on a tag
  and on a dry run.

## [0.3.0] - 2026-09-24

### Added

- **Dump buffer**: throws away everything viewers haven't seen yet, so it never
  airs, and keeps streaming with the same delay. For the moment something
  happens on stream that must not go out. With Rewind (the default), viewers see
  the last stretch again and the stream continues from after the dump; the delay
  never drops, so no overlay is needed. With Mask, or when the buffer doesn't
  reach back far enough, the overlay slate covers the stream while the delay
  builds back up. In the dock, dashboard, tray menu, API
  (`POST /api/v1/stream/dump`), command line (`streamdelayd dump`) and as a
  hotkey.
- **End stream** now airs what viewers haven't seen yet, then ends the broadcast;
  nothing after the click airs, and **Keep streaming** takes it back until then.
  **End stream now** is the old behavior (the buffer never airs), and explains
  itself the first time it is used. API: `POST /api/v1/stream/end` with
  `{"when": "after-air"}`; command line: `streamdelayd end --after-air`.
- **Check for updates** on the dashboard's Advanced tab, with the version.
- Docks and overlays that OBS kept open reload themselves after stream-delay is
  updated, instead of running the old version.
- The dock warns before a Mask dump when no overlay is connected
  (`{"type": "overlays"}` on the events WebSocket counts them).

### Changed

- **The dock's buttons follow the stream.** Starting a stream is up to OBS; the
  dock shows **Dump buffer**, **End stream** and **End stream now** while you
  are streaming, and after ending tells you to stop and start streaming in OBS
  to go live again (**Resume broadcasting** is on the dashboard and tray).
- "Go live" is now "remove the delay" everywhere, so it isn't mistaken for
  starting the stream: the **0 s** preset (formerly **Live**), **Remove delay
  after it airs** (formerly *Air up to now, then go live*), and a **No delay**
  status (formerly **Live**).
- **Stopping and restarting the stream in OBS before its end has aired** lets
  the old broadcast finish, then starts a new one for the new stream (unless
  you click **End stream** meanwhile). The old broadcast used to carry on with
  no data while OBS was stopped, and Twitch drops a connection that goes quiet
  for about 30 s.
- The overlay's pop-up appears only when you change the delay, once the change
  is in effect ("Stream delay: 15 s", "Stream delay removed"). It no longer pops
  up when a stream starts or reconnects, and says what the delay really is when
  the buffer was too short for the one asked for. Its badge shows the delay you
  set rather than one stretched by a keyframe.
- The desktop app's log file starts again at 16 MB, keeping the full one as
  `stream-delay.1.log`, instead of growing for as long as the app runs.

### Fixed

- **Dump buffer also throws away what was on its way to Twitch.** When the
  upload had fallen behind or stalled, up to about 10 s already handed to the
  connection to Twitch still aired after a dump. Now the dump asks the
  operating system what it has not sent yet: if anything, or if a write was
  under way, the connection is dropped and made again at once, which discards
  it (viewers see a short interruption). This holds however little is waiting,
  on a low-bitrate stream or after the encoder paused too. A Rewind dump still
  rewinds, replaying only what Twitch acknowledged. The dump answers once this
  is done.
- On Linux, memory no longer creeps up over hours of streaming: the buffer's
  blocks are always returned to the system when freed. Before, the process
  could hold about twice the buffered video after a few hours.
- The OBS setup wizard took an OBS at this computer's own network address (the
  one OBS's WebSocket settings show) for one on another computer: it offered to
  set up an OBS that already streamed to stream-delay, then refused.
- "Buffered 2:11 of 2:00": the buffer label no longer shows more than the
  maximum.
- A settings change that cannot be saved (disk full, no permission) no longer
  takes effect anyway: the dashboard could show a new destination while the
  stream still went to the old one. Nothing changes, and the dashboard says why.
  Changes made at the same moment, and their stream keys, are applied one after
  the other; a stream key the change had already saved or removed (or the OBS
  wizard had imported) is put back.
- **A new stream key in an unchanged destination URL is used at once.** Before,
  the relay kept publishing with the old key until the next restart.
- `secrets.toml` is replaced whole on every write, so a crash or a full disk can
  no longer leave it empty. Each secret is kept in one place only: a key the
  keychain refused goes to the file, and if the keychain's older copy cannot be
  removed, saving fails rather than keep two. A damaged file is moved to
  `secrets.toml.damaged` instead of being overwritten.
- OBS setup wizard steps run one at a time, and the saved OBS password and
  settings backup are stored with the OBS they belong to: two steps at once
  (two tabs) could pair one OBS's password with another's address.
- **The OBS wizard saves OBS's password and settings backup with its
  settings, or not at all.** When the settings could not be saved, connecting
  to another OBS had already replaced (or removed) the saved password of the
  one in the settings, and restoring OBS had already deleted the backup the
  settings still named. Now the previous ones stay, and a restore can be tried
  again.
- **The OBS wizard sets up the overlay for an OBS on another computer** with an
  address that computer reaches this one at. It used to give it `127.0.0.1`,
  that computer itself, and report success. When stream-delay's web pages are
  not served to the network, it says so before changing anything. An overlay
  source added earlier with an old address is updated.
- Saving a secret no longer loses the previous one when the keychain accepts the
  new value but the private file cannot be updated: the keychain gets its old
  value back. A file that cannot be read now stops the save before the keychain
  is touched.

### Security

- **A saved stream key is only ever sent to the server it was saved for.** It is
  now stored together with that server. Before, if the keychain failed to remove
  it when the destination changed to another server, the old key could be sent
  to the new one; now such a change is refused, and a key left behind would
  still not be sent. Keys saved by older versions are tied to their destination
  at the first start. For servers other than Twitch and YouTube, "the server"
  now means the same scheme, host, port and application: a key saved for
  `rtmps://host/private` no longer goes to `rtmp://host:1936/other`, nor
  unencrypted to `rtmp://host/private`.
- **Destination replies no longer show stream keys.** A server refusing a stream
  may quote the key it was given; it is removed from the message before it is
  logged or shown, and dock and overlay links no longer see these replies at
  all.
- **Diagnostics files are redacted value by value.** Secrets containing quotes or
  backslashes could be missed, and a redaction that broke the file's JSON fell
  back to the unredacted one.
- **Encoder data waiting to be processed is capped** (32 MB). A publisher sending
  faster than stream-delay takes it in is slowed down instead of filling memory,
  and the buffer's RAM cap counts what each message costs, not just its
  payload, so floods of tiny messages cannot exceed it many times over. The
  1 MB blocks received messages are stored in are capped too (at the RAM cap
  plus 32 MB, across connections): a small message kept in the buffer could
  hold a whole block, so a publisher interleaving tiny kept messages with large
  discarded ones could use about 1 MB per tiny message.
- **On Windows, the settings file and `secrets.toml` are readable only by
  you**: their access list names only your account and inherits nothing from
  the folder. They used to take the folder's permissions, which a custom
  settings location could leave open to others.
- **Error messages about a damaged settings or secrets file no longer quote it.**
  The parser's message showed the offending line, which could hold the API
  token or a stream key, in logs and on screen; it now gives the line number
  and the reason only.
- **What an encoder sends before giving the stream key is kept out of logs**
  beyond 64 characters, a second `connect` on one connection is refused, and
  log lines kept for diagnostics are capped at 2 KB: one connection could make
  stream-delay keep megabytes of text of its choosing.
- **Credentials in a destination URL's query** (`rtmps://host/app?auth=…`, used
  by some servers) are hidden wherever the URL is shown: the state (which dock
  and overlay links see), logs, error messages, the settings page and
  diagnostics show `?…`. Saving the settings page keeps the real query. URLs
  with a user name and password before the host are refused.
- **Dock links no longer see the encoder's address in the replies to End
  stream and Resume**, which returned the full state instead of what those
  links are shown elsewhere.
- **Memory an encoder's sessions keep is bounded.** Each reconnect kept its
  stream metadata for as long as any earlier video was buffered, outside the
  RAM cap: an encoder reconnecting over and over with large metadata and no
  video could fill memory. Sessions without buffered media are dropped,
  metadata is capped at 64 KB, and what sessions keep counts against the cap.
- **API tokens you set yourself may only contain letters, digits and
  `. _ ~ -`.** Links carry the token in their address, where `+`, `&`, `#` or
  `%` changed or cut it, so the dashboard or its live updates did not work.
  stream-delay now says so at startup. Generated tokens are unaffected.
- **Releases:** a tag builds nothing unless CI passes on that commit and the
  versions and changelog match it; releases require the updater keys; every
  asset and update signature is checked before the checksums are added, and
  every update, including the entries for each kind of installation (deb, rpm,
  AppImage, MSI, setup, app), is offered from this release's own GitHub URLs;
  every installer is installed and started on clean machines; and the
  container image is pushed only once the release is published. A draft that
  a rerun changed has no `SHA256SUMS.txt` unless the rerun passed everything:
  every job that changes the draft removes it first (and never changes a
  published release), and `release-checks.txt` names the commit and run that
  passed. v0.2.0 lost
  three of its command-line archives and its `SHA256SUMS.txt` to a packaging
  bug, now fixed.

## [0.2.0] - 2026-09-23

### Changed

- **Stopping the stream in OBS ends the broadcast as soon as the rest has
  aired.** It used to stay connected for the whole grace period (30 s by
  default) with nothing to send, so every stream ended with 30 s of frozen
  video. The grace period now applies only when OBS crashes or loses its
  connection. Stopping and restarting OBS within 30 s therefore starts a new
  broadcast, as it does when streaming to Twitch directly.
- **Reconnecting to Twitch after a drop starts at once**, then after 0.5, 1, 2
  and 4 s and every 5 s after that (it used to wait 1 s first and up to 10 s
  later). Every second spent reconnecting added a second of delay. A refused
  stream (for example a wrong key) is still retried only every 10 s.
- Settings given on the command line apply to that run only: changing settings
  on the dashboard saves just what was changed there, never the command-line
  values, to `config.toml`.
- The dock no longer has a separate **Go live now** button: its **Live** preset
  does the same, and the dock always shows it, even without a 0 s preset. The
  dashboard, tray menu and hotkeys keep both go-live actions.
- API tokens must be at least 16 characters; a shorter `--token` or
  `STREAMDELAY_TOKEN` is refused at startup (generated tokens have 32).
- Settings from the config file and the command line (`--max-delay`, `--delay`,
  `--grace`) are checked at startup with the same limits as the dashboard; the
  grace period can be at most 600 s.
- The Docker instructions publish the dashboard port on the local machine only
  (`-p 127.0.0.1:7788:7788`); see the user guide before exposing it.

### Fixed

- **A finished stream could go live again later.** If Twitch could not be
  reached when OBS stopped (internet down, or a refused stream key),
  stream-delay kept retrying forever, and once Twitch was reachable again it
  started a new broadcast with a leftover frame of the finished stream (which
  can notify followers). It now gives up once the grace period is over and
  discards what never aired. A stream shorter than the delay still airs in
  full, on schedule.
- **A connection to Twitch that died without either side noticing** (after the
  computer switched networks, for example) froze the broadcast until the
  operating system gave up on it, which can take a quarter of an hour on macOS
  and Linux. stream-delay now reconnects once Twitch has taken no data for 20 s,
  and continues from the buffer.
- **`docker stop` (and systemd) did not stop `streamdelayd` cleanly:** the
  request was ignored, Docker killed it after 10 s, and Twitch was not told the
  broadcast had ended. It now ends the broadcast and exits, like Ctrl+C.
- **Running the OBS setup again after stream-delay's port changed** (as the
  troubleshooting guide suggests) replaced the backup of your original OBS
  settings with stream-delay's old address, so **Restore** could not bring them
  back. The backup is now kept.
- The OBS setup pointed an OBS on another computer at `127.0.0.1`, that is at
  itself. It now gives OBS this computer's address, or explains that
  stream-delay only accepts streams from this computer.
- With a maximum delay below 120 s (for example `--max-delay 60`), the default
  120 s preset failed when pressed and the delay settings could not be saved.
  Presets longer than the maximum are now left out, with a warning in the log.
- **End stream while stream-delay was still connecting to Twitch** let that
  connection finish and start the broadcast for a moment. The attempt is now
  dropped before the broadcast starts.
- **OBS's reconnect was refused after a network drop between two PCs**, for up
  to 30 s, because its old connection still looked open ("another encoder is
  already streaming"). A reconnecting encoder now takes over once the old
  connection has been silent for 2 s.
- Quitting stream-delay while live now waits until Twitch has been told the
  broadcast ended (at most a few seconds) instead of a fixed 0.3 s, and the
  desktop app asks before quitting while you are streaming.
- A broadcast whose encoder left no longer waits forever on a destination that
  stopped taking data.
- **A `--dest` for another server got the stream key saved for your usual
  destination** when no `STREAMDELAY_KEY` was given. The saved key now only goes
  to the destination it was saved for (or another server of the same service,
  such as Twitch over RTMPS).
- Memory stays flat at about the buffered data plus 12 MB. With glibc, freeing
  and reallocating the 1 MiB receive blocks kept 20-35 MB more (and the nightly
  soak test failed its memory check); full blocks are now reused.
- A destination URL with a non-ASCII character where `rtmp://` would end crashed
  the request that set it.
- When stream-delay gives up at the end of a stream because Twitch can't be
  reached, the dashboard says so, and an old connection error is no longer shown
  once the destination is working again.
- The desktop app re-registered all global hotkeys whenever any setting changed,
  so a hotkey pressed at that moment could be missed; it now does so only when
  hotkeys or presets change.
- Release builds: without an Apple certificate configured, the macOS build
  failed trying to import an empty one. It now builds unsigned, with a warning.
- The nightly fuzzing job never ran (the repository's `rust-toolchain.toml`
  overrode the nightly toolchain it needs), and one fuzz target no longer
  compiled. Both fixed; CI now checks the fuzz targets compile, and the engine
  target also covers End stream and the rolling-buffer switch.
- The desktop app kept only the current run's log; the previous one is now kept
  as `stream-delay.previous.log`, so the log of a crashed run survives the
  restart.
- **Resume broadcasting aired what OBS sent while the stream was ended.**
  Resuming within the delay after **End stream** aired everything from the moment
  the stream was ended. Now only what OBS sends after resuming airs.
- **End stream on a slow or stalled upload.** It now cuts the connection at once,
  so video the computer had not sent yet is dropped instead of airing, and it no
  longer waits for a stalled connection (which could block resuming or new
  broadcasts for minutes).
- When the upload can't keep up, at most 8 MiB waits to be sent; the rest stays
  in the delay buffer (which has a memory cap) instead of an ever-growing queue.
- Saving two settings changes at the same moment could fail or leave the
  settings file with the older change.
- Links (dashboard, dock, overlay, OBS server) were broken when stream-delay
  listened on an IPv6 address.
- **End stream was undone when OBS reconnected by itself.** After a dropped
  connection OBS reconnects automatically, which resumed the broadcast. Now only
  stopping and starting the stream in OBS (or **Resume broadcasting**) does.
- `POST /api/v1/live` sent without a `Content-Type` header went live at once even
  when the body asked to air the buffer first. The body is now always read, and
  an invalid one is refused.
- *Allow control from other devices* took partial effect as soon as it was saved;
  like the listening address, it now applies at the next start.

### Security

- A wrong ingest key is refused only after a second, and keys are compared in
  constant time, so a network-reachable ingest key can't be guessed quickly.
- The dashboard removes its access token from the address bar once it has
  remembered it, so it doesn't show on stream or in screenshots.
- The backup of OBS's stream settings is kept in the keychain instead of the
  settings file: for a custom server it could include the server's password,
  which also showed in the settings API and diagnostics. Existing backups are
  moved at startup.
- The saved OBS WebSocket password is only sent to the OBS it was saved for, and
  OBS's original settings (with the stream key) are only restored to the OBS
  they came from. Before, connecting to another address sent it the saved
  password, and restoring would hand it the stream key.
- Importing the Twitch key from OBS checks that the destination really is
  Twitch, not just that its address mentions `twitch.tv`.
- Dock and overlay links no longer see the encoder's network address.
- *Download diagnostics* uses a single-use link, so the dashboard's access token
  no longer ends up in the browser's download history.
- The events WebSocket accepts messages of at most 64 KiB.
- Anyone who could reach the RTMP input could keep OBS from connecting by
  holding all 16 connection slots with idle connections. When the slots are
  full, the oldest connection that is not streaming is now closed to make room;
  IPv6 addresses count per /64 network against the per-address limit; and an
  address that sends five wrong ingest keys is ignored for a minute.
- An encoder could make stream-delay keep large receive buffers outside its
  memory limits by starting big messages and abandoning them; those buffers are
  now freed.
- Messages from the destination server are limited to 128 KiB (a hostile server,
  or anyone in the path of a plain `rtmp://` connection, could make stream-delay
  hold up to 64 MiB), and its error text is shortened before it is shown.
- Decoder configuration kept for splices is capped, so an encoder can no longer
  make stream-delay hold data outside the memory cap with many of them.
- GitHub Actions are pinned to commit hashes, so a moved tag of a third-party
  action cannot run in the release job, which holds the signing keys.
- Web pages can no longer be framed by other sites, and responses send
  `nosniff` and `Referrer-Policy: no-referrer`.
- Release builds: the update signing key and Apple credentials are only
  available to tag builds (through the `release` environment, see
  `docs/RELEASING.md`); dry runs sign with a throwaway key.

## [0.1.0] - 2026-09-23

The first public beta. Everything below is new.

### Relay

- Local RTMP server for OBS and other encoders, and an RTMP/RTMPS client to the
  destination (Twitch, YouTube, or any custom server). Media passes through
  unchanged; nothing is re-encoded.
- Supports legacy FLV and Enhanced RTMP (HEVC, AV1, VP9, Opus and others) and
  handles 32-bit timestamp wraparound on long streams.
- Keeps the destination connected when the encoder drops, for a grace period
  (30 s by default), and continues seamlessly when it comes back.
- Reconnects to the destination with backoff and resumes from the buffer, so
  viewers miss nothing.
- Ingest key for setups where the relay listens on the network. It is required
  then; if none is set, one is generated, saved and shown with the OBS server
  address.

### Delay

- Change the delay while live. **Rewind** jumps back instantly; **Mask** covers
  the change with an on-stream slate; **go live now**; **air up to now, then go
  live**;
  lowering the delay skips forward. Every change lands on a keyframe with
  continuous timestamps, including B-frame streams and open-GOP HEVC.
- Guarantee: once a delay is in effect, nothing airs sooner than that delay allows.
- **End stream**: ends the broadcast at once without airing what is in the delay
  buffer; **Resume** (or restarting the stream in OBS) starts a new broadcast.
- Optional rolling buffer (Setup): turn it off to keep only what the current
  delay needs; delay is then always added behind the Mask slate.
- A new stream never rewinds into the previous one, and the buffer is released
  when a broadcast ends.
- Presets, a start delay, a maximum delay (120 s by default) and a memory cap.
- Health warnings for long keyframe intervals, a slow uplink, Enhanced Broadcasting
  (multitrack) and not enough buffered history.

### Controls

- Web dashboard, OBS browser dock and a transparent overlay (delay badge,
  change pop-up, Mask slate), all served locally.
- Local HTTP and WebSocket API, protected by a per-install token, a `Host` check
  against DNS rebinding and a same-origin check. Dock and overlay links carry
  tokens that can only control the delay or only read the state; settings,
  stream keys, diagnostics and OBS need the dashboard link.
- `streamdelayd` command line: `run`, `urls`, `delay`, `live`, `state`,
  `diagnostics`.

### Desktop app

- Tray app for Windows, macOS and Linux: icon colored by state, presets in the
  menu, global hotkeys, start with the computer, single instance, updates.
- OBS setup wizard over obs-websocket: backs up OBS's stream settings, points OBS
  at stream-delay, optionally imports the Twitch key and adds the overlay, and
  restores everything with one click.
- Stream keys and passwords are stored in the OS keychain (in memory only with
  `--ephemeral`). A key typed into the destination URL is stored the same way,
  and changing the destination to another server forgets the saved key.
- Moves to another port automatically if 1935 or 7788 is taken by another
  program, and says so if another copy of stream-delay is already running.

### Support and hardening

- **Download diagnostics** (dashboard → Advanced, or `streamdelayd diagnostics`):
  version, settings, state and recent logs, with keys, passwords and tokens removed.
- Memory stays flat on long streams: buffered media is stored in shared 1 MiB
  blocks rather than one allocation per message, which fragmented the heap.
- Limits on memory use for untrusted RTMP input: before an encoder publishes it
  may only send messages up to 64 KiB, must publish within 15 s, and one address
  may hold at most four connections. Fuzzing of the RTMP, AMF0 and FLV parsers
  and the engine; chaos tests (destination resets and stalls, encoder
  crashes); a soak test that watches memory over hours.
- User guide at <https://commitandpray67.github.io/stream-delay/>.

### Known limitations

- Not yet tested widely on real Twitch streams; see `docs/testing.md`.
- Twitch Enhanced Broadcasting (multitrack) and the separate Twitch VOD audio
  track are not supported.
- One destination at a time; the buffer is kept in memory.
- Installers are not code-signed yet (SmartScreen and Gatekeeper warnings).

[Unreleased]: https://github.com/commitandpray67/stream-delay/compare/v0.3.2...HEAD
[0.3.2]: https://github.com/commitandpray67/stream-delay/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/commitandpray67/stream-delay/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/commitandpray67/stream-delay/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/commitandpray67/stream-delay/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/commitandpray67/stream-delay/releases/tag/v0.1.0

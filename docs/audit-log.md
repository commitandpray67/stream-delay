# Audit log

The code is audited in seven areas. Each audit reads the whole area for
correctness and security bugs; every real finding gets a test that fails
first, then the fix. An area is done for a round when a full pass over it
finds nothing real. The weekly audit takes the area audited longest ago.

| # | Area | Paths | What matters most |
|---|---|---|---|
| 1 | Engine | `crates/engine` | Never airs early or airs what was dumped; timestamps and splices stay valid; memory stays bounded; every change finishes |
| 2 | Protocol parsing | `crates/rtmp`, `crates/flv` | Untrusted bytes: bounded memory and CPU, no panics, no desync |
| 3 | Relay | `crates/relay` | Sockets, ingest limits and keys, egress reconnects and dumps, the broadcast lifecycle, shutdown |
| 4 | Control and settings | `crates/control`, `crates/config` | Authentication and scopes, origin and host checks, settings and secrets staying consistent, what the API reveals |
| 5 | Clients | `crates/streamdelayd`, `apps/desktop/src-tauri`, `crates/obs` | Command line, desktop shell (tray, hotkeys, updater), the OBS wizard |
| 6 | Web UI | `ui/src` | What the streamer is told matches what happens; tokens stay out of sight; the overlay's reports |
| 7 | Build and release | `.github`, `Dockerfile*`, `tests`, `fuzz` | Pinned inputs, signing, what a release checks, the test harnesses themselves |

## Entries

Newest first: date, area, what was found and fixed (or that nothing was).

### 2026-09-28 — The web UI in a browser (areas 3, 4 and 6)

The dashboard, the dock and the overlay run in Chromium against
`streamdelayd` with ffmpeg as encoder and destination, the overlay told it
was on stream as OBS does. Each page's text was read every second next to
what the API said, through presets, a Rewind and a Mask change, a dump under
the slate, removing the delay after it aired, End and Resume; then settings
saved on the dashboard while the dock and overlay stayed open. What the
pages said matched the state throughout (the slate went up and was confirmed
for each change, the badge and pop-ups came when the delay settled), no page
logged an error, and no tab scrolls sideways at phone width. Presets, the
rolling buffer switch and the overlay's look reached the open dock and
overlay at once. Four findings, each fixed with a test that failed first:

- Area 6: what the dock said a dump did stayed until the next click there:
  "The slate covers the stream while the delay builds back up" was still up
  70 s after the delay was back and had been removed from the dashboard. It
  now lasts while it holds: until the delay is back (a replay: until the
  stretch replayed has aired), the delay is changed from anywhere, or the
  stream ends. "The delay is shorter than asked" follows the same rule,
  until the state no longer says so.
- Area 6: the Setup tab told everyone to find the stream key in the Twitch
  Creator Dashboard and offered Twitch's bandwidth test, whatever the
  service. It now follows the service chosen.
- Area 6: the Overlay tab's color fields showed `#fff` and `#0e0e10cc`, both
  valid settings, as black, and picking a color dropped the alpha. The value
  saved was right when untouched.
- Area 3: with a destination but no stream key (as installed, after the key
  is removed, or after another server is saved, which forgets it) the
  relay's state said `idle`, waiting for a stream, while OBS streamed: the
  dashboard said *Waiting for OBS*, and the dock's notice for a missing key
  never showed, as it waited for `disabled`, which only no destination at
  all gave. The relay now decides whether it has a key to send with in one
  place, used both to connect and to report `disabled` (at start, when the
  destination changes, and when a stopped connection reports idle).

Considered and left as is: after another server was saved while streaming
and its key entered 5 s later, the delay was 5 s longer, as after any outage
with *Resume where it left off*, and the dock offered to go back. A warning
from the OBS WebSocket library in the log came from `RUST_LOG=info` set for
the run: the default filter leaves it out.

Mutation testing of the relay change: every mutant of the new `stream_key`
and `resting_status` was caught, but four of the relay's initial state
survived (its destination status, listening address and ingest key warning
left out). The relay built that state twice: once to show until its first
update, without the destination's status or address, and once to start
from; tests reading the state at once saw the first, and those reading it
later did not look. A new relay reported a destination with a key as
`disabled` until its first update (well under a millisecond). It is now
built once, and a test reads it both at once and after the first update;
all eight mutants of it are caught.

Also: one run of the workspace tests had one chaos test fail, under the load
of a second run, and which one was not recorded. Since, 8 runs alone and 12
under load (two at once, beside the lifecycle tests) passed.

Areas 3 and 6 are due another full pass.

### 2026-09-28 — Exploratory end-to-end runs (areas 1, 3 and 4)

Three scenarios the end-to-end test does not have, run with ffmpeg as
encoder and as destination (which listened again after each broadcast, so
each broadcast was its own recording), and an overlay emulated over the
WebSocket. Every recording started on a keyframe, decoded without errors,
and its timestamps only rose. No finding:

- End after air, resume, the encoder stopping and starting again, end now.
  The first broadcast held what was sent up to End (18 s, and the 7.9 s a
  rewind replayed); resuming started a new one with what came after; an
  encoder that stops (unpublishes) and comes back starts a new broadcast
  once the old one's end has aired; End now cut the last at once.
- A delay longer than the buffer (history short), a change cancelled, two
  dumps in a row, the encoder killed and back within the grace period (the
  same broadcast went on, the delay absorbing the gap: no gap over 1 s in
  the recording), and a delay set while Remove delay after it airs was under
  way.
- Mask with an overlay: a Mask delay (the slate confirmed, then the delay
  built), a Mask dump with the overlay on stream (`cover`: the last frame
  held until the slate was confirmed and its margin passed, then the stream
  under the slate without its sound), off stream (`hold`: the last frame,
  still and silent), and a Mask change with no overlay (went ahead).

### 2026-09-28 — CI: Windows install test, the dashboard window

The Release dry run of 80c1b86 (a commit with tests only) failed once in
"Install and start (Windows)": the MSI-installed app answered its health
check, served the dashboard and took RTMP within a second of starting, but
had not logged "dashboard opened" 20 s later. The same build passed there
on four commits before it, and none of the 40 Release runs before this one
failed. It could not be run again from here (no permission), so the cause
is not known: the web view being slow to start the first time on a fresh
machine, or never starting. So that the next one tells: the app now logs
"opening the dashboard window" before building it, and the test waits for
the window as long as for the start (90 s) and prints how long it took. It
still fails if the window never opens.

### 2026-09-28 — Area 6 again: the Setup tab and the OBS wizard

Read again with the wizard's changes in mind: the Setup, Delay and Advanced
tabs, the OBS wizard and `lib/types.ts` against what the server sends. One
finding, fixed with a test that failed first:

- The Setup tab loads its destination fields once. The OBS wizard on the
  same tab can change the destination (moving a Twitch key in makes Twitch
  the destination, with the key stored), and the tab went on showing the
  old one; saving it put the old destination and passthrough back. The
  fields now follow the saved destination while they have not been edited.

Considered and left as is: the other tabs load their forms once too, but
nothing on their page changes their settings, and a save from elsewhere is
replaced by the next save from them, as the last one wins.

Area 6 is due another full pass.

Then, every action of the web UI against the rule its other actions keep,
that one click never does what cannot be undone: **Remove saved key** on the
Setup tab removed the stream key at once, and while streaming that ends the
broadcast (the relay reconnects without a key, and the destination refuses
it). It now takes a second click and says so when streaming (test first).
Setting up and restoring OBS are refused while OBS streams. Saving another
destination while streaming moves the broadcast there, which the edit and
Save ask for; the other saves change nothing that cannot be changed back.

Area 5, mutation testing of `streamdelayd` (51 mutants): 10 survived in
`run` and in `shown_to_a_person`, none a bug. Tests were added for what `run`
prints (an `--ephemeral` run's links carry their tokens, which it has no
settings file to show otherwise; to a log, the tokens and the OBS key are left
out and it says where to find them) and for the destination key taken from
`STREAMDELAY_KEY`, which no test covered. Left untested: `--no-keychain` and
whether output goes to a person (a terminal, not a container), which need an
OS keychain or a terminal.

### 2026-09-28 — Area 7 clean; areas 4 and 5: the OBS wizard's stream key

Area 7, read again in full: every workflow, both Dockerfiles and
`.dockerignore`, `deny.toml`, the Renovate settings and the release scripts
(`verify_release.py`, `check_image_inputs.py`, `prepare_release.py`,
`smoke_desktop.py`). No finding: the area is clean for this round. Checked
too: the nightly job's 5000 property-test cases reach every crate's
property tests (the relay's lifecycle test fixes its own 2000, on every
push), and the new `tauri-plugin-opener` passes cargo-deny.

Area 4, read again: `auth.rs`, `routes.rs`, `ui.rs`, `settings.rs`,
`overlays.rs`, `diagnostics.rs`, `changes.rs`, `dest_key.rs`, `bound.rs`,
`obs_routes.rs` and parts of `app.rs` and the config crate. Three findings,
all in the OBS wizard (`obs_routes.rs`, area 5's concern), each fixed with a
test against the fake OBS that failed first:

- With passthrough, the wizard gave OBS stream-delay's own key
  (`streamdelay`), which passthrough then sent to the destination. OBS now
  keeps its own; with none of its own, the wizard refuses (409) and changes
  nothing.
- It knew OBS was set up by stream-delay before (an earlier port) only by
  stream-delay's key in OBS. With passthrough OBS has its own, so running it
  again would have replaced the backup of OBS's own settings. A server on
  this computer with application `live`, while a backup for this OBS is
  kept, now counts too.
- "Set up" looked at the server only: an OBS with the key for this computer
  only, once an ingest key is required, showed as set up and was left so.
  The key now counts where one is required.

Mutation testing of the new code (`is_loopback_host` and the wizard's
`set_up`, `set_up_before`, `key_for_obs` and `own_key`): 5 of 29 mutants
survived at first, each a case no test had (a Host of `[::1]` without a port;
passthrough with an ingest key and OBS holding one of stream-delay's keys;
another RTMP server on this computer, with no backup or on another
application). Tests for each were added; all 29 are caught.

Areas 4 and 5 are due another full pass.

### 2026-09-28 — Area 5 again: the desktop app's Tauri setup

Read: `tauri.conf.json`, `capabilities/default.json`, and how Tauri 2.11
and wry 0.55 handle a page's requests for a new window. The window gets no
Tauri permissions, and the updater key is only set by the release workflow.
One finding:

- The dashboard's links to the user guide, the source code and the latest
  release open a new window (`target="_blank"`). The app set no handler for
  that, and Tauri then leaves it to the web view: read from wry's source,
  macOS and Linux open nothing, and Windows opens a bare web view window.
  They now open in the web browser (`tauri-plugin-opener`, called from the
  app, so the page gets no new permission), and only `http` and `https`
  links do. The test covers which links open; the handler itself runs only
  in a desktop session, which the tests here do not have.

Also run: the `engine` target fuzzed for 30 minutes (3,324,012 runs), with
no crash.

### 2026-09-28 — Areas 5 and 6, full passes

Area 5, read in full: `apps/desktop/src-tauri/src` (`main.rs`, `tray.rs`,
`hotkeys.rs`), `crates/streamdelayd/src` (`main.rs`, `client.rs`) and
`crates/obs/src/lib.rs`. Four findings, each fixed with a test that failed
first:

- 388ea29, 5b3f3de: before moving to other ports, the desktop app checks
  whether its port is taken by a running copy. It asked 127.0.0.1 only, so
  with the API on `[::1]` or a network address a second start moved to
  other ports and saved them. The first fix named the API's address in the
  Host header, which the server refuses for a loopback address other than
  127.0.0.1 without LAN access (found by asking a real server, which the
  test now does, with and without LAN access); it sends `localhost`.
- 5b3f3de: opening the app again while it ran renamed the running copy's
  log to `stream-delay.previous.log`, losing the log of the run before:
  logging starts in `main`, and the single-instance plugin sends the new
  start away only later, in its own setup. The log now holds a lock
  (`stream-delay.lock`); a start that cannot take it leaves the logs alone.
- 7e6004a: `streamdelayd`'s client takes `HTTP_PROXY`, `HTTPS_PROXY` and
  `ALL_PROXY` from the environment (ureq's default), and without a
  `NO_PROXY` for loopback sent its requests to this computer, token
  included, through the proxy. Requests to `localhost` or a loopback
  address now go directly.
- 7e6004a, in area 4's code: without LAN access the Host check accepted
  `127.0.0.1`, `localhost` and `[::1]` only, so an API on `127.0.0.2`
  refused every request. Any loopback address literal is accepted.

Area 6, read in full: every file under `ui/src`. Three findings:

- 1234c98: hotkeys exist for the first five presets. Giving one to preset 7
  left a gap in the list, sent as `null`, and the server refused the whole
  Advanced tab save.
- 1147f0b: the Control tab's "stream to" hint used the address the RTMP
  input listens on (`0.0.0.0` in the Docker image), not the OBS server
  address the Setup tab and dock give.
- 1147f0b: without the clipboard API (plain HTTP from another device), Copy
  fell back to its own field, which for the hidden dashboard link (a
  password field) copies nothing, and said "Copied" either way.

Considered and left as is: rebuilding the tray menu can leave its status
line a state behind until the next change; the command line shows a
refusal the server gives as plain text as "421 Misdirected Request: request
failed"; the OBS wizard's host and port fields are reset to the saved ones
when the settings are sent again (after a reconnection) while being edited.

Also run: `chunk_decoder`, `flv` and `sessions` fuzzed for 10 minutes each
(1,862,376, 150,076,359 and 1,457,883 runs), with no crash; the mutants of
`check_hotkeys` and `hotkey_keys` all caught (7; four had first failed to
build for lack of disk space and were run again).

Both areas found something, so both are due another full pass.

### 2026-09-28 — Areas 5 and 6 again: what the dashboard and the app send

Both areas found something in their last pass, so they were looked at
again, this time from where they meet the settings code: every field the
dashboard sends against the check the server makes, and how the desktop app
uses what is saved. Two findings, each fixed with a test that failed first;
both fixes are in the settings code (area 4):

- a538262: two actions could be given the same hotkey. The desktop app
  registers a key once, so it went to the first action registered, End
  stream now before Dump buffer, with only a line in the log. Saving hotkeys
  like that is now refused, naming both actions; keys compare as the app
  reads them (case, order and other names of the modifiers).
- ee27736: the overlay's title and subtitle fields allow 200 and 400
  characters, but the server counted bytes, so text in another script was
  refused at about half that. It counts characters now.

Every other field agrees with its check (the delay limits and steps, presets
against the maximum, the grace period, colors, the badge position, the
stream key, the OBS port). Areas 5 and 6 are due a full pass again.

### 2026-09-28 — Area 3, relay (third pass)

Read again in full: `egress.rs`, `ingest.rs`, `core.rs`, `lib.rs`,
`lifecycle.rs`, `io.rs` and `heap.rs` (`sendq.rs` for the Windows numbers
below). No real finding.

CI: `a_dump_replays_what_aired_and_never_airs_what_had_not` failed once on
Windows, the dump holding instead of replaying. When a dump finds media
still in the OS it resets the connection, and counts as aired only what
lies further back than what the OS may hold. Windows adds a segment to that
while the other end's window is small, and on loopback a segment is 64 KB
(`Mss 65495` in the numbers of an earlier failed run). The test had sent
less than that in all, so nothing counted as aired and the dump rightly did
not replay. Forcing those numbers on Linux failed the old test every time;
the failure seen is consistent with it. Fixed in the test (677f2b4): frames
of about 1 Mbps, streamed a little longer, pass with the forced numbers, and
a failure now says whether the dump reset the connection.

Considered and left as is: that margin, a full segment, is 64 KB only on
loopback; on a real network a segment is about 1.5 KB, well under a tenth
of a second of stream.

### 2026-09-28 — Area 2, protocol parsing (third pass)

Read again in full what the weekly pass read only in part or not at all:
`chunk.rs`, `amf0.rs`, `ts.rs`, `url.rs`, `session/mod.rs`,
`session/client.rs` and `crates/flv`. No real finding in the parsing.

Hardened, with a test that failed first (64c75d2): the client session's
settings, when printed for debugging, hid the stream key but not the query
of the URL, where some servers take a password (`RtmpUrl` hides both).
Nothing prints them today.

### 2026-09-28 — Area 7, build and release (full pass)

Read: every workflow, both Dockerfiles and `.dockerignore`, the Renovate
settings and the toolchain pin, `deny.toml`, `CODEOWNERS`, the release
scripts (`verify_release.py`, `check_image_inputs.py`, `prepare_release.py`,
`smoke_desktop.py`), the end-to-end harness and the soak's checks.

Fixed (0f1b085), with a test that failed first: building the image from
source (`Dockerfile`) failed. `.dockerignore` left out `apps/desktop`, a
workspace member since the desktop app came in, so Cargo could not load the
workspace; with the build context's files, `cargo metadata` failed. Nothing
built that Dockerfile: the release image packages the release binaries. The
release workflow now builds it (amd64, not pushed), and the checksums wait
for it, as the release workflow's tests now require. It built in the dry
run of 0f1b085.

Checked and sound: inputs reach scripts through the environment, never
pasted into them; actions and base images are pinned by digest; each job
has only the permissions it uses.

### 2026-09-28 — Area 6, web UI (second full pass)

Read again in full: `lib/` (`api`, `live`, `obs`, `overlay`, `format`,
`i18n`), the overlay, the dock, `DelayControls`, `StatusBadge`, `Health`,
`CopyField`, and every dashboard tab with the OBS wizard.

Fixed, each with a test that failed first (cba24a7):

- The custom delay field, typed into and emptied again, removed the delay
  at once when Set (or Enter) was pressed. Svelte gives an emptied number
  field `null`, not `""`, so Set stayed enabled, and `Number(null)` is 0,
  which means "no delay". Set now needs a number.
- For the same reason the Delay settings tab saved an emptied "Wait for OBS
  to reconnect" field as 0 s. The other number fields there were refused by
  the server; the tab now asks for a number in every field before saving.

Checked and sound: the dock shows the encoder connection's last error, which
holds no address (the dashboard alone gets those); the overlay keeps the
slate up while it is disconnected; an emptied port in the OBS wizard is sent
as 0, which cannot connect, and the wizard saves only after connecting.

### 2026-09-28 — Area 5, clients (second full pass)

Read again in full: `streamdelayd` (`main.rs`, `client.rs`), the desktop
app (`main.rs`, `tray.rs`, `hotkeys.rs`, its capabilities and
`tauri.conf.json`) and `crates/obs`.

Fixed, with a test that failed first:

- dbf71b6: `streamdelayd urls` always gave `127.0.0.1` in its links and
  OBS server address, while `run` gives where the API and the RTMP input
  can be reached. With either on an IPv6 address (`[::1]`, or `[::]` on
  Windows) or on one address of the computer, the links did not connect.
  The Docker images pass their addresses on the command line, so
  `docker exec … streamdelayd urls` prints what it did before.

Considered and left as is: once Install is chosen while nothing streams, a
stream started during the download is ended by the install. The streamer
has just agreed to an immediate restart, and the download takes seconds.

### 2026-09-28 — Area 4, control and settings (third pass)

Read again: `auth.rs`, `routes.rs`, `lib.rs`, the startup and links in
`app.rs`, `settings.rs`, `changes.rs`, `dest_key.rs`, `bound.rs`,
`diagnostics.rs`, `overlays.rs`, `ui.rs`, `obs_routes.rs`, and in
`crates/config` the settings file, private files (the Windows access list
included) and the secret store with its keychain. No real finding.

Also traced every log line that could carry a secret: the destination's
`Debug` shows neither its key nor its query, destination errors are
redacted before they are logged, and the OBS target hides its password.
Static files are served through rust-embed, which refuses paths outside
`ui/dist`.

Considered and left as designed: with the API bound to one address that is
not loopback and without `--allow-lan`, every request is refused as
naming an unexpected host, the printed links included. `allow_lan` is what
lets requests name this server by anything but loopback; accepting the
bound address without it would undercut that. The documentation and the
OBS wizard ask for both.

### 2026-09-28 — Area 1, engine (full pass)

Read in full: `lib.rs` (ingest, the output connecting and disconnecting,
commands, dumps and replays, pending changes, splices and output
timestamps, headers, eviction and the memory cap) and `snapshot.rs`. No
real finding: the Mask, Dump, Hold and Replay changes each finish, never
air what was dumped, and keep timestamps rising across splices; eviction
works on whole groups of pictures and stays under the memory cap.

Checked: the engine properties at 5000 cases; `cargo fuzz` on the engine
target for 7 minutes (1,327,760 runs), no crash.

### 2026-09-28 — Area 3, relay (second pass)

A second full pass, reading for what passes over one file at a time miss:
how the ingest, core, egress and engine interact. No real finding.

Traced:

- A dump that resets the destination connection. The egress answers the
  dump before it reports the disconnection, and the core may run the engine
  in between. The dumped part is gone from the buffer either way: a hold
  empties it, and a replay cuts it off. So the disconnection can only move
  the output back to what aired, never into what was dumped. What the
  engine emits in between carries the old connection's generation, and is
  dropped and taken off the backlog.
- What the destination surely has after a reset. The write log counts
  plaintext RTMP bytes, while the OS reports TCP bytes (TLS overhead, and
  control messages the log leaves out). Taking the larger from the smaller
  errs towards "not delivered", so a dump never counts something as aired
  that may not have.
- End stream, a destination change or a shutdown while the connection is
  just coming up. The stop reaches the egress before its first media: the
  control channel is read first, and queued media is dropped on the way
  out. Every buffer discard happens with the connection stopping or off.
- Publisher takeover, eviction for room and shutdown, through the close
  notification, the connection slots and the IDs events carry.
- Backpressure: ingest waits for room in the queue budget without
  reordering one connection's events, and egress writes time out.

Considered and left as designed: a publisher crash-reconnecting before
anything has aired starts the broadcast from the new connection, without
what came before the crash (as a restart does; see the first pass).

### 2026-09-28 — Area 2, protocol parsing (weekly)

The area audited longest ago. Read again: `handshake.rs`, `message.rs` and
`session/server.rs` in full, with the parts of `chunk.rs` and `amf0.rs`
that bound memory. No real finding.

Considered: a publish on a connection that already unpublished is refused
(it never worked; until a22c820 the error said "publish before connect"), and the connection is
then closed; encoders (OBS, FFmpeg) open a new one to publish again.

Checked: the rtmp and flv property tests at 3000 cases; `cargo fuzz` on the
amf0, chunk_decoder and sessions targets for a minute each (2.5 million
runs), no crash.

### 2026-09-28 — Second passes: areas 1, 4, 5, 6, 7

The areas whose first pass found something were read again, starting with
what the first pass skipped.

- Area 1 (engine): the two paths bd9286f changed (a replay that runs out
  early turning into a hold only 2 s or more before what follows is due,
  and a reconnect during a hold or a dump waiting for its slate only
  marking a resync). Clean. Also checked: a repeated slate confirmation
  changes nothing (only the first is recorded), which 54d410e relies on.
- Area 4 (control and settings): the OBS wizard's configure and restore,
  the config file (private, written atomically), the secret store's code,
  startup and the links. One finding, fixed in 900513f: a destination URL
  that does not parse was shown unchanged, so a stream key typed after the
  application (which only a URL that parses can have moved to the secret
  store) reached the dashboard and the diagnostics file. A third look at
  the URL paths this touches (startup, key binding, the relay's
  destination) found one more, fixed in 6398f8e: an edited URL that kept
  the `…` standing for a hidden query or key saved `…` in its place; it is
  refused now. Also corrected: a comment claiming only two places use
  unsafe code (6b47091).
- Area 5 (clients): the OBS browser source, `streamdelayd`'s options and
  its HTTP client (no token sent on across redirects). Clean.
- Area 6 (web UI): the overlay's announcements and badge, and every hint
  and action text against what the engine does in each phase. Clean.
- Area 7 (build and release): after ea2383d, `prepare_release.py`,
  `smoke_desktop.py` and the fuzz targets. Clean.

### 2026-09-28 — Area 7, build and release

Read: every workflow (`ci`, `release`, `publish-image`, `soak`, `fuzz`,
`docs`, `latest-rust`, `real-destinations`), both Dockerfiles, the
Renovate settings, the release scripts (`verify_release.py`,
`check_image_inputs.py`, `prepare_release.py`, `smoke_desktop.py`), the
fuzz targets, and the soak and end-to-end harnesses.

Fixed, with a test that failed first:

- ea2383d: the release check made sure every expected asset was in the
  draft, not that nothing else was: a file left over from another attempt
  would have been checksummed and published. Every release so far holds
  exactly the expected assets; anything more is now refused.

Checked: actions and base images pinned by digest, and the Rust pin the same
in `rust-toolchain.toml` and the source Dockerfile (Renovate moves them
together); least privilege per job, signing secrets only in the `release`
environment (tags), dry runs signing with a throwaway key; the image pushed
only for a published full release whose checks name the tag's commit and
whose archives match their checksums; the release scripts' tests pass (43);
the FLV fuzz target, after area 2's change, 42 million runs without a crash.

### 2026-09-28 — Area 6, web UI

Read: `lib/` (`api`, `live`, `obs`, `overlay`, `format`), the overlay, the
dock, `DelayControls`, and the dashboard's tabs and OBS wizard.

Fixed, each with a test that failed first:

- 31fef77: the slate took the background color with its alpha (the API and
  `config.toml` accept `#rrggbbaa`), so it could be see-through while what
  airs under it is nearly live. It is now always opaque.
- a3cf895: the overlay reported the slate painted from the state alone,
  though it draws it only once it has the overlay settings.
- 54d410e: the overlay remembered the last change it confirmed across
  reconnections, so after stream-delay restarted (it numbers changes from 0
  again) with OBS keeping the page open, a change with the same number
  went unconfirmed.

Checked and sound: the dashboard's token leaves the address bar; dock and
overlay tokens stay separate; confirmations and the dump explanation follow
the live state as it changes.

### 2026-09-28 — Area 5, clients

Read: `crates/obs`, the desktop app (`main.rs`, `tray.rs`, `hotkeys.rs`,
capabilities), `streamdelayd` (`main.rs`, `client.rs`).

Fixed, with a test that failed first:

- 1e3a11f: the desktop app's update question chose its wording when it
  appeared (at start). Left open until after a stream started, Install
  ended the stream without saying so. It is asked again, with the
  warning, if a stream started meanwhile.

Checked and sound: the dashboard window has no Tauri permissions; fallback
ports never move away from a running copy; the log file is bounded; quit
while live asks first; `streamdelayd` keeps tokens out of logs when not at
a terminal.

### 2026-09-27 — Area 4, control and settings

Read: `auth.rs`, `routes.rs`, `lib.rs`, `ui.rs`, `settings.rs`, `changes.rs`,
`diagnostics.rs` (redaction), `overlays.rs`, `obs_routes.rs` (connecting and
secret binding), `dest_key.rs`, `bound.rs`, the startup in `app.rs`, and the
secret store's tests.

Fixed, each with a test that failed first:

- 5b0bb0d: dock and overlay links got the destination URL in the state,
  though the settings they get leave it out.
- 5a87e3c: a destination URL in `config.toml` that does not parse kept the
  app from starting at all, so the dashboard could not fix it. The stricter
  URL parsing of area 2 could lead there on upgrade (a saved port 0). Such
  a URL now gets no destination and the Setup tab names the problem; one on
  the command line is still refused at once.

Checked and sound: token scopes and constant-time comparison, Host and
Origin checks (DNS rebinding), single-use diagnostics codes, redaction of
every stored secret and token, keys bound to their server and OBS secrets
to their OBS, the settings-and-secrets transaction.

### 2026-09-27 — Area 3, relay

Read: `ingest.rs`, `core.rs`, `lib.rs`, `lifecycle.rs`, `egress.rs`,
`sendq.rs`, `io.rs`, `heap.rs`. No real finding.

Traced in particular: every message the core queues for the destination is
taken off the backlog on each path that drops it (idle, backoff, stop, a
stale connection or dump in `publish`), so `backlog_empty` and the room
for more cannot drift; the cut/dump answer on each path; stop and End
stream winning over a connection attempt; ingest writes time out, so a
peer that stops reading cannot hold a task.

Investigated and left as designed: when OBS is stopped and started again
before any of the first stream has aired (a stream shorter than the
delay), the engine drops that first stream when the new one starts
(`Engine::ingest_start`), so the new stream gets a broadcast of its own.
If End stream (after air) was pressed during that first stream, the
stream stays ended until Resume or another restart in OBS: rare, visible
in the dashboard, and nothing unexpected airs.

### 2026-09-27 — Area 2, protocol parsing

Read in full: `chunk.rs`, `amf0.rs`, `handshake.rs`, `message.rs`, `ts.rs`,
`url.rs`, `session/{mod,server,client}.rs`, `crates/flv`. A second pass over
the chunk decoder, the timestamp unwrapper and the link's control messages
found nothing more.

Fixed (0ee7aba), each with a test that failed first:

- `RtmpUrl::parse` read an unbracketed IPv6 address (`rtmp://::1/app`) as
  host `::`, port 1; dropped text after a closing bracket
  (`rtmp://[::1]x/app`); and took port 0. All three are now refused.
- Multitrack video with more than one track per message: the track size
  that follows the track id was read as the composition time.

Considered and left as is:

- AMF0 decoding holds up to about 56 times the bytes it decodes (a strict
  array of one-byte nulls, each a 56-byte value), briefly. Message limits
  bound it: at most 64 KiB before publish (about 3.5 MiB per connection
  being decoded) and 1 MiB for commands and data once publishing (about
  56 MiB, from the one publisher that knows the key). Capping the array's
  preallocation would not lower that bound.

### 2026-09-27 — Area 1, engine

Fixed, each with a test that failed first:

- 5bff8dd: during a dump's slate, Cancel took the slate down and aired
  what was recorded after the dump almost live. Setting the delay during
  a dump rewound into what was recorded after it. A dump now can't be
  cancelled while it builds the delay back up (the snapshot says
  `cancellable`), and a delay set meanwhile becomes the delay the dump
  builds. Also: a replay that runs out early holds its last frame; the
  same Mask change twice no longer restarts it.
- 5b6262e: a reconnect during a dump's hold sent the new connection
  nothing until the delay was back. The hold now continues on the new
  connection, after the metadata and decoder configuration it needs.
- bd9286f: two regressions of mine from the commits above, found on
  re-reading and by the random-operations property. A normal replay held
  a stale keyframe for a moment. A reconnect during a hold left the next
  output with timestamps standing still.

Checked: the engine properties at 5000 cases; `cargo fuzz` on the engine
target for 8 minutes (about 1.2 million runs), no crash.

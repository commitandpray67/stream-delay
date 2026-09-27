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

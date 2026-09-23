# Testing and compatibility

stream-delay is tested at several levels. Everything except the last section runs
automatically.

| Level | What | Where | When |
|---|---|---|---|
| Unit and property tests | RTMP chunking, AMF0, FLV parsing, the delay engine's invariants (never airs early, timestamps always increase, splices land on keyframes) | `cargo test` | every push |
| Integration tests | the relay over real sockets, the API, the OBS wizard against a mock obs-websocket | `cargo test` | every push |
| Chaos tests | destination resets and stalls, a slow uplink, the encoder crashing inside and past the grace period | `crates/relay/tests/chaos.rs` | every push |
| End to end | ffmpeg → streamdelayd → ffmpeg with H.264 B-frames, open-GOP HEVC and the 24-bit timestamp wrap; checks there are 0 decode errors | `tests/e2e/run.sh` | every push (Linux) |
| Fuzzing | chunk decoder, AMF0, FLV, publish sessions, engine | `fuzz/` | nightly, and PRs touching the decoders |
| Soak | hours of streaming with random delay changes; checks that memory stays flat, there are no reconnects and no decode errors | `tests/soak/run.sh` | nightly (2 h); 12 h by hand before a release |

What the automated tests can't cover is how real encoders, Twitch's ingest and
real players react. That is the manual checklist below. Please add your results,
even partial ones, to a
[compatibility report issue](https://github.com/commitandpray67/stream-delay/issues/new/choose)
or a PR that edits this file.

## Test procedure (about 20 minutes)

Do it twice: first as a private bandwidth test, which nobody can watch but which
shows how Twitch's ingest reacts, then as a normal stream (ideally on a second,
test channel) to see what viewers see.

1. On the dashboard's **Setup** tab, save your Twitch key with `?bandwidthtest=true`
   appended, for example `live_123456_abcdef?bandwidthtest=true`.
2. Set OBS up (automatic or by hand) with a 2 s keyframe interval.
3. Open [Twitch Inspector](https://inspector.twitch.tv/) to watch the stream
   health. On the second run, without `?bandwidthtest=true`, also watch the
   channel on another device or browser.
4. Show something with obvious motion and a visible clock in OBS (for example a
   browser with [time.is](https://time.is)), so jumps and repeats are easy to see.
5. Go live in OBS, wait 2 minutes, then walk through the transitions:

| # | Action | Expected |
|---|---|---|
| 1 | Preset 30 s (Rewind) | The last ~30 s play again, then the clock runs 30–32 s behind. No green or smeared frames; audio stays in sync. |
| 2 | Preset 60 s (Rewind) | Jumps back another ~30 s. |
| 3 | Lower to 15 s | Skips forward; the clock is 15–17 s behind. |
| 4 | Mask 20 s | Slate appears at once, stays for ~40 s, and no content is shown twice. |
| 5 | Go live after it airs | Plays up to the moment you pressed, then jumps to live. |
| 6 | Delay 30 s, then **Go live now** | Jumps to live within ~2 s. |
| 7 | Stop OBS's stream, wait 10 s, start again (within the 30 s grace period) | The Twitch connection stays up in Inspector; the stream continues. |
| 8 | Disconnect the network for ~10 s while delayed | stream-delay reconnects; after it, nothing was skipped and the delay is ~10 s longer. |
| 9 | Stream 1 h with a change every few minutes | No Inspector warnings besides the reconnect in step 8; memory in Task Manager/Activity Monitor stays flat. |

For each step, note anything odd: freezes (and how long), artifacts, audio
drift, player errors, and whether the low-latency and normal Twitch players
behave differently. If a transcoded rendition (160p–720p) is offered, check one of
those too.

Then attach the diagnostics file (dashboard → Advanced → **Download diagnostics**)
to your report.

## Compatibility matrix

Legend: ✅ works, ⚠️ works with issues (link the issue), ❌ broken, blank = not tested yet.

### Operating systems and OBS versions

| OBS version | Windows 10/11 | macOS 13+ (Apple Silicon) | macOS 13+ (Intel) | Ubuntu 22.04/24.04 | Other Linux |
|---|---|---|---|---|---|
| 30.x | | | | | |
| 31.x | | | | | |
| 32.x | | | | | |

### Encoders and codecs

| Encoder | H.264 | HEVC | AV1 | Notes |
|---|---|---|---|---|
| x264 (software) | | n/a | n/a | |
| NVIDIA NVENC | | | | |
| AMD AMF | | | | |
| Intel QSV | | | | |
| Apple VideoToolbox | | | n/a | |
| Streamlabs Desktop | | | | |
| ffmpeg (CLI) | ✅ (e2e) | ✅ (e2e) | | Automated end-to-end test |

At the time of writing, Twitch takes HEVC and AV1 only through Enhanced
Broadcasting, which stream-delay does not support yet. Test those codecs against
YouTube or a custom server.

| Audio | Twitch | YouTube |
|---|---|---|
| AAC | | |
| Opus | n/a | |

### Destinations

| Destination | RTMP | RTMPS | Notes |
|---|---|---|---|
| Twitch (`?bandwidthtest=true`) | | | |
| YouTube (unlisted) | | | |
| Kick | n/a | | Custom server, `rtmps://…` from the Kick dashboard |
| ffmpeg as an RTMP server (`-listen 1`) | ✅ (e2e) | | Automated end-to-end test |
| MediaMTX / nginx-rtmp (self-hosted) | | | |

### Viewer side

| Player | Transitions look right | Notes |
|---|---|---|
| Twitch web, low latency on | | |
| Twitch web, low latency off | | |
| Twitch mobile app | | |
| Twitch transcoded rendition | | |
| YouTube web | | |

# Delay modes: what viewers see

stream-delay always changes the delay by jumping to a different point in its
buffer at a keyframe. The connection to Twitch stays up, so viewers keep watching;
what they see around the change depends on the mode.

The one rule it never breaks: **once a delay of D seconds is in effect, nothing
reaches Twitch less than D seconds after it happened**, until you lower the delay
again.

## Adding delay

### Rewind (default, instant)

The stream jumps back D seconds.

```text
You (live):    … 101 102 103 104 | 105 106 107 …        you press "30 s" at 104
Viewers see:   … 101 102 103 104 | 74 75 76 … 104 105 …  the last 30 s play again
```

Viewers see the last D seconds a second time, then the stream continues as normal,
D seconds behind you. Nothing new is revealed at any point. This is the fastest
way to protect yourself: it takes effect immediately.

### Mask (slate covers the change)

The overlay shows a full-screen slate (title, subtitle, optional image; set it up
on the **Overlay** tab) while the delay builds up.

1. You press the button: the slate appears on stream immediately.
2. stream-delay waits until it has D seconds of slate-covered video, then switches
   the delay on.
3. The slate disappears.

Viewers see the slate for about **twice the delay** (a 30 s mask shows the slate
for about a minute) and never see any gameplay twice. Mask mode needs the
[overlay](dock-and-overlay.md#overlay) in your scenes; it does not need
obs-websocket.

## Removing or lowering delay

### Remove delay now

The **0 s** preset. The stream jumps forward to live at the next keyframe (within
about 2 s). What happened during the delay window is skipped.

### Remove delay after it airs

stream-delay remembers the moment you pressed the button. Viewers keep watching
until they reach that moment, then the stream jumps to live; what happens while
that airs is skipped. Use it to let chat see everything up to "now" (for example,
the end of a match) before you drop the delay. It is on the dashboard, the tray
menu and a hotkey; the dock keeps to the presets.

### Lowering the delay (for example 60 s → 20 s)

The stream skips forward to a keyframe so that the delay becomes 20 s. The skipped
40 s are never shown.

## Dumping the buffer

Something happened on stream that must not go out: a wardrobe malfunction, a
doxxing slip, anything against the rules. **Dump buffer** throws away everything
viewers haven't seen yet, so it never airs, and the stream carries on with the
same delay.

- **Rewind** (the default mode): viewers see the last stretch of the stream again,
  as when adding delay, and then it continues from what you do after the dump. The
  delay never drops, so the dump needs no overlay. It needs the buffer to reach
  back about twice the delay (a minute for a 30 s delay).
- **Mask**, or when the buffer doesn't reach back that far, or with the rolling
  buffer off: the overlay slate goes up at once and covers the stream while the
  delay builds back up (about as long as the delay). Only the picture is
  covered: what you say meanwhile airs under the slate, as with any Mask change.
  Without the overlay in your scenes, viewers would see you live meanwhile; the
  dock says so before you confirm.

If your upload has fallen behind, part of what viewers haven't seen is already
queued for Twitch rather than in the buffer. The dump throws that away too, by
dropping the connection to Twitch and making it again at once: viewers see a
short interruption instead. What Twitch had already received by then can't be
taken back, and since stream-delay can't tell how much that was, the dump masks
instead of rewinding.

Press it as soon as you can: only what is still in the buffer can be thrown away.
It takes two clicks in the dock and dashboard (the first time, it explains itself
instead of the first click), and is also in the tray menu, the API and the command
line (`streamdelayd dump`), and can have a hotkey (none by default).

## Ending the stream

Normally you end the stream in OBS, like always: stream-delay airs what is still
buffered, then ends the Twitch broadcast. The dock also has two buttons for it,
shown while you are streaming:

- **End stream** airs what viewers haven't seen yet (the last D seconds), then
  ends the broadcast. Nothing you do after clicking airs, even if OBS keeps
  streaming. Until the end has aired, **Keep streaming** takes it back.
- **End stream now** ends the broadcast on Twitch immediately and throws away
  everything in the delay buffer, so none of it is ever shown. Use it when
  something went wrong and even the delayed part must not air. It cuts the
  connection to Twitch outright, so even on a slow upload, video still waiting to
  be sent is dropped rather than delivered, and if stream-delay is still
  connecting to Twitch, that connection is dropped before the broadcast can start.
  The first time, the dock explains what it throws away (tick **Don't show this
  again** once you know).

Both take two clicks in the dock and dashboard; both are also in the tray menu,
the API and the command line (`streamdelayd end --after-air`, `streamdelayd
end`), and can have hotkeys (none by default).

Quitting stream-delay also ends the stream: it tells Twitch the broadcast is over
before it exits, but what is still in the buffer does not air. The desktop app
asks before quitting while you are streaming.

After ending, OBS can keep streaming to stream-delay; nothing goes out until you
stop and start streaming in OBS (or press **Resume broadcasting** on the
dashboard or in the tray). If OBS loses its connection and reconnects by itself,
the stream stays ended. A new broadcast starts from what OBS sends from then on,
with your current delay.

## Without the rolling buffer

On the **Setup** tab you can turn off **Keep a rolling buffer**. stream-delay then
keeps only what the current delay needs (almost nothing while live), which saves
memory, but there is nothing to rewind into: every delay increase uses **Mask**,
even from presets and hotkeys set to Rewind. Add the overlay to your scenes so the
slate covers the build-up. Lowering the delay, going live and ending the stream
work the same. The switch takes effect immediately.

## Details worth knowing

- **Just after you start streaming**, the buffer can't hold D seconds of history
  yet. stream-delay then uses as much as it has and says so ("Only 12 s of the
  stream was buffered…"). It never quietly weakens the protection you asked for:
  once enough has been buffered, a new change gets the full delay.
- **Keyframes:** jumps land on keyframes, so the delay may be up to one keyframe
  interval (2 s) longer than requested.
- **Maximum delay** is 120 s by default and can be raised on the **Delay settings**
  tab (it needs memory: about bitrate × maximum delay, so 6 Mbps × 120 s ≈ 90 MB).
- **If the connection to Twitch drops,** stream-delay reconnects at once (then
  after ½, 1, 2 and 4 s, and every 5 s after that) and continues from where it
  was in the buffer, so viewers miss nothing; the delay grows by however long the
  outage lasted. Press a preset to bring it back to the delay you want. A
  connection that dies without either side noticing (after your computer switched
  networks, say) counts as dropped once Twitch has taken no data for 20 s.
- **When you click *Stop Streaming* in OBS,** stream-delay airs what is still
  buffered (the last D seconds) and then ends the Twitch broadcast cleanly. With
  no delay, the broadcast ends right away. A stream shorter than the delay (a
  quick test, say) still airs in full, D seconds later.
- **If you start streaming again in OBS before that has aired,** the old
  broadcast still gets its whole end, then ends, and the new stream starts a new
  broadcast D seconds after you started it, as when streaming to Twitch directly.
  (Carrying on with the old broadcast would leave Twitch without data for as long
  as OBS was stopped, and Twitch drops a connection that goes quiet for about
  half a minute.)
- **If OBS crashes or loses its connection,** stream-delay keeps Twitch connected
  for 30 s (configurable on the **Delay settings** tab) and keeps airing what is
  buffered. When OBS comes back in time, the stream continues from its first
  keyframe. Otherwise, once the buffer has aired and the grace period is over, it
  ends the Twitch broadcast cleanly.
- **If Twitch can't be reached when OBS stops** (your internet is down, or the
  stream key is refused), stream-delay stops trying once the grace period is
  over and throws away what never aired. It never connects later to air the
  rest: that would start a new broadcast, and notify your followers, long after
  you stopped.
- **If OBS's connection dies without stream-delay noticing** (for example the
  network between two PCs drops), OBS's reconnect is accepted as soon as the old
  connection has been silent for 2 seconds, and the stream continues.

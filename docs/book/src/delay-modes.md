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

### Go live now

The stream jumps forward to live at the next keyframe (within about 2 s). What
happened during the delay window is skipped and never shown.

### Air up to now, then go live

stream-delay remembers the moment you pressed the button. Viewers keep watching
until they reach that moment, then the stream jumps to live; what happens while
that airs is skipped. Use it to let chat see everything up to "now" (for example,
the end of a match) before you drop the delay. It is on the dashboard, the tray
menu and a hotkey; the dock keeps only **Go live now** to stay compact.

### Lowering the delay (for example 60 s → 20 s)

The stream skips forward to a keyframe so that the delay becomes 20 s. The skipped
40 s are never shown.

## Ending the stream

**End stream** ends the broadcast on Twitch immediately and throws away everything
in the delay buffer, so none of it is ever shown. Use it when something went wrong
on stream and even the delayed part must not air. It cuts the connection to Twitch
outright, so even on a slow upload, video still waiting to be sent is dropped
rather than delivered. In the dock and dashboard it
takes two clicks (the second within 3 seconds); it is also in the tray menu, the
API and the command line, and can have a hotkey (none by default).

OBS can keep streaming to stream-delay; nothing goes out until you press
**Resume broadcasting** or stop and start streaming in OBS. If OBS loses its
connection and reconnects by itself, the stream stays ended. Resuming starts a new
broadcast from what OBS sends from then on, with your current delay.

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
- **If the connection to Twitch drops,** stream-delay reconnects and continues from
  where it was in the buffer, so viewers miss nothing; the delay grows by however
  long the outage lasted. Press a preset to bring it back to the delay you want.
- **If OBS disconnects** (a crash, a network blip, or you click *Stop
  Streaming*), stream-delay keeps Twitch connected for 30 s (configurable on the
  **Delay settings** tab) and keeps airing what is buffered. When OBS comes back
  in time, the stream continues from its first keyframe. Otherwise, once the
  buffer has aired and the grace period is over, it ends the Twitch broadcast
  cleanly. So after you stop streaming, viewers still see the last D seconds
  before the stream ends.

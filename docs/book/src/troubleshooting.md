# Troubleshooting

## Test without going public

Add `?bandwidthtest=true` to the end of your Twitch stream key (on the **Setup**
tab). Twitch accepts the stream but doesn't show it to anyone, and you can watch
the connection in [Twitch Inspector](https://inspector.twitch.tv/). Remove it
before your real stream.

## OBS says "Failed to connect to server"

- Is stream-delay running? The dock or dashboard shows "stream-delay is not
  reachable" if not.
- Check the server address in OBS against the one on the **Setup** tab. If port
  1935 was already in use (by another RTMP server or a second copy of
  stream-delay), the desktop app moves to 19350 or 29350 and shows the new address
  there. Run the OBS setup again (it keeps the backup of your original OBS
  settings), or update OBS by hand.
- With an ingest key (Docker or two-PC setups), OBS must use that key. After five
  wrong keys, stream-delay ignores that computer for a minute.

## "stream-delay is already running"

The desktop app found another copy of stream-delay on its ports, usually
`streamdelayd` still running in a terminal window. Close it (Ctrl+C in that
window), then start the app again. Only one copy can run at a time; the app and
`streamdelayd` share the same settings, stream key and links.

## The dock or overlay is blank, or says the link is missing its token

The links contain a private token. Copy them again from the **Setup** tab or the
tray menu. If you reinstalled or deleted the settings file, the token changed.

## The dock shows old buttons (Go live now, no End stream)

OBS kept an older version of the dock open, or cached it. Open **Docks → Custom
Browser Docks…**, add `&v=2` to the end of the dock's URL, and click **Apply**.
From version 0.3 on, docks and overlays reload themselves after an update.

## The OBS setup wizard says OBS runs on another computer

Versions up to 0.2.0 took OBS's own network address (the one OBS's WebSocket
settings show, such as `192.168.0.10`) for another computer. Update, or enter
`127.0.0.1` as the OBS host on the Setup tab. If OBS really runs on another
computer, start stream-delay with `--ingest 0.0.0.0:1935`; see
[two-PC setups](install.md#two-pc-setups).

## "Keyframe interval is 4.0 s"

Set the keyframe interval to 2 s in OBS (**Settings → Output → Streaming →
Keyframe Interval**, in Advanced output mode). With long keyframe intervals, delay
changes are less precise and Twitch may show buffering.

## "Only 12 s of the stream was buffered"

You asked for more delay than has been streamed so far (for example, 60 s right
after going live). stream-delay used everything it had. Once enough has been
buffered, press the preset again to get the full delay.

## "Upload is falling behind"

Your connection can't send the stream to Twitch as fast as OBS produces it. Lower
the bitrate in OBS. This happens without stream-delay too; stream-delay just shows
it.

## The delay is longer than I set after a connection problem

When the connection to Twitch drops, stream-delay reconnects (the first attempt
comes at once) and continues where it left off, so viewers miss nothing, but the
delay grows by the length of the outage. Press your preset again to go back to
the delay you want.

## The Mask slate doesn't appear

- The overlay browser source must be in the scene you're streaming, at the top of
  the source list, sized to the canvas. See [Overlay](dock-and-overlay.md#overlay).
- Right-click the source → **Refresh cache of current page** if you changed its
  URL.

## "Multitrack video (Enhanced Broadcasting) was detected"

Enhanced Broadcasting isn't supported yet. Turn it off in OBS under
**Settings → Stream**.

## Global hotkeys don't work

- Another program may already use the combination: pick another one on the
  **Advanced** tab.
- On Linux with Wayland, see [Global hotkeys](integrations.md#global-hotkeys-desktop-app).

## Viewers see a short freeze or glitch when the delay changes

A brief stutter at the moment of a jump is expected with some players. If you see
green or smeared frames, or the stream stalls, please
[report it](#reporting-a-problem) with your encoder, codec and OBS version.

## Reporting a problem

1. On the dashboard's **Advanced** tab, click **Download diagnostics** (or run
   `streamdelayd diagnostics -o diagnostics.json`). The file contains the version,
   your settings, the current state and recent log lines. Stream keys, passwords
   and the access token are removed; have a look before you share it.
2. [Open an issue](https://github.com/commitandpray67/stream-delay/issues/new/choose),
   describe what happened and what you expected, and attach the file.

The desktop app also writes a full log to `logs/stream-delay.log` next to the
settings file (see [Where things are stored](install.md#where-things-are-stored)).
The log of the run before is kept as `stream-delay.previous.log`: after a crash,
attach that one.
Security problems should be reported privately instead: see
[Security and privacy](security.md).

# Quick start

This takes about five minutes. You need OBS 28 or newer.

1. **Install** the desktop app for your system from the
   [releases page](https://github.com/commitandpray67/stream-delay/releases)
   (see [Installing](install.md) for details and security prompts). Start it.
   The dashboard opens and a tray icon appears.
2. **Add your stream key.** On the dashboard's **Setup** tab, choose Twitch and
   paste your stream key from the Twitch Creator Dashboard (Settings → Stream).
   It is stored in your OS keychain and never shown again.
3. **Point OBS at stream-delay.** Either let the Setup tab do it for you through
   obs-websocket (it can also import the key OBS already has), or in OBS go to
   **Settings → Stream**, choose **Custom…**, and enter:
   - Server: `rtmp://127.0.0.1:1935/live`
   - Stream key: anything (for example `streamdelay`)

   Both ways are described in [Connecting OBS](obs-setup.md).
4. **Set the keyframe interval to 2 s** in OBS: **Settings → Output**, switch
   *Output Mode* to **Advanced**, and on the **Streaming** tab set *Keyframe
   Interval* to `2 s`. Delay changes happen on keyframes, and Twitch requires 2 s.
5. **Add the dock and overlay** (optional but recommended): copy the links from the
   Setup tab. In OBS, **Docks → Custom Browser Docks** for the dock, and a
   **Browser** source for the overlay. See [OBS dock and overlay](dock-and-overlay.md).
6. **Start streaming in OBS** as usual. The dock shows **LIVE**.
7. **Change the delay** with the dock's preset buttons, the tray menu, or the
   default hotkeys `Ctrl+Alt+Shift+1` … `5` (`Cmd` on macOS). Go back to live with
   `Ctrl+Alt+Shift+L`.

To try it without anyone watching, add `?bandwidthtest=true` to the end of your
Twitch stream key. Twitch then accepts the stream but doesn't show it.

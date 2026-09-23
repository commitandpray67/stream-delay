# Connecting OBS

OBS has to send its stream to stream-delay instead of straight to Twitch. There
are two ways to set this up. Both are on the dashboard's **Setup** tab (tray menu
→ **Set up OBS…**).

## Automatic (recommended)

This uses obs-websocket, which is built into OBS 28 and newer.

1. In OBS, open **Tools → WebSocket Server Settings**, tick **Enable WebSocket
   server**, and click **Show Connect Info** to see the port and password.
2. On the Setup tab, enter the host (`127.0.0.1` if OBS runs on the same PC), port
   (usually `4455`) and password, then click **Connect to OBS**.
3. Choose what to do:
   - **Move my Twitch stream key from OBS into stream-delay:** if OBS is set up
     for Twitch, stream-delay copies that key into your OS keychain, so you don't
     have to find it again.
   - **Add the overlay to my current scene:** adds a browser source for the delay
     badge and the Mask slate.
4. Click **Set up OBS automatically**. stream-delay saves a backup of OBS's stream
   settings (in your OS keychain, as they include the stream key) and points OBS
   at itself.

OBS must not be streaming while you do this; stream-delay refuses rather than
interrupt a live stream.

**Undo:** **Restore my original OBS settings** puts back exactly what OBS had
before, including the Twitch service and key. It only restores to the OBS the
backup came from: if you have since connected stream-delay to another OBS,
connect it back first.

## By hand

In OBS, open **Settings → Stream**:

- Service: **Custom…**
- Server: `rtmp://127.0.0.1:1935/live` (the Setup tab shows the exact address if
  the port had to change)
- Stream Key: anything, for example `streamdelay`, unless stream-delay listens on
  your network: then use the ingest key shown on the Setup tab

stream-delay uses the key you saved on the Setup tab, not the one in OBS. If you
prefer to keep the real key in OBS, tick **Use the stream key entered in OBS
instead (passthrough)** on the Setup tab and put your real key in OBS.

## Keyframe interval: set it to 2 seconds

Delay changes happen on keyframes, so they can only be as precise as the keyframe
interval. Twitch also requires 2 s.

**Settings → Output**, *Output Mode*: **Advanced**, **Streaming** tab,
*Keyframe Interval*: `2 s`.

With the Twitch service selected, OBS enforces this automatically, but with a
custom server it doesn't. stream-delay shows a warning in the dock and dashboard
if keyframes are further apart than 2.5 s.

## Things that change when OBS streams to a custom server

- **Enhanced Broadcasting** (Twitch's multi-quality upload from OBS) is not
  supported yet. stream-delay shows a warning if it detects it; turn it off in
  **Settings → Stream**.
- The **Twitch VOD track** option only appears in OBS for the Twitch service, so
  the separate VOD audio track is not available through stream-delay yet.
- Other encoders work the same way: anything that can stream to an RTMP server
  (Streamlabs Desktop, vMix, XSplit, hardware encoders) can use
  `rtmp://127.0.0.1:1935/live`.

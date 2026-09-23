# OBS dock and overlay

Both are web pages served by stream-delay. Copy their links from the **Setup** tab
or the tray menu. Each link contains its own access token: the dock's can only
change the delay and the overlay's can only show it; neither can change your
settings or stream key. Still, don't show them on stream or share them. Links
copied before this version carry the dashboard's token: copy them again.

## Dock

In OBS: **Docks → Custom Browser Docks…**, name it "Stream Delay", paste the dock
URL, and click **Apply**. Drag the dock wherever you like.

The dock shows:

- a large status badge: **LIVE**, **DELAY 30 s**, **ADDING…**, **GOING LIVE…**;
- your preset buttons (Live/15/30/60/120 s by default; change them on the
  **Delay settings** tab). **Live** drops the delay at the next keyframe; the
  dock always shows it, even if you remove the 0 s preset;
- a custom value field and the Rewind/Mask choice;
- **End stream** (two clicks; ends the broadcast without airing the buffer) and
  **Cancel** while a Mask or go-live change is still pending. After ending,
  **Resume broadcasting** takes their place. (**Go live now** and **Air up to
  now, then go live** are on the dashboard, the tray menu and hotkeys.)
- how much of the stream is buffered, plus warnings (keyframe interval, upload
  backlog, Enhanced Broadcasting).

The same controls, plus **Air up to now, then go live**, are on the dashboard's
**Control** tab, and the dock works in any
browser (a phone or tablet too, if you enable LAN access on the Advanced tab).

## Overlay

Add a **Browser** source with the overlay URL to your scenes, sized to your canvas
(for example 1920×1080), and keep it at the top of the source list. The page is
transparent except for:

- a **delay badge** while the stream is delayed (optional; choose the corner);
- a short **pop-up** when the delay changes (optional);
- the **Mask slate**, shown only while a Mask change is in progress.

Customize the slate text, image and colors on the **Overlay** tab.

Tips:

- To have the overlay in every scene, put it in one scene and add that scene as a
  source (a nested scene) to the others.
- The slate only covers what OBS renders. It works as long as the overlay source is
  visible in the scene you're streaming.
- The overlay does not need obs-websocket. The setup wizard can add it for you to
  the current scene.

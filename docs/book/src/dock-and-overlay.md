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

- a large status badge: **NO DELAY**, **DELAYED 30 s**, **ADDING DELAY…**,
  **REMOVING DELAY…**, **ENDING STREAM…**, **STREAM ENDED**;
- your preset buttons (0/15/30/60/120 s by default; change them on the **Delay
  settings** tab). **0 s** removes the delay at the next keyframe; the dock always
  shows it, even if you remove the 0 s preset;
- a custom value field and the Rewind/Mask choice;
- while you are streaming: **Dump buffer**, **End stream** (after what is
  buffered has aired) and **End stream now** (without airing it); see
  [Dumping the buffer](delay-modes.md#dumping-the-buffer) and
  [Ending the stream](delay-modes.md#ending-the-stream). Each takes two clicks.
  While the end airs, **Keep streaming** takes it back. **Cancel** appears while a
  Mask or delay removal is still pending;
- how much of the stream is buffered, plus warnings (keyframe interval, upload
  backlog, Enhanced Broadcasting).

Starting a stream is always done in OBS. After ending, the dock tells you to stop
and start streaming in OBS to go live again.

The dashboard's **Control** tab has the same controls, plus **Remove delay after it
airs** and **Resume broadcasting** (a new broadcast without restarting the stream in
OBS). The dock works in any browser (a phone or tablet too, if you enable LAN access
on the Advanced tab).

After you update stream-delay, docks and browser sources that OBS kept open reload
themselves with the new version. Version 0.2.0 and earlier can't: if the dock still
shows old buttons after updating, open **Docks → Custom Browser Docks…**, add
`&v=2` to the end of its URL and click **Apply**. A changed address is always
loaded fresh.

## Overlay

Add a **Browser** source with the overlay URL to your scenes, sized to your canvas
(for example 1920×1080), and keep it at the top of the source list. The page is
transparent except for:

- a **delay badge** while a delay is in effect (optional; choose the corner);
- a short **pop-up** when you change the delay (optional): "Stream delay: 30 s" once
  the new delay is in effect, "Stream delay removed" once it is gone. There is no
  pop-up when a broadcast starts or reconnects, while a change is still under way,
  or for a dump;
- the **Mask slate**, shown only while a Mask change or a dump under the slate is
  in progress.

Customize the slate text, image and colors on the **Overlay** tab.

Tips:

- To have the overlay in every scene, put it in one scene and add that scene as a
  source (a nested scene) to the others.
- The slate only covers what OBS renders. It works as long as the overlay source is
  visible in the scene you're streaming.
- The overlay does not need obs-websocket. The setup wizard can add it for you to
  the current scene.

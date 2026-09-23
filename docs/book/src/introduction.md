# stream-delay

**Change your stream delay while you're live, without restarting OBS.**

OBS locks its stream delay once you go live, and Twitch's own delay setting can't
be changed mid-stream either. stream-delay is a small relay that sits between your
encoder and Twitch:

```text
OBS  →  rtmp://127.0.0.1:1935/live  →  stream-delay  →  Twitch
```

It keeps a rolling buffer of what OBS sends. When you change the delay, it jumps
to a different point in that buffer at a keyframe and rewrites the timestamps, so
the connection to Twitch never drops and viewers never see a "stream offline"
screen.

- **Add delay instantly** when a stream sniper shows up. Nothing new leaks.
- **Mask mode** covers the switch with a slate so no gameplay is shown twice.
- **Go live** straight away, or **after what's buffered has aired** so chat can
  catch up.
- **No re-encoding.** Video and audio pass through byte for byte, with almost no
  CPU use.
- **Control it from anywhere:** an OBS dock, global hotkeys, a tray icon, a
  browser-source overlay, the command line, or an HTTP API for Stream Deck and
  bots.
- **Free and private:** GPL-3.0, no accounts, no telemetry. Your stream key stays
  in your OS keychain.

It runs on Windows, macOS and Linux, and works with any encoder that can stream
RTMP (OBS, Streamlabs, vMix, hardware encoders).

> **Beta.** stream-delay is tested automatically end to end, under network faults
> and for hours at a time, but it has had little use on real Twitch streams so far.
> Try it with [a private test stream](troubleshooting.md#test-without-going-public)
> first, and please [report problems](troubleshooting.md#reporting-a-problem).

Start with the [quick start](quick-start.md).

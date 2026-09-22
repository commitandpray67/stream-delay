# ADR 0001: Delay the stream with a post-encode RTMP proxy

- Status: accepted
- Date: 2026-09-22

## Context

Streamers want to add, remove or change stream delay while live. OBS locks its built-in stream delay when the stream starts, and Twitch's delay setting cannot be changed mid-stream. Options:

1. **Delay inside OBS before encoding** (for example a video filter that buffers raw frames). Raw 1080p60 frames need gigabytes of RAM for 30 seconds, audio must be delayed separately, and it only works in OBS.
2. **An OBS plugin that replaces the stream output.** Tightly coupled to OBS internals and only works in OBS.
3. **A local RTMP proxy after encoding.** OBS streams to `rtmp://127.0.0.1`; the proxy buffers the compressed stream (about 1 MB/s at 8 Mbps) and forwards it to Twitch.

## Decision

Build a local RTMP proxy that works on encoded data (option 3). Video and audio are never re-encoded; payloads pass through byte for byte.

## Consequences

- Works with any RTMP encoder (OBS, Streamlabs, vMix, hardware encoders) on any OS.
- Negligible CPU use; memory is roughly bitrate × maximum delay.
- Delay can only change on keyframe boundaries (see ADR 0002).
- OBS treats the stream as a custom server, which disables Twitch-only OBS features such as Enhanced Broadcasting and the VOD audio track until we add explicit support.
- We must implement RTMP carefully (handshake, chunking, extended timestamps, acknowledgements).

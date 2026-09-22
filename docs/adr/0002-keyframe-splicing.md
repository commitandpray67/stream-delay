# ADR 0002: Change delay by splicing on keyframes with timestamp rewriting

- Status: accepted
- Date: 2026-09-22

## Context

Without transcoding, the only way to change what viewers see is to choose which encoded messages to forward and when. A decoder can only start cleanly at a keyframe. The ingest server (Twitch) expects timestamps that keep increasing on one uninterrupted connection.

## Decision

- Every message from the encoder is stored in a ring buffer with its arrival time. Keyframes are indexed.
- The output sends a message when `now >= arrival + delay`, preserving the encoder's pacing.
- A delay change is a *splice*: the output cursor jumps to a keyframe and all following timestamps are offset so output timestamps keep increasing (`offset = last_output_ts + one frame − keyframe_ts`). Audio and data older than the keyframe are dropped after a splice so audio stays in sync.
- **Increasing delay** jumps back to the newest keyframe at least `D` old ("rewind": viewers see the last `D` seconds again). The resulting delay is rounded up, never down.
- **Mask mode** shows an overlay slate first and rewinds only to a keyframe recorded after the slate appeared, so no gameplay repeats.
- **Going live** jumps forward to the next keyframe that arrives from the encoder.
- The previous segment may end mid-GOP. Decoders discard the few frames that referenced unsent frames. If testing against Twitch shows visible artifacts, an option will delay the splice until the old segment reaches its next keyframe.

## Consequences

- Delay changes take effect instantly (rewind) or within one keyframe interval (go live, normally ≤ 2 s).
- Effective delay may exceed the requested delay by up to one keyframe interval.
- The key safety invariant, tested with property tests: after a delay `D` is applied, nothing is sent less than `D` after it arrived until the delay is lowered.
- The Twitch connection never drops during a change.

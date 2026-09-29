# ADR 0003: Keep splices safe for B-frames and open-GOP HEVC

- Status: accepted
- Date: 2026-09-22
- Refines: [ADR 0002](https://github.com/commitandpray67/stream-delay/blob/main/docs/adr/0002-keyframe-splicing.md)

## Context

The first end-to-end runs (ffmpeg → streamdelayd → ffmpeg, see `tests/e2e/run.sh`) showed two problems that ADR 0002 predicted as risks:

1. **B-frames.** Output timestamps are decode timestamps (DTS). A frame sent just before a splice can have a presentation time (PTS = DTS + composition time) later than the keyframe we splice to, so decoded frames briefly went backwards in time.
2. **Open-GOP HEVC.** x265 (and some hardware encoders) mark CRA pictures as keyframes. The RASL pictures that follow a CRA reference frames from *before* it. After a splice those frames were never sent, so the decoder reported missing references. Even without RASL pictures, a CRA in the middle of a stream does not reset the decoder's picture ordering.

## Decision

- The engine tracks the highest PTS it has sent (using the composition time from the FLV tag). The new keyframe is placed after both the last DTS and the last PTS.
- When the splice point is an HEVC CRA, its slice NAL units are rewritten as BLA_W_LP (a one-byte change of the NAL type), which tells the decoder the reference chain is broken. The RASL pictures that follow it are dropped until the first trailing or random-access picture.

## Consequences

- Payloads are still passed through unchanged, except for the NAL type byte of a spliced CRA frame.
- The end-to-end test runs in CI for H.264 with B-frames, HEVC with open GOP (Enhanced RTMP) and timestamps crossing the 24-bit extended-timestamp boundary. All decode with zero errors across every splice.
- H.264 open GOP (non-IDR I-frames flagged as keyframes) is not handled; OBS does not produce it. If needed, the same idea applies with a recovery-point check.

## Amendment (2026-09-29)

- A dump's hold sends the last keyframe that aired again. It follows the same rule as a splice's keyframe: after both the last DTS and the last PTS. It came 1 ms after the last DTS, so with B-frames it could be presented before frames already sent. The 12-hour soak's decoder caught this when the destination dropped during a hold.
- After a splice to a CRA, the RADL leading pictures are dropped too, not only the RASL ones. RADL pictures are presented before their CRA, so they could come before frames already sent. Trailing pictures never reference leading pictures (H.265 3.114), so the rest still decodes.
- The engine tests simulate B-frames and check on every connection that no keyframe is presented before a frame sent earlier.

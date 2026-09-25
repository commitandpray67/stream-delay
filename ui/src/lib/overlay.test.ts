import { describe, expect, it } from "vitest";
import { badgeDelay, DelayAnnouncer, previewUrl, settledDelay } from "./overlay";
import type { Phase, RelayState } from "./types";

function state(
  phase: Phase,
  target_s: number,
  effective_s: number,
  opts: { connected?: boolean; ended?: boolean; short?: boolean } = {},
): RelayState {
  return {
    delay: {
      phase,
      target_ms: target_s * 1000,
      effective_ms: effective_s * 1000,
      max_delay_ms: 120_000,
      history_ms: 60_000,
      buffered_bytes: 0,
      mask_visible: false,
      history_short: opts.short ?? false,
      ingest: {
        active: true,
        video_codec: null,
        audio_codec: null,
        bitrate_kbps: 0,
        fps: 30,
        gop_ms: 2000,
        enhanced: false,
        multitrack: false,
      },
      output: { connected: opts.connected ?? true, splices: 0, dropped_frames: 0, sent_bytes: 0 },
      warnings: [],
    },
    ingest: { listen: "", connected: true, peer: null, app: null, last_error: null },
    egress: { status: "live", destination: null, last_error: null, bitrate_kbps: 0, backlog_bytes: 0, reconnects: 0 },
    ended: opts.ended ?? false,
    ending: false,
  };
}

describe("overlay announcements", () => {
  it("announces changes once they take effect, and nothing else", () => {
    const a = new DelayAnnouncer();
    expect(a.update(state("offline", 0, 0, { connected: false }))).toBeNull();
    // The broadcast starts: its delay is not news.
    expect(a.update(state("live", 0, 0))).toBeNull();
    // Mask: nothing while the slate builds the delay, then the new delay.
    expect(a.update(state("adding", 15, 0))).toBeNull();
    expect(a.update(state("delayed", 15, 15))).toBe("Stream delay: 15 s");
    // A keyframe or a reconnect stretches it a little: not a change.
    expect(a.update(state("delayed", 15, 16.4))).toBeNull();
    // Removing it.
    expect(a.update(state("going-live", 0, 15))).toBeNull();
    expect(a.update(state("live", 0, 0))).toBe("Stream delay removed");
    expect(a.update(state("delayed", 30, 30))).toBe("Stream delay: 30 s");
  });

  it("says what the delay really is when the buffer was too short", () => {
    const a = new DelayAnnouncer();
    a.update(state("live", 0, 0));
    expect(a.update(state("delayed", 15, 3, { short: true }))).toBe("Stream delay: 3 s");
    // Too short to count as a delay at all: still no delay, so nothing to say.
    const b = new DelayAnnouncer();
    b.update(state("live", 0, 0));
    expect(b.update(state("live", 15, 0.2, { short: true }))).toBeNull();
  });

  it("starts over with each broadcast", () => {
    const a = new DelayAnnouncer();
    a.update(state("delayed", 30, 30));
    // Ended, or the destination dropped: the next broadcast's delay is not news.
    expect(a.update(state("delayed", 30, 30, { ended: true }))).toBeNull();
    expect(a.update(state("live", 0, 0))).toBeNull();
    expect(a.update(state("offline", 0, 0, { connected: false }))).toBeNull();
    expect(a.update(state("delayed", 60, 60))).toBeNull();
  });

  it("shows the badge while a delay is in effect", () => {
    expect(badgeDelay(state("live", 0, 0))).toBeNull();
    expect(badgeDelay(state("delayed", 30, 31))).toBe(30);
    // While a change is under way, the delay viewers have now.
    expect(badgeDelay(state("reducing", 10, 30))).toBe(30);
    expect(badgeDelay(state("delayed", 30, 30, { ended: true }))).toBeNull();
    expect(badgeDelay(state("delayed", 30, 30, { connected: false }))).toBeNull();
    expect(settledDelay(state("adding", 30, 0))).toBeNull();
  });
});

describe("the dashboard's overlay preview", () => {
  it("uses the overlay link's read token, on the dashboard's own server", () => {
    const src = previewUrl("http://127.0.0.1:7788/overlay?token=read-token%2Fx", "badge");
    expect(src).toBe("/overlay?preview=badge&token=read-token%2Fx");
    expect(new URLSearchParams(src!.split("?")[1]).get("token")).toBe("read-token/x");
  });

  it("shows nothing without a link that has a token", () => {
    expect(previewUrl(undefined, "mask")).toBeNull();
    expect(previewUrl("not a url", "mask")).toBeNull();
    expect(previewUrl("http://127.0.0.1:7788/overlay", "mask")).toBeNull();
  });
});

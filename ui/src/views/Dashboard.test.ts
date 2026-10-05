// @vitest-environment jsdom
import { cleanup, render } from "@testing-library/svelte";
import { afterEach, expect, it } from "vitest";
import { live } from "../lib/live.svelte";
import type { RelayState } from "../lib/types";
import Dashboard from "./Dashboard.svelte";

/** A broadcast with 20 s of delay, airing what is buffered before it ends. */
function ending(): RelayState {
  return {
    delay: {
      phase: "delayed",
      target_ms: 20_000,
      effective_ms: 20_000,
      max_delay_ms: 120_000,
      history_ms: 60_000,
      buffered_bytes: 0,
      mask_visible: false,
      cancellable: false,
      slate_change: 0,
      history_short: false,
      memory_short: false,
      excess_ms: 0,
      full_delay_ready: false,
      ingest: {
        active: false,
        video_codec: "avc",
        audio_codec: "aac",
        bitrate_kbps: 0,
        fps: 0,
        gop_ms: 2000,
        enhanced: false,
        multitrack: false,
      },
      output: { connected: true, splices: 0, dropped_frames: 0, sent_bytes: 0 },
      warnings: [],
    },
    ingest: { listen: "127.0.0.1:1935", connected: false, peer: null, app: "live", last_error: null },
    egress: {
      status: "live",
      destination: "twitch",
      last_error: null,
      bitrate_kbps: 6000,
      backlog_bytes: 0,
      reconnects: 0,
    },
    ended: false,
    ending: true,
  } as RelayState;
}

afterEach(() => {
  cleanup();
  live.state = null;
});

it("the header says the stream is ending, as the controls do", () => {
  live.state = ending();
  const { container } = render(Dashboard);
  const header = container.querySelector("header [role=status]")?.textContent ?? "";
  expect(header).toMatch(/Ending stream/);
});

// @vitest-environment jsdom
import { render, screen } from "@testing-library/svelte";
import { tick } from "svelte";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { live, sendLive } from "../lib/live.svelte";
import type { LimitedConfig, RelayState, Snapshot } from "../lib/types";
import Overlay from "./Overlay.svelte";

// What the page sends to stream-delay, and a connection that is up.
vi.mock("../lib/live.svelte", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/live.svelte")>()),
  sendLive: vi.fn(),
  whenConnected: vi.fn((callback: () => void) => callback()),
}));

function state(delay: Partial<Snapshot>): RelayState {
  return {
    delay: {
      phase: "delayed",
      target_ms: 30_000,
      effective_ms: 30_000,
      max_delay_ms: 120_000,
      history_ms: 120_000,
      buffered_bytes: 0,
      mask_visible: false,
      cancellable: false,
      slate_change: 0,
      history_short: false,
      memory_short: false,
      excess_ms: 0,
      full_delay_ready: false,
      ingest: {
        active: true,
        video_codec: "avc",
        audio_codec: "aac",
        bitrate_kbps: 6000,
        fps: 60,
        gop_ms: 2000,
        enhanced: false,
        multitrack: false,
      },
      output: { connected: true, splices: 0, dropped_frames: 0, sent_bytes: 0 },
      warnings: [],
      ...delay,
    },
    ingest: { listen: "", connected: true, peer: null, app: null, last_error: null },
    egress: { status: "live", destination: null, last_error: null, bitrate_kbps: 0, backlog_bytes: 0, reconnects: 0 },
    ended: false,
    ending: false,
  };
}

const config = {
  scope: "read",
  config: {
    overlay: {
      badge: true,
      badge_position: "top-right",
      popup: true,
      mask_title: "Be right back",
      mask_subtitle: "Adding delay",
      accent_color: "#9146ff",
      background_color: "#0e0e10",
      text_color: "#ffffff",
      mask_image: "",
    },
  },
} as unknown as LimitedConfig;

/** Two animation frames: the slate has been painted. */
const painted = () => vi.advanceTimersByTimeAsync(40);

/** Opens the overlay page at `path`, as an OBS browser source or not. */
function open(path: string, obs: boolean) {
  history.replaceState(null, "", path);
  if (obs) (globalThis as { obsstudio?: object }).obsstudio = {};
  live.config = config;
  live.state = state({});
  return render(Overlay);
}

const sent = () => vi.mocked(sendLive).mock.calls.map(([msg]) => msg);

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  delete (globalThis as { obsstudio?: object }).obsstudio;
  vi.clearAllMocks();
  vi.useRealTimers();
});

describe("overlay slate", () => {
  it("covers the picture while the delay changes, and hides the badge", async () => {
    open("/overlay?token=r", false);
    expect(screen.getByText("⏱ 30 s delay", { exact: false })).toBeTruthy();
    expect(screen.queryByText("Be right back")).toBeNull();
    live.state = state({ phase: "adding", mask_visible: true, slate_change: 1 });
    await tick();
    expect(screen.getByRole("heading", { name: "Be right back" })).toBeTruthy();
    expect(screen.getByText("Adding delay")).toBeTruthy();
    expect(screen.queryByText("delay", { exact: false, selector: ".badge" })).toBeNull();
  });

  it("in OBS, says once it has painted the slate for each change", async () => {
    open("/overlay?token=r", true);
    live.state = state({ phase: "adding", mask_visible: true, slate_change: 3 });
    await tick();
    // Not before it is on screen.
    expect(sent()).toEqual([]);
    await painted();
    expect(sent()).toEqual([{ type: "slate-shown", change: 3 }]);
    // The same change again (a new state while it is up): said once.
    live.state = state({ phase: "adding", mask_visible: true, slate_change: 3, effective_ms: 12_000 });
    await tick();
    await painted();
    expect(sent()).toHaveLength(1);
    // The slate goes, then comes back for another change.
    live.state = state({});
    await tick();
    live.state = state({ phase: "adding", mask_visible: true, slate_change: 4 });
    await tick();
    await painted();
    expect(sent()).toEqual([
      { type: "slate-shown", change: 3 },
      { type: "slate-shown", change: 4 },
    ]);
  });

  it("in OBS, never says it painted a slate it could not draw", async () => {
    open("/overlay?token=r", true);
    // No settings (yet): there is nothing to draw the slate with.
    live.config = null;
    live.state = state({ phase: "adding", mask_visible: true, slate_change: 6 });
    await tick();
    await painted();
    expect(screen.queryByText("Be right back")).toBeNull();
    expect(sent()).toEqual([]);
    // Once they come, it is drawn, then said.
    live.config = config;
    await tick();
    await painted();
    expect(sent()).toEqual([{ type: "slate-shown", change: 6 }]);
  });

  it("in OBS, says whether it is on stream once OBS says so", async () => {
    open("/overlay?token=r", true);
    // OBS does not say when the page loads: nothing to report yet.
    expect(sent()).toEqual([]);
    window.dispatchEvent(new CustomEvent("obsSourceActiveChanged", { detail: { active: true } }));
    window.dispatchEvent(new CustomEvent("obsSourceActiveChanged", { detail: { active: false } }));
    expect(sent()).toEqual([
      { type: "overlay", active: true },
      { type: "overlay", active: false },
    ]);
  });

  it("in a browser tab or as a preview, never claims to cover the stream", async () => {
    for (const [path, obs] of [
      ["/overlay?token=r", false],
      ["/overlay?preview=mask&token=r", true],
    ] as const) {
      const view = open(path, obs);
      window.dispatchEvent(new CustomEvent("obsSourceActiveChanged", { detail: { active: true } }));
      live.state = state({ phase: "adding", mask_visible: true, slate_change: 5 });
      await tick();
      await painted();
      expect(sent(), path).toEqual([]);
      view.unmount();
      delete (globalThis as { obsstudio?: object }).obsstudio;
    }
  });

  it("the slate hides the stream even with a see-through background color", async () => {
    open("/overlay?token=r", false);
    const seeThrough = structuredClone(config);
    (seeThrough.config.overlay as { background_color: string }).background_color = "#0e0e1040";
    live.config = seeThrough;
    live.state = state({ phase: "adding", mask_visible: true, slate_change: 1 });
    await tick();
    const root = document.querySelector<HTMLElement>(".root")!;
    expect(root.style.getPropertyValue("--slate")).toBe("#0e0e10");
    // The badge keeps the color as set.
    expect(root.style.getPropertyValue("--bg")).toBe("#0e0e1040");
  });

  it("the preview shows the slate without a stream", () => {
    history.replaceState(null, "", "/overlay?preview=mask&token=r");
    live.config = config;
    live.state = null;
    render(Overlay);
    expect(screen.getByRole("heading", { name: "Be right back" })).toBeTruthy();
  });
});

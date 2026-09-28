// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, within } from "@testing-library/svelte";
import { tick } from "svelte";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import * as api from "../lib/api";
import { t } from "../lib/i18n";
import { live } from "../lib/live.svelte";
import type { Ack, DelayConfig, LimitedConfig, RelayState, Snapshot } from "../lib/types";
import DelayControls from "./DelayControls.svelte";

vi.mock("../lib/api", () => ({
  applyPreset: vi.fn(),
  cancel: vi.fn(),
  dumpBuffer: vi.fn(),
  endStream: vi.fn(),
  goLive: vi.fn(),
  resumeStream: vi.fn(),
  setDelay: vi.fn(),
}));

/** A broadcast running with `target_s` of delay and `history_s` buffered. */
function state(target_s: number, history_s: number, delay: Partial<Snapshot> = {}): RelayState {
  return {
    delay: {
      phase: target_s > 0 ? "delayed" : "live",
      target_ms: target_s * 1000,
      effective_ms: target_s * 1000,
      max_delay_ms: 120_000,
      history_ms: history_s * 1000,
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
    ingest: { listen: "127.0.0.1:1935", connected: true, peer: null, app: "live", last_error: null },
    egress: {
      status: "live",
      destination: "twitch",
      last_error: null,
      bitrate_kbps: 6000,
      backlog_bytes: 0,
      reconnects: 0,
    },
    ended: false,
    ending: false,
  };
}

/** The dock's settings. */
function config(delay: Partial<DelayConfig> = {}): LimitedConfig {
  return {
    scope: "control",
    config: {
      delay: {
        max_seconds: 120,
        start_seconds: 0,
        default_mode: "rewind",
        presets: [
          { seconds: 0, mode: "rewind" },
          { seconds: 30, mode: "rewind" },
        ],
        ram_cap_mb: 1024,
        mask_margin_ms: 1500,
        mute_under_slate: true,
        after_reconnect: "keep",
        keep_buffer: true,
        ...delay,
      },
      overlay: {
        badge: true,
        badge_position: "top-right",
        popup: true,
        mask_title: "Be right back",
        mask_subtitle: "",
        accent_color: "#9146ff",
        background_color: "#0e0e10",
        text_color: "#ffffff",
        mask_image: "",
      },
    },
    urls: { obs_server: "rtmp://127.0.0.1:1935/live" },
    version: "0.0.0",
    ui_build: null,
  };
}

function ack(extra: Partial<Ack> = {}): Ack {
  return { target_ms: 30_000, effective_ms: 30_000, pending: false, history_short: false, ...extra };
}

/** Renders the dock with the stream in `s`, overlays in OBS (`active` on stream). */
function dock(s: RelayState, overlays = { count: 0, active: 0 }, delay: Partial<DelayConfig> = {}) {
  live.state = s;
  live.config = config(delay);
  live.overlays = overlays.count;
  live.activeOverlays = overlays.active;
  return render(DelayControls, { compact: true });
}

const button = (name: string) => screen.getByRole("button", { name });
const popup = () => screen.getByRole("alertdialog");
/** What the page says after an action. */
const notice = () => document.querySelector("p.notice[role=status]")?.textContent ?? null;

/** Lets the answer to an action come in, without running out any timer that follows. */
const answered = () => vi.advanceTimersByTimeAsync(10);

/** Dumps without the first-time explanation: armed by one click, done by a second. */
function explained() {
  localStorage.setItem("stream-delay-understood:dump", "1");
}

beforeEach(() => {
  vi.useFakeTimers();
  localStorage.clear();
});

afterEach(() => {
  vi.clearAllMocks();
  vi.useRealTimers();
});

describe("dump", () => {
  it("explains itself the first time, and dumps only when confirmed there", async () => {
    vi.mocked(api.dumpBuffer).mockResolvedValue(ack({ dump: "replay" }));
    dock(state(30, 120));
    await fireEvent.click(button("Dump buffer"));
    expect(popup().textContent).toContain(t("action.dump.replay", { delay: "30 s" }));
    // Waiting does not dump, nor close the explanation.
    await vi.advanceTimersByTimeAsync(10_000);
    expect(api.dumpBuffer).not.toHaveBeenCalled();
    await fireEvent.click(within(popup()).getByRole("button", { name: "Dump buffer" }));
    expect(api.dumpBuffer).toHaveBeenCalledOnce();
    expect(api.dumpBuffer).toHaveBeenCalledWith("rewind");
    await answered();
    expect(notice()).toBe(t("dumped.replay", { delay: "30 s" }));
    expect(screen.queryByRole("alertdialog")).toBeNull();
    // Not understood yet: explained again next time.
    await fireEvent.click(button("Dump buffer"));
    expect(screen.queryByRole("alertdialog")).not.toBeNull();
  });

  it("cancelling the explanation dumps nothing", async () => {
    dock(state(30, 120));
    await fireEvent.click(button("Dump buffer"));
    await fireEvent.click(within(popup()).getByRole("button", { name: "Cancel" }));
    expect(screen.queryByRole("alertdialog")).toBeNull();
    expect(button("Dump buffer")).toBeTruthy();
    expect(api.dumpBuffer).not.toHaveBeenCalled();
  });

  it("once understood, a click arms it, a second dumps, and the arm runs out", async () => {
    vi.mocked(api.dumpBuffer).mockResolvedValue(ack({ dump: "replay" }));
    dock(state(30, 120));
    await fireEvent.click(button("Dump buffer"));
    await fireEvent.click(within(popup()).getByRole("checkbox"));
    await fireEvent.click(within(popup()).getByRole("button", { name: "Dump buffer" }));
    expect(api.dumpBuffer).toHaveBeenCalledOnce();
    await vi.runAllTimersAsync();

    await fireEvent.click(button("Dump buffer"));
    expect(screen.queryByRole("alertdialog")).toBeNull();
    // Armed: says what viewers will see.
    expect(button("Click again to dump")).toBeTruthy();
    expect(screen.getByText(t("action.dump.replay", { delay: "30 s" }))).toBeTruthy();
    await vi.advanceTimersByTimeAsync(3000);
    expect(button("Dump buffer")).toBeTruthy();
    expect(api.dumpBuffer).toHaveBeenCalledOnce();

    await fireEvent.click(button("Dump buffer"));
    await fireEvent.click(button("Click again to dump"));
    expect(api.dumpBuffer).toHaveBeenCalledTimes(2);
  });

  it("says what viewers will see: the replay, the slate, or a still frame", async () => {
    explained();
    const expected = async (s: RelayState, overlays: { count: number; active: number }, delay = {}) => {
      const view = dock(s, overlays, delay);
      await fireEvent.click(button("Dump buffer"));
      const text = screen.getByText((_, el) => el?.matches("p[role=status].muted") ?? false).textContent;
      view.unmount();
      return text;
    };
    const none = { count: 0, active: 0 };
    const onStream = { count: 1, active: 1 };
    // The buffer reaches back past what is thrown away: replay.
    expect(await expected(state(30, 120), none)).toBe(t("action.dump.replay", { delay: "30 s" }));
    // It does not: the slate, if an overlay is on stream.
    expect(await expected(state(30, 40), onStream)).toBe(
      `${t("action.dump.slate", { delay: "30 s" })} ${t("action.dump.slate.muted")}`,
    );
    expect(await expected(state(30, 40), onStream, { mute_under_slate: false })).toBe(
      `${t("action.dump.slate", { delay: "30 s" })} ${t("action.dump.slate.sound")}`,
    );
    // Else a still frame, and why.
    expect(await expected(state(30, 40), none)).toBe(
      `${t("action.dump.hold.noOverlay")} ${t("action.dump.hold", { delay: "30 s" })}`,
    );
    // An overlay OBS has not said is on stream cannot cover it.
    expect(await expected(state(30, 40), { count: 1, active: 0 })).toBe(
      `${t("action.dump.hold.unconfirmed")} ${t("action.dump.hold", { delay: "30 s" })}`,
    );
    // Without the rolling buffer every change is masked: never a replay.
    expect(await expected(state(30, 120), onStream, { keep_buffer: false })).toBe(
      `${t("action.dump.slate", { delay: "30 s" })} ${t("action.dump.slate.muted")}`,
    );
  });

  it("the still-frame warning stands out in the explanation", async () => {
    dock(state(30, 40));
    await fireEvent.click(button("Dump buffer"));
    const warning = within(popup()).getByText(t("action.dump.hold", { delay: "30 s" }), { exact: false });
    expect(warning.classList.contains("error")).toBe(true);
  });

  it("dumps the way the mode toggle says", async () => {
    explained();
    vi.mocked(api.dumpBuffer).mockResolvedValue(ack({ dump: "cover", pending: true }));
    dock(state(30, 120), { count: 1, active: 1 });
    await fireEvent.click(screen.getByRole("radio", { name: "Mask" }));
    await fireEvent.click(button("Dump buffer"));
    // Mask never replays.
    expect(screen.getByText(t("action.dump.slate", { delay: "30 s" }), { exact: false })).toBeTruthy();
    await fireEvent.click(button("Click again to dump"));
    expect(api.dumpBuffer).toHaveBeenCalledWith("mask");
  });

  it("says what the dump did, as the server reports it", async () => {
    explained();
    const cases: [Ack, string][] = [
      [ack({ dump: "replay", target_ms: 45_000 }), t("dumped.replay", { delay: "45 s" })],
      [ack({ dump: "cover", pending: true }), t("dumped.cover")],
      [ack({ dump: "hold", pending: true }), t("dumped.hold")],
      // Before or at the end of a broadcast nothing is held: nothing more airs.
      [ack({ dump: "hold", pending: false }), t("dumped.gone")],
    ];
    for (const [answer, says] of cases) {
      vi.mocked(api.dumpBuffer).mockResolvedValueOnce(answer);
      const view = dock(state(30, 40));
      await fireEvent.click(button("Dump buffer"));
      await fireEvent.click(button("Click again to dump"));
      await answered();
      expect(notice()).toBe(says);
      view.unmount();
    }
  });

  it("shows why a dump failed", async () => {
    explained();
    vi.mocked(api.dumpBuffer).mockRejectedValue(new Error("there is no delay, so nothing is waiting to air"));
    dock(state(30, 40));
    await fireEvent.click(button("Dump buffer"));
    await fireEvent.click(button("Click again to dump"));
    await vi.runAllTimersAsync();
    expect(screen.getByRole("alert").textContent).toBe("there is no delay, so nothing is waiting to air");
    expect(notice()).toBeNull();
  });

  it("is offered only with a delay to dump, while a broadcast runs", () => {
    let view = dock(state(0, 120));
    expect((button("Dump buffer") as HTMLButtonElement).disabled).toBe(true);
    view.unmount();
    view = dock(state(30, 120));
    expect((button("Dump buffer") as HTMLButtonElement).disabled).toBe(false);
    view.unmount();
    // Nothing is streaming.
    const idle = state(30, 120);
    idle.ingest.connected = false;
    idle.egress.status = "idle";
    dock(idle);
    expect(screen.queryByRole("button", { name: "Dump buffer" })).toBeNull();
  });
});

describe("what an action did", () => {
  /** The next state stream-delay sends. */
  async function next(s: RelayState) {
    live.state = s;
    await tick();
  }
  /** Building 30 s of delay back up after a dump, `history_s` of it so far. */
  const building = (history_s: number, phase: "adding" | "holding" = "adding") =>
    state(30, history_s, { phase, mask_visible: phase === "adding", effective_ms: 0 });
  async function dump(answer: Ack, overlays = { count: 1, active: 1 }) {
    explained();
    vi.mocked(api.dumpBuffer).mockResolvedValue(answer);
    dock(state(30, 40), overlays);
    await fireEvent.click(button("Dump buffer"));
    await fireEvent.click(button("Click again to dump"));
    await answered();
  }

  it("the slate covering a dump is told until the delay is back", async () => {
    await dump(ack({ dump: "cover", pending: true }));
    // The answer can come in before the state showing the dump.
    await next(state(30, 41));
    expect(notice()).toBe(t("dumped.cover"));
    await next(building(1));
    await next(building(20));
    expect(notice()).toBe(t("dumped.cover"));
    await next(state(30, 31));
    expect(notice()).toBeNull();
  });

  it("a still frame is told until the delay is back", async () => {
    await dump(ack({ dump: "hold", pending: true }), { count: 0, active: 0 });
    await next(state(30, 41));
    await next(building(5, "holding"));
    expect(notice()).toBe(t("dumped.hold"));
    await next(state(30, 31));
    expect(notice()).toBeNull();
  });

  it("a replay is told while the stretch replayed airs", async () => {
    explained();
    vi.mocked(api.dumpBuffer).mockResolvedValue(ack({ dump: "replay" }));
    dock(state(30, 120));
    await fireEvent.click(button("Dump buffer"));
    await fireEvent.click(button("Click again to dump"));
    await answered();
    await next(state(30, 121));
    expect(notice()).toBe(t("dumped.replay", { delay: "30 s" }));
    await vi.advanceTimersByTimeAsync(29_000);
    expect(notice()).toBe(t("dumped.replay", { delay: "30 s" }));
    await vi.advanceTimersByTimeAsync(1_000);
    expect(notice()).toBeNull();
  });

  it("is over once the delay is changed from elsewhere, or the stream ends", async () => {
    await dump(ack({ dump: "cover", pending: true }));
    await next(building(3));
    // Removed from the dashboard or a hotkey while the slate was up.
    await next(state(0, 10, { phase: "live" }));
    expect(notice()).toBeNull();
    cleanup();

    await dump(ack({ dump: "hold", pending: false }), { count: 0, active: 0 });
    await next(state(30, 41));
    expect(notice()).toBe(t("dumped.gone"));
    const ended = state(30, 0);
    ended.ended = true;
    await next(ended);
    expect(notice()).toBeNull();
  });

  it("a delay shorter than asked is told until it no longer is", async () => {
    const short = () => screen.queryByRole("alert")?.textContent ?? null;
    const says = "Not enough of the stream is buffered yet; the delay is shorter than asked.";
    vi.mocked(api.setDelay).mockResolvedValue(ack({ history_short: true, effective_ms: 12_000 }));
    dock(state(0, 12));
    await fireEvent.input(screen.getByLabelText(t("custom.label")), { target: { value: "30" } });
    await fireEvent.click(button("Set"));
    await answered();
    await next(state(0, 12));
    expect(short()).toBe(says);
    await next(state(30, 13, { effective_ms: 12_000, history_short: true }));
    expect(short()).toBe(says);
    // Set again (from here or elsewhere) once the buffer reached back far enough.
    await next(state(30, 40));
    expect(short()).toBeNull();
  });
});

describe("end stream", () => {
  it("needs a second click, within 3 seconds", async () => {
    vi.mocked(api.endStream).mockResolvedValue(state(30, 120));
    dock(state(30, 120));
    await fireEvent.click(button("End stream"));
    expect(api.endStream).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(3000);
    await fireEvent.click(button("End stream"));
    await fireEvent.click(button("Click again to end"));
    expect(api.endStream).toHaveBeenCalledWith("after-air");
  });

  it("ending now first says how much never airs", async () => {
    dock(state(30, 120));
    await fireEvent.click(button("End stream now"));
    expect(popup().textContent).toContain(t("action.endNow.warning", { delay: "30 s" }));
    await fireEvent.click(within(popup()).getByRole("button", { name: "End stream now" }));
    expect(api.endStream).toHaveBeenCalledWith("now");
  });
});

describe("cancel", () => {
  it("is offered while a change can be stopped, and not while a dump builds the delay back", async () => {
    vi.mocked(api.cancel).mockResolvedValue(ack());
    dock(state(30, 120, { phase: "adding", mask_visible: true, cancellable: true }));
    await fireEvent.click(button("Cancel"));
    expect(api.cancel).toHaveBeenCalledOnce();
    cleanup();
    // The slate covering a dump: the same phase, but nothing to cancel.
    dock(state(30, 40, { phase: "adding", mask_visible: true, cancellable: false }));
    expect(screen.queryByRole("button", { name: "Cancel" })).toBeNull();
  });
});

describe("custom delay", () => {
  const field = () => screen.getByLabelText(t("custom.label")) as HTMLInputElement;
  const type = (value: string) => fireEvent.input(field(), { target: { value } });
  const submit = () => fireEvent.submit(field().form!);

  it("sets what is typed, and 0 removes the delay", async () => {
    vi.mocked(api.setDelay).mockResolvedValue(ack());
    vi.mocked(api.goLive).mockResolvedValue(ack({ target_ms: 0 }));
    dock(state(30, 120));
    await type("45");
    await fireEvent.click(button("Set"));
    expect(api.setDelay).toHaveBeenCalledWith(45, "rewind");
    await type("0");
    await submit();
    expect(api.goLive).toHaveBeenCalledWith("now");
  });

  it("does nothing with the field emptied: it never removes the delay", async () => {
    dock(state(30, 120));
    await type("45");
    await type("");
    expect((button("Set") as HTMLButtonElement).disabled).toBe(true);
    // Enter in the field, whatever the button says.
    await submit();
    await vi.runAllTimersAsync();
    expect(api.goLive).not.toHaveBeenCalled();
    expect(api.setDelay).not.toHaveBeenCalled();
  });
});

describe("a delay that is not the one set", () => {
  it("offers to go back to it after an outage stretched it", async () => {
    vi.mocked(api.setDelay).mockResolvedValue(ack());
    dock(state(30, 120, { effective_ms: 52_000, excess_ms: 22_000 }));
    await fireEvent.click(button("Back to 30 s"));
    expect(api.setDelay).toHaveBeenCalledWith(30, "rewind");
  });

  it("offers to set it again once the buffer reaches back far enough", async () => {
    vi.mocked(api.setDelay).mockResolvedValue(ack());
    dock(state(30, 60, { effective_ms: 12_000, history_short: true, full_delay_ready: true }));
    await fireEvent.click(button("Set 30 s again"));
    expect(api.setDelay).toHaveBeenCalledWith(30, "rewind");
  });

  it("offers neither while it is the one set, or after the stream ended", () => {
    let view = dock(state(30, 60, { history_short: true, effective_ms: 12_000 }));
    expect(screen.queryByRole("button", { name: /^(Back to|Set .* again)/ })).toBeNull();
    view.unmount();
    const ended = state(30, 120, { effective_ms: 52_000, excess_ms: 22_000 });
    ended.ended = true;
    view = dock(ended);
    expect(screen.queryByRole("button", { name: /^Back to/ })).toBeNull();
  });
});

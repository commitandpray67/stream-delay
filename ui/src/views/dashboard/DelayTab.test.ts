// @vitest-environment jsdom
import { fireEvent, render, screen } from "@testing-library/svelte";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import * as api from "../../lib/api";
import { live } from "../../lib/live.svelte";
import type { PublicConfig } from "../../lib/types";
import DelayTab from "./DelayTab.svelte";

vi.mock("../../lib/api", () => ({ updateConfig: vi.fn() }));

/** The dashboard's settings: two presets, 30 s to wait for OBS. */
function config(): PublicConfig {
  return {
    scope: "admin",
    config: {
      ingest: { bind: "127.0.0.1:1935", grace_seconds: 30 },
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
      },
    },
  } as unknown as PublicConfig;
}

const grace = () => screen.getByLabelText(/Wait for OBS to reconnect/) as HTMLInputElement;
const preset = (n: number) => screen.getByLabelText(`Preset ${n} seconds`) as HTMLInputElement;
const type = (input: HTMLInputElement, value: string) => fireEvent.input(input, { target: { value } });
const save = () => fireEvent.click(screen.getByRole("button", { name: "Save" }));

beforeEach(() => {
  live.config = config();
  vi.mocked(api.updateConfig).mockResolvedValue(config());
});

afterEach(() => {
  vi.clearAllMocks();
});

describe("delay settings", () => {
  it("saves the numbers as typed", async () => {
    render(DelayTab);
    await type(grace(), "45");
    await type(preset(2), "60");
    await save();
    const update = vi.mocked(api.updateConfig).mock.calls[0][0] as {
      delay: { presets: { seconds: number }[] };
      grace_seconds: number;
    };
    expect(update.grace_seconds).toBe(45);
    expect(update.delay.presets[1].seconds).toBe(60);
  });

  it("saves nothing while a number is left empty, instead of 0", async () => {
    render(DelayTab);
    for (const input of [grace, () => preset(2)]) {
      await type(input(), "");
      await save();
      expect(api.updateConfig).not.toHaveBeenCalled();
      expect(screen.getByRole("alert").textContent).toMatch(/number/i);
      await type(input(), "30");
    }
    await save();
    expect(api.updateConfig).toHaveBeenCalledOnce();
  });
});

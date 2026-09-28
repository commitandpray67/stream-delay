// @vitest-environment jsdom
import { fireEvent, render, screen } from "@testing-library/svelte";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import * as api from "../../lib/api";
import { live } from "../../lib/live.svelte";
import type { PublicConfig } from "../../lib/types";
import AdvancedTab from "./AdvancedTab.svelte";

vi.mock("../../lib/api", () => ({
  updateConfig: vi.fn(),
  checkUpdates: vi.fn(),
  diagnosticsLink: vi.fn(),
}));

/** Seven presets (added on the Delay tab), hotkeys for the first five. */
function config(): PublicConfig {
  return {
    scope: "admin",
    version: "0.0.0",
    secrets_backend: "the OS keychain",
    urls: { dashboard: "http://127.0.0.1:7788/?token=t" },
    config: {
      api: { allow_lan: false },
      delay: {
        presets: [0, 15, 30, 60, 120, 45, 90].map((seconds) => ({ seconds, mode: "rewind" })),
      },
      hotkeys: {
        enabled: true,
        go_live: "CmdOrCtrl+Alt+Shift+L",
        go_live_after_air: "CmdOrCtrl+Alt+Shift+A",
        end_stream: "",
        end_stream_after_air: "",
        dump: "",
        presets: [1, 2, 3, 4, 5].map((n) => `CmdOrCtrl+Alt+Shift+${n}`),
      },
    },
  } as unknown as PublicConfig;
}

const type = (input: HTMLElement, value: string) => fireEvent.input(input, { target: { value } });

beforeEach(() => {
  live.config = config();
  vi.mocked(api.updateConfig).mockResolvedValue(config());
});

afterEach(() => {
  vi.clearAllMocks();
});

describe("hotkey settings", () => {
  it("saves a hotkey for a preset added after the first five, leaving the others without", async () => {
    render(AdvancedTab);
    await type(screen.getByLabelText(/^Preset 7/), "CmdOrCtrl+Alt+Shift+7");
    await fireEvent.click(screen.getByRole("button", { name: "Save" }));
    expect(api.updateConfig).toHaveBeenCalledOnce();
    // As sent: JSON has no gaps, and the server takes text for each preset.
    const sent = JSON.parse(JSON.stringify(vi.mocked(api.updateConfig).mock.calls[0][0])) as {
      hotkeys: { presets: unknown[] };
    };
    expect(sent.hotkeys.presets).toEqual([
      ...[1, 2, 3, 4, 5].map((n) => `CmdOrCtrl+Alt+Shift+${n}`),
      "",
      "CmdOrCtrl+Alt+Shift+7",
    ]);
  });
});

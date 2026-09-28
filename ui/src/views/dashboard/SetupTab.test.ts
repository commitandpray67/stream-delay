// @vitest-environment jsdom
import { fireEvent, render, screen } from "@testing-library/svelte";
import { tick } from "svelte";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import * as api from "../../lib/api";
import { live } from "../../lib/live.svelte";
import type { PublicConfig } from "../../lib/types";
import SetupTab from "./SetupTab.svelte";

vi.mock("../../lib/api", () => ({
  updateConfig: vi.fn(),
  setStreamKey: vi.fn(),
  clearStreamKey: vi.fn(),
  obsStatus: vi.fn(() => new Promise(() => {})),
  obsConnect: vi.fn(),
  obsConfigure: vi.fn(),
  obsRestore: vi.fn(),
}));

const TWITCH = "rtmps://live.twitch.tv:443/app";

/** The dashboard's settings, with the destination given. */
function config(destination: { service: string; url: string; key_mode: string }): PublicConfig {
  return {
    scope: "admin",
    destination_key_set: false,
    secrets_backend: "the OS keychain",
    services: [
      { id: "twitch", name: "Twitch", url: TWITCH },
      { id: "custom", name: "Custom", url: "" },
    ],
    urls: { obs_server: "rtmp://127.0.0.1:1935/live", obs_key: "streamdelay", dock: "d", overlay: "o" },
    config: { destination, delay: { keep_buffer: true, max_seconds: 120 } },
  } as unknown as PublicConfig;
}

const serverUrl = () => screen.getByLabelText(/Server URL/) as HTMLInputElement;
const passthroughBox = () => screen.getByLabelText(/passthrough/) as HTMLInputElement;

beforeEach(() => {
  live.config = config({ service: "custom", url: "rtmp://relay.example/live", key_mode: "passthrough" });
  vi.mocked(api.updateConfig).mockImplementation(async (u) =>
    config((u as { destination: { service: string; url: string; key_mode: string } }).destination),
  );
});

afterEach(() => {
  vi.clearAllMocks();
});

describe("where to stream", () => {
  it("shows the destination the OBS setup on this tab changed, and saves that", async () => {
    render(SetupTab);
    expect(serverUrl().value).toBe("rtmp://relay.example/live");
    expect(passthroughBox().checked).toBe(true);
    // Setting up OBS moved its Twitch key in: the destination is now Twitch,
    // with the key stored.
    live.config = config({ service: "twitch", url: TWITCH, key_mode: "stored" });
    await tick();
    expect(serverUrl().value).toBe(TWITCH);
    expect(passthroughBox().checked).toBe(false);
    await fireEvent.click(screen.getByRole("button", { name: "Save" }));
    expect(api.updateConfig).toHaveBeenCalledWith({
      destination: { service: "twitch", url: TWITCH, key_mode: "stored" },
    });
  });

  it("keeps what the streamer is typing when the settings change meanwhile", async () => {
    render(SetupTab);
    await fireEvent.input(serverUrl(), { target: { value: "rtmp://other.example/live" } });
    live.config = config({ service: "twitch", url: TWITCH, key_mode: "stored" });
    await tick();
    expect(serverUrl().value).toBe("rtmp://other.example/live");
  });
});

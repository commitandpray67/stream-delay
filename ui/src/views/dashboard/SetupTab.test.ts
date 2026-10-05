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
      { id: "youtube", name: "YouTube", url: "rtmps://a.rtmps.youtube.com:443/live2" },
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

describe("where to find the stream key", () => {
  const help = () => screen.getByText(/is never shown again/).textContent ?? "";
  const pick = (id: string) => fireEvent.change(screen.getByLabelText(/Service/), { target: { value: id } });

  it("names the service picked, and offers Twitch's bandwidth test only for Twitch", async () => {
    live.config = config({ service: "twitch", url: TWITCH, key_mode: "stored" });
    render(SetupTab);
    expect(help()).toContain("Twitch");
    expect(help()).toContain("bandwidthtest");
    await pick("youtube");
    expect(help()).toContain("YouTube");
    expect(help()).not.toMatch(/Twitch|bandwidthtest/);
    await pick("custom");
    expect(help()).not.toMatch(/Twitch|YouTube|bandwidthtest/);
  });
});

describe("passthrough where an ingest key is required", () => {
  /** The stream key "Set up OBS by hand" gives. */
  const byHandKey = () =>
    [...document.querySelectorAll("label")]
      .find((l) => l.textContent?.trim().startsWith("Stream Key"))!
      .querySelector("input")!.value;
  const warning = () => screen.queryByText(/ingest key/, { selector: "p" });
  function withIngestKey(required: boolean) {
    const c = config({ service: "custom", url: "rtmp://relay.example/live", key_mode: "passthrough" });
    Object.assign(c, { ingest_key_required: required });
    (c.urls as { obs_key: string }).obs_key = required ? "ingest-key-0123" : "streamdelay";
    return c;
  }

  it("says OBS streams with the ingest key, and that it is what is forwarded", () => {
    live.config = withIngestKey(true);
    render(SetupTab);
    expect(warning()?.textContent).toMatch(/forward/);
    expect(byHandKey()).toBe("ingest-key-0123");
  });

  it("without one, OBS keeps its own key", () => {
    live.config = withIngestKey(false);
    render(SetupTab);
    expect(warning()).toBeNull();
    expect(byHandKey()).toBe("(your real stream key)");
  });
});

describe("the saved stream key", () => {
  const withKey = () => {
    const c = config({ service: "twitch", url: TWITCH, key_mode: "stored" });
    (c as { destination_key_set: boolean }).destination_key_set = true;
    return c;
  };
  const remove = () => screen.getByRole("button", { name: /Remove saved key|Click again/ });

  afterEach(() => {
    live.state = null;
  });

  it("is removed only on a second click, as it cannot be undone", async () => {
    live.config = withKey();
    render(SetupTab);
    await fireEvent.click(remove());
    expect(api.clearStreamKey).not.toHaveBeenCalled();
    await fireEvent.click(remove());
    expect(api.clearStreamKey).toHaveBeenCalledOnce();
  });

  it("says that removing it while streaming ends the broadcast", async () => {
    live.config = withKey();
    live.state = {
      ended: false,
      ingest: { connected: true },
      egress: { status: "live" },
      delay: { ingest: { bitrate_kbps: 6000 } },
    } as unknown as typeof live.state;
    render(SetupTab);
    await fireEvent.click(remove());
    expect(screen.getByRole("alert").textContent).toMatch(/ends? (your|the) (stream|broadcast)/i);
    expect(api.clearStreamKey).not.toHaveBeenCalled();
  });
});

describe("the dock and overlay links", () => {
  it("says what someone with the dock link can do: dump the buffer and end the stream too", () => {
    render(SetupTab);
    const text = screen.getByText(/neither can change your settings/i).textContent ?? "";
    expect(text).not.toMatch(/can only change the delay/);
    expect(text).toMatch(/dump/i);
    expect(text).toMatch(/end (the|your) (stream|broadcast)/i);
  });
});

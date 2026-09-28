// @vitest-environment jsdom
import { render, screen } from "@testing-library/svelte";
import { afterEach, describe, expect, it } from "vitest";
import { live } from "../lib/live.svelte";
import type { LimitedConfig, RelayState } from "../lib/types";
import Health from "./Health.svelte";

/** Nothing from OBS yet, the RTMP input listening on every interface (as in Docker). */
function waiting(listen: string): RelayState {
  return {
    ended: false,
    ingest: { connected: false, listen, bad_keys_recent: 0 },
    egress: { status: "idle", backlog_bytes: 0 },
    delay: { ingest: {} },
  } as unknown as RelayState;
}

afterEach(() => {
  live.config = null;
});

describe("connections", () => {
  it("tells OBS the address it can stream to, not the one listened on", () => {
    live.config = { urls: { obs_server: "rtmp://127.0.0.1:1935/live" } } as unknown as LimitedConfig;
    for (const listen of ["0.0.0.0:1935", "[::]:1935"]) {
      const { unmount } = render(Health, { state: waiting(listen) });
      const hint = screen.getByText(/stream to/).textContent ?? "";
      expect(hint).toContain("rtmp://127.0.0.1:1935/live");
      expect(hint).not.toContain(listen);
      unmount();
    }
  });
});

describe("the destination", () => {
  const label = (egress: Partial<RelayState["egress"]>) => {
    const s = waiting("127.0.0.1:1935");
    s.egress = { ...s.egress, ...egress };
    const { unmount } = render(Health, { state: s });
    const text = screen.getByText("To destination").nextElementSibling?.textContent ?? "";
    unmount();
    return text;
  };

  it("says when it has no stream key, apart from when there is none", () => {
    expect(label({ status: "disabled", destination: "rtmps://live.twitch.tv/app" })).toContain("No stream key");
    expect(label({ status: "disabled", destination: null })).toContain("No destination set");
  });
});

// @vitest-environment jsdom
import { fireEvent, render, screen } from "@testing-library/svelte";
import { tick } from "svelte";
import { afterEach, describe, expect, it, vi } from "vitest";
import { live } from "../../lib/live.svelte";
import type { PublicConfig } from "../../lib/types";
import ObsWizard from "./ObsWizard.svelte";

vi.mock("../../lib/api", () => ({
  // OBS not reachable: the connect form is shown.
  obsStatus: vi.fn(async () => ({ reachable: false, error: "could not reach OBS" })),
  obsConnect: vi.fn(),
  obsConfigure: vi.fn(),
  obsRestore: vi.fn(),
}));

/** The dashboard's settings, with the OBS address saved and `keep_buffer`. */
function config(host: string, port: number, keepBuffer = true): PublicConfig {
  return {
    scope: "admin",
    config: { obs: { host, port, backup: null }, delay: { keep_buffer: keepBuffer } },
  } as unknown as PublicConfig;
}

const hostField = () => screen.getByLabelText("Host") as HTMLInputElement;
const portField = () => screen.getByLabelText("Port") as HTMLInputElement;

afterEach(() => {
  live.config = null;
});

describe("the OBS address", () => {
  it("is the one saved, and follows it until edited", async () => {
    live.config = config("127.0.0.1", 4455);
    render(ObsWizard);
    await tick();
    expect(hostField().value).toBe("127.0.0.1");
    live.config = config("192.168.1.20", 4460);
    await tick();
    expect(hostField().value).toBe("192.168.1.20");
    expect(portField().value).toBe("4460");
  });

  it("being typed is kept when another setting is saved meanwhile", async () => {
    live.config = config("127.0.0.1", 4455);
    render(ObsWizard);
    await tick();
    await fireEvent.input(hostField(), { target: { value: "192.168.1.5" } });
    // The rolling buffer switched off on the same tab: new settings arrive.
    live.config = config("127.0.0.1", 4455, false);
    await tick();
    expect(hostField().value).toBe("192.168.1.5");
  });
});

import { describe, expect, it } from "vitest";
import { backToMs, formatBitrate, formatBytes, formatDelay, formatSecondsLabel, phaseTone } from "./format";
import type { Snapshot } from "./types";
import { t } from "./i18n";

describe("format", () => {
  it("offers to go back only to a delay an outage stretched", () => {
    const snap = (target_ms: number, excess_ms: number) => ({ target_ms, excess_ms }) as Snapshot;
    expect(backToMs(null)).toBeNull();
    expect(backToMs(snap(30_000, 0))).toBeNull();
    expect(backToMs(snap(30_000, 20_000))).toBe(30_000);
    expect(backToMs(snap(0, 20_000))).toBeNull();
  });
  it("formats delays", () => {
    expect(formatDelay(0)).toBe("0 s");
    expect(formatDelay(30_400)).toBe("30 s");
    expect(formatDelay(125_000)).toBe("2:05");
  });
  it("labels presets", () => {
    expect(formatSecondsLabel(0)).toBe("0 s");
    expect(formatSecondsLabel(15)).toBe("15 s");
    expect(formatSecondsLabel(120)).toBe("2 min");
    expect(formatSecondsLabel(90)).toBe("90 s");
  });
  it("formats rates and sizes", () => {
    expect(formatBitrate(6200)).toBe("6.2 Mbps");
    expect(formatBitrate(800)).toBe("800 kbps");
    expect(formatBytes(3 * 1024 * 1024)).toBe("3.0 MB");
  });
  it("maps phases to tones", () => {
    expect(phaseTone("live")).toBe("live");
    expect(phaseTone("adding")).toBe("busy");
    expect(phaseTone("offline")).toBe("off");
  });
});

describe("i18n", () => {
  it("interpolates", () => {
    expect(t("phase.delayed", { delay: "30 s" })).toBe("Delayed 30 s");
  });
});

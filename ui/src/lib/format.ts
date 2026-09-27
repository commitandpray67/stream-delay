import type { Phase, Snapshot } from "./types";

/**
 * The delay to show once a change has settled: the one asked for, since rounding
 * back to a keyframe stretches the exact one by a second or so; the real one
 * when the buffer or the memory limit keeps it shorter, or an outage made it
 * longer than rounding explains.
 */
export function shownDelayMs(d: Snapshot): number {
  const asked = d.target_ms > 0 && !d.history_short && !d.memory_short && !(d.excess_ms > 0);
  return asked ? d.target_ms : d.effective_ms;
}

/**
 * The delay to offer to go back to, when an outage made the delay longer than
 * set (and rounding explains); else null.
 */
export function backToMs(d: Snapshot | null): number | null {
  return d && d.excess_ms > 0 && d.target_ms > 0 ? d.target_ms : null;
}

/**
 * The delay to offer to set again, when it came out shorter than set for lack
 * of buffer and the buffer now reaches back far enough; else null.
 */
export function setAgainMs(d: Snapshot | null): number | null {
  return d && d.full_delay_ready && d.target_ms > 0 ? d.target_ms : null;
}

/** "0 s", "30 s", "2:05" */
export function formatDelay(ms: number): string {
  const s = Math.round(ms / 1000);
  if (s < 100) return `${s} s`;
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
}

export function formatSecondsLabel(seconds: number): string {
  if (seconds <= 0) return "0 s";
  if (seconds < 60 || seconds % 60 !== 0) return `${seconds} s`;
  return `${seconds / 60} min`;
}

export function formatBitrate(kbps: number): string {
  if (kbps >= 1000) return `${(kbps / 1000).toFixed(1)} Mbps`;
  return `${kbps} kbps`;
}

export function formatBytes(n: number): string {
  if (n >= 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
  if (n >= 1024) return `${(n / 1024).toFixed(0)} KB`;
  return `${n} B`;
}

export function phaseTone(phase: Phase | "ending" | "ended"): "live" | "delayed" | "busy" | "ended" | "off" {
  switch (phase) {
    case "ended":
    case "ending":
      return "ended";
    case "live":
      return "live";
    case "delayed":
      return "delayed";
    case "adding":
    case "going-live":
    case "reducing":
    case "holding":
      return "busy";
    default:
      return "off";
  }
}

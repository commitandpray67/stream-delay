import type { Phase } from "./types";

/** "0 s", "30 s", "2:05" */
export function formatDelay(ms: number): string {
  const s = Math.round(ms / 1000);
  if (s < 100) return `${s} s`;
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
}

export function formatSecondsLabel(seconds: number): string {
  if (seconds <= 0) return "Live";
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

export function phaseTone(phase: Phase): "live" | "delayed" | "busy" | "off" {
  switch (phase) {
    case "live":
      return "live";
    case "delayed":
      return "delayed";
    case "adding":
    case "going-live":
    case "reducing":
      return "busy";
    default:
      return "off";
  }
}

// What the overlay tells viewers about the delay, and when.

import { formatDelay } from "./format";
import type { RelayState } from "./types";

/** A broadcast is running: the destination is connected and the streamer has not ended it. */
function broadcasting(state: RelayState | null): state is RelayState {
  return !!state && !state.ended && state.delay.output.connected;
}

/**
 * The delay viewers have, in whole seconds, once it has settled: 0 for none, and
 * null while a change is still under way or no broadcast is running. The delay
 * asked for counts rather than the exact one, which a keyframe or a dropped
 * connection can stretch by a second or two; unless the buffer was too short to
 * give it.
 */
export function settledDelay(state: RelayState | null): number | null {
  if (!broadcasting(state)) return null;
  const d = state.delay;
  if (d.phase === "live") return 0;
  if (d.phase !== "delayed") return null;
  const ms = d.target_ms > 0 && !d.history_short ? d.target_ms : d.effective_ms;
  return Math.round(ms / 1000);
}

/**
 * The dashboard's overlay preview: this server's overlay page with the overlay
 * link's own token, which can only read, never the dashboard's. Relative, so it
 * loads from wherever the dashboard was opened. Null without a usable link.
 */
export function previewUrl(overlayLink: string | undefined, preview: "mask" | "badge"): string | null {
  let token: string | null = null;
  try {
    token = overlayLink ? new URL(overlayLink).searchParams.get("token") : null;
  } catch {
    return null;
  }
  if (!token) return null;
  return `/overlay?${new URLSearchParams({ preview, token })}`;
}

/** Seconds of delay for the badge, or null when there is none to show. */
export function badgeDelay(state: RelayState | null): number | null {
  if (!broadcasting(state)) return null;
  const secs = settledDelay(state) ?? Math.round(state.delay.effective_ms / 1000);
  return secs >= 1 ? secs : null;
}

/**
 * Decides when the overlay pops up a message: when the streamer changes the
 * delay and the change has taken effect. Not when a broadcast starts or comes
 * back after a dropped connection (its delay is simply taken as it is), not while
 * a change is under way, and not for a dump, which keeps the delay.
 */
export class DelayAnnouncer {
  private shown: number | null = null;

  /** Call with every new state; returns the message to show, if any. */
  update(state: RelayState | null): string | null {
    if (!broadcasting(state)) {
      this.shown = null;
      return null;
    }
    const now = settledDelay(state);
    if (now === null || now === this.shown) return null;
    const first = this.shown === null;
    this.shown = now;
    if (first) return null;
    return now === 0 ? "Stream delay removed" : `Stream delay: ${formatDelay(now * 1000)}`;
  }
}

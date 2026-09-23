// Live connection to the stream-delay WebSocket. Components read `live` reactively.

import { getToken } from "./api";
import type { LimitedConfig, PublicConfig, RelayState } from "./types";

export const live = $state({
  state: null as RelayState | null,
  /** Full settings for dashboard links, the dock/overlay subset for others. */
  config: null as LimitedConfig | null,
  connected: false,
  /** Set when the token is rejected, so the UI can explain instead of retrying forever. */
  unauthorized: false,
});

/** The full settings, or null when this link's token is not the dashboard's. */
export function adminConfig(): PublicConfig | null {
  const c = live.config;
  return c?.scope === "admin" ? (c as PublicConfig) : null;
}

let started = false;

export function connectLive(): void {
  if (started) return;
  started = true;
  let delay = 500;
  const open = () => {
    const proto = location.protocol === "https:" ? "wss" : "ws";
    const ws = new WebSocket(`${proto}://${location.host}/api/v1/events?token=${encodeURIComponent(getToken())}`);
    let opened = false;
    ws.onopen = () => {
      opened = true;
      live.connected = true;
      live.unauthorized = false;
      delay = 500;
    };
    ws.onmessage = (ev) => {
      const msg = JSON.parse(ev.data as string);
      if (msg.type === "state") live.state = msg.state;
      else if (msg.type === "config") live.config = msg.config;
    };
    ws.onclose = async () => {
      live.connected = false;
      if (!opened) {
        // Distinguish a bad token from the app not running.
        try {
          const r = await fetch("/api/v1/state", { headers: { Authorization: `Bearer ${getToken()}` } });
          live.unauthorized = r.status === 401;
        } catch {
          live.unauthorized = false;
        }
      }
      setTimeout(open, delay);
      delay = Math.min(delay * 2, 5000);
    };
  };
  open();
}

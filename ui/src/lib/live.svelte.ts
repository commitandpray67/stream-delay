// Live connection to the stream-delay WebSocket. Components read `live` reactively.

import { getToken } from "./api";
import type { PublicConfig, RelayState } from "./types";

export const live = $state({
  state: null as RelayState | null,
  config: null as PublicConfig | null,
  connected: false,
  /** Set when the token is rejected, so the UI can explain instead of retrying forever. */
  unauthorized: false,
});

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

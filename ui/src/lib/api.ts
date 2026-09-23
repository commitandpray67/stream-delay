import type { Ack, DelayMode, GoLiveWhen, ObsStatus, PublicConfig, RelayState } from "./types";

/**
 * Where a page remembers its token. Dock and overlay links carry tokens that can do
 * less than the dashboard's, so each page keeps its own and opening one never
 * replaces the dashboard's token.
 */
function tokenKey(): string {
  const view = location.pathname.replace(/\/+$/, "");
  return view === "/dock" || view === "/overlay" ? `stream-delay-token:${view.slice(1)}` : "stream-delay-token";
}

/** The API token: from `?token=` in the URL, else remembered from a previous visit. */
export function getToken(): string {
  const fromUrl = new URLSearchParams(location.search).get("token");
  if (fromUrl) {
    try {
      localStorage.setItem(tokenKey(), fromUrl);
    } catch {
      // Storage can be unavailable (private windows, OBS browser sources).
    }
    return fromUrl;
  }
  try {
    return localStorage.getItem(tokenKey()) ?? "";
  } catch {
    return "";
  }
}

export class ApiError extends Error {
  constructor(
    message: string,
    public status: number,
  ) {
    super(message);
  }
}

export async function api<T>(method: string, path: string, body?: unknown): Promise<T> {
  let res: Response;
  try {
    res = await fetch(path, {
      method,
      headers: {
        Authorization: `Bearer ${getToken()}`,
        ...(body === undefined ? {} : { "Content-Type": "application/json" }),
      },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
  } catch {
    throw new ApiError("stream-delay is not reachable. Is it running?", 0);
  }
  const text = await res.text();
  let data: unknown = null;
  try {
    data = text ? JSON.parse(text) : null;
  } catch {
    data = null;
  }
  if (!res.ok) {
    const msg =
      (data as { error?: string } | null)?.error ??
      (res.status === 401 ? "The link is missing its token. Copy it again from the dashboard." : text);
    throw new ApiError(msg || `HTTP ${res.status}`, res.status);
  }
  return data as T;
}

export const setDelay = (seconds: number, mode?: DelayMode) =>
  api<Ack>("PUT", "/api/v1/delay", { seconds, mode });
export const goLive = (when: GoLiveWhen) => api<Ack>("POST", "/api/v1/live", { when });
export const applyPreset = (index: number) => api<Ack>("POST", `/api/v1/presets/${index}`);
export const cancel = () => api<Ack>("POST", "/api/v1/cancel");
export const endStream = () => api<RelayState>("POST", "/api/v1/stream/end");
export const resumeStream = () => api<RelayState>("POST", "/api/v1/stream/resume");
export const getState = () => api<RelayState>("GET", "/api/v1/state");
export const getConfig = () => api<PublicConfig>("GET", "/api/v1/config");
export const updateConfig = (update: Record<string, unknown>) =>
  api<PublicConfig>("PUT", "/api/v1/config", update);
export const setStreamKey = (key: string) => api<PublicConfig>("PUT", "/api/v1/destination/key", { key });
export const clearStreamKey = () => api<PublicConfig>("DELETE", "/api/v1/destination/key");

export const obsStatus = () => api<ObsStatus>("GET", "/api/v1/obs/status");
export const obsConnect = (host: string, port: number, password: string) =>
  api<ObsStatus>("POST", "/api/v1/obs/connect", { host, port, password });
export const obsConfigure = (opts: { import_key: boolean; add_overlay: boolean }) =>
  api<{ status: ObsStatus; imported_key: boolean; overlay_added: boolean; message: string }>(
    "POST",
    "/api/v1/obs/configure",
    opts,
  );
export const obsRestore = () => api<ObsStatus>("POST", "/api/v1/obs/restore");

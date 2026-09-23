// Mirrors the JSON produced by the Rust API (crates/control, crates/relay, crates/engine).

export type Phase = "offline" | "live" | "delayed" | "adding" | "going-live" | "reducing";
export type DelayMode = "rewind" | "mask";
export type GoLiveWhen = "now" | "after-air";
export type EgressStatus = "disabled" | "idle" | "connecting" | "live" | "retrying";

export interface Ack {
  target_ms: number;
  effective_ms: number;
  pending: boolean;
  history_short: boolean;
}

export interface Snapshot {
  phase: Phase;
  target_ms: number;
  effective_ms: number;
  max_delay_ms: number;
  history_ms: number;
  buffered_bytes: number;
  mask_visible: boolean;
  history_short: boolean;
  ingest: {
    active: boolean;
    video_codec: string | null;
    audio_codec: string | null;
    bitrate_kbps: number;
    fps: number;
    gop_ms: number | null;
    enhanced: boolean;
    multitrack: boolean;
  };
  output: { connected: boolean; splices: number; dropped_frames: number; sent_bytes: number };
  warnings: string[];
}

export interface RelayState {
  delay: Snapshot;
  ingest: {
    listen: string;
    connected: boolean;
    peer: string | null;
    app: string | null;
    last_error: string | null;
  };
  egress: {
    status: EgressStatus;
    destination: string | null;
    last_error: string | null;
    bitrate_kbps: number;
    backlog_bytes: number;
    reconnects: number;
  };
  /** The broadcast was ended with "End stream"; nothing is sent until resumed. */
  ended: boolean;
  /** "End stream" was asked for: the broadcast ends once what is buffered has aired. */
  ending: boolean;
}

export interface Preset {
  seconds: number;
  mode: DelayMode;
}

export interface OverlayConfig {
  badge: boolean;
  badge_position: "top-left" | "top-right" | "bottom-left" | "bottom-right";
  popup: boolean;
  mask_title: string;
  mask_subtitle: string;
  accent_color: string;
  background_color: string;
  text_color: string;
  mask_image: string;
}

export interface DelayConfig {
  max_seconds: number;
  start_seconds: number;
  default_mode: DelayMode;
  presets: Preset[];
  ram_cap_mb: number;
  /** Keep a rolling buffer so Rewind can add delay instantly. */
  keep_buffer: boolean;
}

export interface DestinationConfig {
  service: string;
  url: string;
  key_mode: "stored" | "passthrough";
}

export interface HotkeyConfig {
  enabled: boolean;
  go_live: string;
  go_live_after_air: string;
  end_stream: string;
  end_stream_after_air: string;
  dump: string;
  presets: string[];
}

export interface Config {
  ingest: { bind: string; grace_seconds: number };
  destination: DestinationConfig;
  delay: DelayConfig;
  api: { bind: string; token: string; allow_lan: boolean };
  overlay: OverlayConfig;
  obs: { host: string; port: number; backup: unknown | null };
  hotkeys: HotkeyConfig;
}

/** What a link's token may do: overlay links read, dock links control, the dashboard is admin. */
export type Scope = "read" | "control" | "admin";

/** Settings every link receives: what the dock and overlay display. */
export interface LimitedConfig {
  scope: Scope;
  config: { delay: DelayConfig; overlay: OverlayConfig };
  urls: { obs_server: string };
  version: string;
  /** The script the server's web UI starts from; a page running another is out of date. */
  ui_build: string | null;
}

/** The full settings, sent only to dashboard (admin) links. */
export interface PublicConfig extends LimitedConfig {
  scope: "admin";
  config: Config;
  destination_key_set: boolean;
  secrets_backend: string;
  urls: { dashboard: string; dock: string; overlay: string; obs_server: string; obs_key: string };
  services: { id: string; name: string; url: string }[];
  restart_required: boolean;
}

export interface ObsStatus {
  reachable: boolean;
  version: string | null;
  streaming: boolean;
  configured: boolean;
  current_server: string | null;
  error: string | null;
  has_backup: boolean;
  password_saved: boolean;
}

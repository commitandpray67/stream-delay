<script lang="ts">
  import { formatBitrate, formatBytes } from "../lib/format";
  import { live } from "../lib/live.svelte";
  import type { RelayState } from "../lib/types";

  let { state }: { state: RelayState | null } = $props();

  // Where OBS reaches the RTMP input, as on the Setup tab: not the address it
  // listens on, which may be every interface (0.0.0.0 or [::]).
  const obsServer = $derived(live.config?.urls.obs_server ?? `rtmp://${state?.ingest.listen}/live`);

  const egressLabel: Record<string, string> = {
    disabled: "No destination set",
    idle: "Waiting for OBS",
    connecting: "Connecting…",
    live: "Live",
    retrying: "Reconnecting…",
  };
</script>

{#if state}
  <dl class="health">
    <dt>From OBS</dt>
    <dd>
      {#if state.ingest.connected}
        <span class="ok">Receiving</span>
        · {formatBitrate(state.delay.ingest.bitrate_kbps)}
        · {state.delay.ingest.fps} fps
        {#if state.delay.ingest.gop_ms}· keyframe every {(state.delay.ingest.gop_ms / 1000).toFixed(1)} s{/if}
        {#if state.delay.ingest.video_codec}· {state.delay.ingest.video_codec.toUpperCase()}{/if}
      {:else}
        <span class="muted">Not connected. In OBS, stream to <code>{obsServer}</code></span>
      {/if}
    </dd>
    <dt>To destination</dt>
    <dd>
      <span class:ok={state.egress.status === "live"} class:warn={state.egress.status === "retrying"}>
        {state.ended && state.egress.status !== "live" ? "Stopped (stream ended)" : egressLabel[state.egress.status]}
      </span>
      {#if state.egress.destination}<span class="muted"> · {state.egress.destination}</span>{/if}
      {#if state.egress.status === "live"} · {formatBitrate(state.egress.bitrate_kbps)}{/if}
      {#if state.egress.backlog_bytes > 1_000_000}
        <div class="warn">Upload is falling behind ({formatBytes(state.egress.backlog_bytes)} queued). Lower your bitrate.</div>
      {/if}
      {#if state.egress.last_error && state.egress.status !== "live"}
        <div class="error">{state.egress.last_error}</div>
      {/if}
    </dd>
    {#if state.ingest.last_error}
      <dt>OBS connection</dt>
      <dd class="error">{state.ingest.last_error}</dd>
    {/if}
    {#if state.ingest.bad_keys_recent}
      <dt>Wrong keys</dt>
      <dd class="warn">
        {state.ingest.bad_keys_recent} wrong ingest {state.ingest.bad_keys_recent === 1 ? "key" : "keys"} in the last
        10 minutes{#if state.ingest.bad_key_from}, the latest from {state.ingest.bad_key_from}{/if}. If that isn't
        your encoder, someone may be guessing the key: change it (the <code>ingest.key</code> setting) and firewall
        the RTMP port to your encoder's address.
      </dd>
    {/if}
    {#if state.ingest.key_warning}
      <dt>Ingest key</dt>
      <dd class="warn">{state.ingest.key_warning}</dd>
    {/if}
  </dl>
{/if}

<style>
  .health {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: 0.35rem 0.9rem;
    margin: 0;
    font-size: 0.9rem;
  }
  dt {
    color: var(--muted);
  }
  dd {
    margin: 0;
  }
  .ok {
    color: var(--live);
    font-weight: 600;
  }
  .warn {
    color: var(--delayed);
  }
</style>

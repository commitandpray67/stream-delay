<script lang="ts">
  import DelayControls from "../components/DelayControls.svelte";
  import { formatBitrate } from "../lib/format";
  import { t } from "../lib/i18n";
  import { live } from "../lib/live.svelte";

  const egressLabel: Record<string, string> = {
    disabled: "no stream key",
    idle: "idle",
    connecting: "connecting…",
    live: "live",
    retrying: "reconnecting…",
  };
</script>

<main class="dock">
  {#if live.unauthorized}
    <p class="notice">{t("conn.unauthorized")}</p>
  {:else if !live.connected}
    <p class="notice">{t("conn.lost")}</p>
  {/if}
  <DelayControls compact />
  {#if live.state}
    {#if !live.state.ingest.connected}
      <p class="notice">
        <b>Waiting for OBS.</b> In OBS, Settings → Stream: Service <b>Custom…</b>, Server
        <code>{live.config?.urls.obs_server ?? `rtmp://${live.state.ingest.listen}/live`}</code>, any stream key. If OBS
        is already live, it is streaming somewhere else.
        {#if live.state.ingest.last_error}
          <br /><span class="error">Last attempt: {live.state.ingest.last_error}</span>
        {/if}
      </p>
    {:else if live.state.egress.status === "disabled"}
      <p class="notice"><b>No stream key yet.</b> Add it on the dashboard's Setup tab.</p>
    {/if}
    <footer class="muted">
      From OBS: {live.state.ingest.connected ? "receiving" : "not connected"}
      · To destination: {egressLabel[live.state.egress.status] ?? live.state.egress.status}
      {#if live.state.egress.status === "live"}({formatBitrate(live.state.egress.bitrate_kbps)}){/if}
    </footer>
  {/if}
</main>

<style>
  .dock {
    padding: 0.6rem;
    display: grid;
    gap: 0.6rem;
    font-size: 14px;
  }
  footer {
    font-size: 0.8rem;
  }
  p {
    margin: 0;
  }
  code {
    user-select: all;
    word-break: normal;
    overflow-wrap: anywhere;
  }
</style>

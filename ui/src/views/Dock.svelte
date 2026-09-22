<script lang="ts">
  import DelayControls from "../components/DelayControls.svelte";
  import { formatBitrate } from "../lib/format";
  import { t } from "../lib/i18n";
  import { live } from "../lib/live.svelte";
</script>

<main class="dock">
  {#if live.unauthorized}
    <p class="notice">{t("conn.unauthorized")}</p>
  {:else if !live.connected}
    <p class="notice">{t("conn.lost")}</p>
  {/if}
  <DelayControls compact />
  {#if live.state}
    <footer class="muted">
      OBS {live.state.ingest.connected ? "✓" : "✗"}
      · Out: {live.state.egress.status}
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
</style>

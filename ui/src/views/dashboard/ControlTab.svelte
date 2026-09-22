<script lang="ts">
  import DelayControls from "../../components/DelayControls.svelte";
  import Health from "../../components/Health.svelte";
  import { live } from "../../lib/live.svelte";
</script>

<div class="grid">
  <section class="panel" aria-labelledby="control-h">
    <h2 id="control-h">Delay</h2>
    <DelayControls />
  </section>
  <section class="panel" aria-labelledby="health-h">
    <h2 id="health-h">Connections</h2>
    <Health state={live.state} />
    {#if live.state}
      <p class="muted small">
        {live.state.delay.output.splices} delay changes this stream ·
        {live.state.egress.reconnects} reconnects
      </p>
    {/if}
  </section>
</div>

<style>
  .grid {
    display: grid;
    grid-template-columns: minmax(0, 1.2fr) minmax(0, 1fr);
    gap: 1rem;
  }
  @media (max-width: 760px) {
    .grid {
      grid-template-columns: 1fr;
    }
  }
  h2 {
    margin: 0 0 0.75rem;
    font-size: 1.05rem;
  }
  .small {
    font-size: 0.85rem;
  }
</style>

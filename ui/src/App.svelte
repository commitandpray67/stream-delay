<script lang="ts">
  import { connectLive } from "./lib/live.svelte";
  import Dashboard from "./views/Dashboard.svelte";
  import Dock from "./views/Dock.svelte";
  import Overlay from "./views/Overlay.svelte";

  const view = location.pathname.replace(/\/+$/, "") || "/";
  if (view === "/overlay") document.body.classList.add("overlay");
  connectLive();
</script>

{#if view === "/overlay"}
  <Overlay />
{:else if view === "/dock"}
  <Dock />
{:else}
  <Dashboard />
{/if}

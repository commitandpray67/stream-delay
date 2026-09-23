<script lang="ts">
  import { updateConfig } from "../../lib/api";
  import { formatSecondsLabel } from "../../lib/format";
  import { adminConfig } from "../../lib/live.svelte";
  import type { DelayConfig, DelayMode } from "../../lib/types";

  let form = $state<DelayConfig | null>(null);
  let grace = $state(30);
  let message = $state("");
  let error = $state("");

  $effect(() => {
    const c = adminConfig()?.config;
    if (c && !form) {
      form = structuredClone($state.snapshot(c.delay)) as DelayConfig;
      grace = c.ingest.grace_seconds;
    }
  });

  function addPreset() {
    form!.presets.push({ seconds: 45, mode: "rewind" });
  }

  async function save(e: Event) {
    e.preventDefault();
    message = error = "";
    try {
      await updateConfig({ delay: $state.snapshot(form), grace_seconds: Number(grace) });
      message = "Saved.";
    } catch (err) {
      error = (err as Error).message;
    }
  }
</script>

{#if form}
  <form class="panel stack" onsubmit={save}>
    <h2>Delay settings</h2>
    <fieldset class="stack">
      <legend>Presets (buttons in the dock, hotkeys 1–{form.presets.length})</legend>
      {#each form.presets as p, i (i)}
        <div class="row preset">
          <label>
            <span class="sr-only">Preset {i + 1} seconds</span>
            <input type="number" min="0" max={form.max_seconds} bind:value={p.seconds} />
          </label>
          <span class="muted">{formatSecondsLabel(Number(p.seconds))}</span>
          <select bind:value={p.mode} aria-label="Preset {i + 1} mode">
            {#each ["rewind", "mask"] as m (m)}<option value={m as DelayMode}>{m}</option>{/each}
          </select>
          <button type="button" onclick={() => form!.presets.splice(i, 1)} disabled={form.presets.length <= 1}>
            Remove
          </button>
        </div>
      {/each}
      <div><button type="button" onclick={addPreset} disabled={form.presets.length >= 10}>Add preset</button></div>
    </fieldset>
    <div class="cols">
      <label>
        Default mode
        <select bind:value={form.default_mode}>
          <option value="rewind">Rewind (instant)</option>
          <option value="mask">Mask (slate covers the change)</option>
        </select>
      </label>
      <label>
        Delay when a stream starts (s)
        <input type="number" min="0" max={form.max_seconds} bind:value={form.start_seconds} />
      </label>
      <label>
        Maximum delay (s)
        <input type="number" min="5" max="900" bind:value={form.max_seconds} />
      </label>
      <label>
        Memory cap (MiB)
        <input type="number" min="16" max="16384" bind:value={form.ram_cap_mb} />
      </label>
      <label>
        Keep destination connected after OBS disconnects (s)
        <input type="number" min="0" max="600" bind:value={grace} />
      </label>
    </div>
    <p class="muted small">
      The buffer needs about bitrate × maximum delay of memory: 6 Mbps × 120 s ≈ 90 MB. Changing the maximum
      delay, memory cap or reconnect time takes effect after a restart.
    </p>
    <div class="row">
      <button class="primary" type="submit">Save</button>
      {#if message}<span class="ok">{message}</span>{/if}
      {#if error}<span class="error" role="alert">{error}</span>{/if}
    </div>
  </form>
{/if}

<style>
  h2 {
    margin: 0;
    font-size: 1.05rem;
  }
  fieldset {
    border: 1px solid var(--border);
    border-radius: var(--radius);
    padding: 0.75rem;
  }
  legend {
    color: var(--muted);
    padding: 0 0.3rem;
  }
  .preset input {
    width: 6rem;
  }
  .preset .muted {
    min-width: 4rem;
  }
  .cols {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(14rem, 1fr));
    gap: 0.75rem;
  }
  .small {
    font-size: 0.85rem;
    margin: 0;
  }
  .ok {
    color: var(--live);
  }
</style>

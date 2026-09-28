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

  /** Memory for `seconds` of a 10 Mbps stream, with some to spare, in MiB. */
  function memoryFor(seconds: number): number {
    return Math.ceil(seconds * 1.3);
  }

  function addPreset() {
    form!.presets.push({ seconds: 45, mode: "rewind" });
  }

  /** Every number in the form. A number field emptied holds null, not 0. */
  function numbers(f: DelayConfig): unknown[] {
    return [f.start_seconds, f.max_seconds, f.ram_cap_mb, f.mask_margin_ms, grace, ...f.presets.map((p) => p.seconds)];
  }

  async function save(e: Event) {
    e.preventDefault();
    message = error = "";
    if (!numbers(form!).every((n) => typeof n === "number" && Number.isFinite(n))) {
      error = "Enter a number in every field.";
      return;
    }
    try {
      // The rolling-buffer switch lives on the Setup tab; keep its current value.
      const delay = { ...$state.snapshot(form), keep_buffer: adminConfig()?.config.delay.keep_buffer ?? true };
      await updateConfig({ delay, grace_seconds: grace });
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
        Slate margin (ms)
        <input type="number" min="500" max="5000" step="100" bind:value={form.mask_margin_ms} />
      </label>
      <label>
        Wait for OBS to reconnect after a crash or dropped connection (s)
        <input type="number" min="0" max="600" bind:value={grace} />
      </label>
      <label>
        When the connection to the destination comes back
        <select bind:value={form.after_reconnect}>
          <option value="keep">Resume where it left off (the delay gets longer)</option>
          <option value="restore">Go back to the delay set (viewers miss the gap)</option>
        </select>
      </label>
    </div>
    <label class="inline">
      <input type="checkbox" bind:checked={form.mute_under_slate} />
      Leave out the sound while the slate covers a dump
    </label>
    <p class="muted small">
      The buffer needs about bitrate × maximum delay of memory: 6 Mbps × 120 s ≈ 90 MB. The slate margin is how
      long Mask waits after putting the slate up before what OBS sends counts as covered; raise it if gameplay
      shows through when a Mask change starts. After a dump, what the slate covers airs almost live: the slate
      hides the picture, not the sound, unless it is left out. After a dropped connection, resuming where it left
      off shows viewers everything and makes the delay longer by the outage (up to the maximum, and the dock offers
      to go back); going back to the delay set keeps it, and viewers miss what happened while it was down.
      Changing the maximum delay, memory cap, slate margin, sound setting or reconnect time takes effect after a
      restart.
    </p>
    {#if form.ram_cap_mb < memoryFor(form.max_seconds)}
      <p class="warn small" role="status">
        At 10 Mbps, a {form.max_seconds} s maximum needs about {memoryFor(form.max_seconds)} MiB. With
        {form.ram_cap_mb} MiB, a long delay is shortened to what memory holds.
      </p>
    {/if}
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
  .warn {
    color: var(--delayed);
  }
</style>

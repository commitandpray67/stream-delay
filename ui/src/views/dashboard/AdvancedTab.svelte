<script lang="ts">
  import CopyField from "../../components/CopyField.svelte";
  import { getToken, updateConfig } from "../../lib/api";
  import { adminConfig } from "../../lib/live.svelte";
  import type { HotkeyConfig } from "../../lib/types";

  const pc = $derived(adminConfig());
  let hotkeys = $state<HotkeyConfig | null>(null);
  let allowLan = $state(false);
  let message = $state("");
  let error = $state("");

  $effect(() => {
    const c = pc?.config;
    if (c && !hotkeys) {
      hotkeys = structuredClone($state.snapshot(c.hotkeys)) as HotkeyConfig;
      allowLan = c.api.allow_lan;
    }
  });

  async function save(e: Event) {
    e.preventDefault();
    message = error = "";
    try {
      await updateConfig({ hotkeys: $state.snapshot(hotkeys), allow_lan: allowLan });
      message = "Saved.";
    } catch (err) {
      error = (err as Error).message;
    }
  }
</script>

{#if pc && hotkeys}
  <div class="stack">
    <form class="panel stack" onsubmit={save}>
      <h2>Global hotkeys</h2>
      <p class="muted small">
        Available in the desktop app. They work while any window is focused, including your game. Use names like
        <code>CmdOrCtrl+Alt+Shift+1</code>; leave a field empty to disable it.
      </p>
      <label class="inline"><input type="checkbox" bind:checked={hotkeys.enabled} /> Enable global hotkeys</label>
      <div class="cols">
        <label>Go live now <input bind:value={hotkeys.go_live} /></label>
        <label>Go live after it airs <input bind:value={hotkeys.go_live_after_air} /></label>
        {#each pc.config.delay.presets as p, i (i)}
          <label>
            Preset {i + 1} ({p.seconds <= 0 ? "live" : `${p.seconds} s`})
            <input bind:value={hotkeys.presets[i]} />
          </label>
        {/each}
      </div>
      <h2>Network</h2>
      <label class="inline">
        <input type="checkbox" bind:checked={allowLan} />
        Allow control from other devices on my network (phone, second PC). Needs a restart and the API to listen on
        your network address.
      </label>
      <div class="row">
        <button class="primary" type="submit">Save</button>
        {#if message}<span class="ok">{message}</span>{/if}
        {#if error}<span class="error" role="alert">{error}</span>{/if}
      </div>
    </form>

    <section class="panel stack">
      <h2>Diagnostics</h2>
      <p class="muted small">
        When reporting a problem, attach this file to your issue. It holds the version, your settings, the current
        state and recent log lines. Stream keys, passwords and the API token are removed.
      </p>
      <div class="row">
        <a class="button" href={`/api/v1/diagnostics?token=${encodeURIComponent(getToken())}`} download>
          Download diagnostics
        </a>
      </div>
    </section>

    <section class="panel stack">
      <h2>API and integrations</h2>
      <p class="muted small">
        Stream Deck (with a web-request plugin), Streamer.bot, Firebot, Touch Portal or scripts can call the HTTP API.
        Send the token as <code>Authorization: Bearer &lt;token&gt;</code>. Examples:
        <code>PUT /api/v1/delay {'{'}"seconds": 30{'}'}</code>,
        <code>POST /api/v1/live {'{'}"when": "after-air"{'}'}</code>,
        <code>POST /api/v1/presets/2</code>, <code>GET /api/v1/state</code>.
      </p>
      <CopyField label="Dashboard link (contains the token)" value={pc.urls.dashboard} secret />
      <p class="muted small">
        stream-delay {pc.version} · secrets are stored in {pc.secrets_backend} ·
        <a href="https://commitandpray67.github.io/stream-delay/" target="_blank" rel="noreferrer">user guide</a> ·
        <a href="https://github.com/commitandpray67/stream-delay" target="_blank" rel="noreferrer">source code (GPL-3.0)</a>
      </p>
    </section>
  </div>
{/if}

<style>
  h2 {
    margin: 0;
    font-size: 1.05rem;
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

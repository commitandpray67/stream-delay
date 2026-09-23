<script lang="ts">
  import CopyField from "../../components/CopyField.svelte";
  import { checkUpdates, diagnosticsLink, updateConfig } from "../../lib/api";
  import { adminConfig } from "../../lib/live.svelte";
  import type { HotkeyConfig } from "../../lib/types";

  const pc = $derived(adminConfig());
  let hotkeys = $state<HotkeyConfig | null>(null);
  let allowLan = $state(false);
  let message = $state("");
  let error = $state("");
  let diagnosticsError = $state("");
  let updates = $state<{ message: string; releases?: string; error?: boolean } | null>(null);

  async function updateCheck() {
    updates = { message: "Checking…" };
    try {
      const r = await checkUpdates();
      updates = r.checking
        ? { message: "Checking for updates. The app asks before installing one." }
        : { message: "This copy doesn't update itself. The latest release is on GitHub:", releases: r.releases };
    } catch (err) {
      updates = { message: (err as Error).message, error: true };
    }
  }

  $effect(() => {
    const c = pc?.config;
    if (c && !hotkeys) {
      hotkeys = structuredClone($state.snapshot(c.hotkeys)) as HotkeyConfig;
      allowLan = c.api.allow_lan;
    }
  });

  /** Downloads through a single-use link, so the token never ends up in a URL. */
  async function downloadDiagnostics() {
    diagnosticsError = "";
    try {
      const { url } = await diagnosticsLink();
      const a = document.createElement("a");
      a.href = url;
      a.download = "";
      document.body.append(a);
      a.click();
      a.remove();
    } catch (err) {
      diagnosticsError = (err as Error).message;
    }
  }

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
        <label>Remove delay now <input bind:value={hotkeys.go_live} /></label>
        <label>Remove delay after it airs <input bind:value={hotkeys.go_live_after_air} /></label>
        <label>Dump buffer <input bind:value={hotkeys.dump} placeholder="none" /></label>
        <label>End stream (after the buffer airs) <input bind:value={hotkeys.end_stream_after_air} placeholder="none" /></label>
        <label>End stream now <input bind:value={hotkeys.end_stream} placeholder="none" /></label>
        {#each pc.config.delay.presets as p, i (i)}
          <label>
            Preset {i + 1} ({p.seconds <= 0 ? "no delay" : `${p.seconds} s`})
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
      <h2>Updates</h2>
      <p class="muted small">You have stream-delay {pc.version}.</p>
      <div class="row">
        <button type="button" onclick={updateCheck}>Check for updates</button>
        {#if updates}
          <span class={updates.error ? "error" : "muted small"} role={updates.error ? "alert" : undefined}>
            {updates.message}
            {#if updates.releases}
              <a href={updates.releases} target="_blank" rel="noreferrer">latest release</a>
            {/if}
          </span>
        {/if}
      </div>
    </section>

    <section class="panel stack">
      <h2>Diagnostics</h2>
      <p class="muted small">
        When reporting a problem, attach this file to your issue. It holds the version, your settings, the current
        state and recent log lines. Stream keys, passwords and the API token are removed.
      </p>
      <div class="row">
        <button type="button" onclick={downloadDiagnostics}>Download diagnostics</button>
        {#if diagnosticsError}<span class="error" role="alert">{diagnosticsError}</span>{/if}
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

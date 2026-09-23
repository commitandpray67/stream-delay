<script lang="ts">
  import { obsConfigure, obsConnect, obsRestore, obsStatus } from "../../lib/api";
  import { adminConfig } from "../../lib/live.svelte";
  import type { ObsStatus } from "../../lib/types";

  let status = $state<ObsStatus | null>(null);
  let host = $state("127.0.0.1");
  let port = $state(4455);
  let password = $state("");
  let importKey = $state(true);
  let addOverlay = $state(true);
  let busy = $state(false);
  let error = $state("");
  let message = $state("");

  $effect(() => {
    const c = adminConfig()?.config.obs;
    if (c) {
      host = c.host;
      port = c.port;
    }
  });

  async function refresh() {
    try {
      status = await obsStatus();
    } catch (e) {
      error = (e as Error).message;
    }
  }
  refresh();

  async function run(action: () => Promise<void>) {
    busy = true;
    error = message = "";
    try {
      await action();
    } catch (e) {
      error = (e as Error).message;
    } finally {
      busy = false;
    }
  }

  const connect = (e: Event) => {
    e.preventDefault();
    return run(async () => {
      status = await obsConnect(host, Number(port), password);
      password = "";
    });
  };
  const configure = () =>
    run(async () => {
      const r = await obsConfigure({ import_key: importKey, add_overlay: addOverlay });
      status = r.status;
      message = r.message;
    });
  const restore = () =>
    run(async () => {
      status = await obsRestore();
      message = "OBS stream settings restored.";
    });
</script>

<div class="stack">
  <p class="muted small">
    stream-delay can configure OBS for you through OBS's built-in WebSocket server (OBS 28 or newer). In OBS,
    open <b>Tools → WebSocket Server Settings</b>, tick <b>Enable WebSocket server</b> and click <b>Show Connect
    Info</b> for the password.
  </p>

  {#if !status?.reachable}
    <form class="row" onsubmit={connect}>
      <label>Host <input bind:value={host} size="12" /></label>
      <label>Port <input type="number" bind:value={port} min="1" max="65535" style="width:6rem" /></label>
      <label>
        Password
        <input
          type="password"
          bind:value={password}
          autocomplete="off"
          placeholder={status?.password_saved ? "(saved)" : ""}
        />
      </label>
      <button class="primary" type="submit" disabled={busy}>Connect to OBS</button>
    </form>
  {:else}
    <p>
      Connected to OBS {status.version}.
      {#if status.configured}<span class="ok">OBS is streaming through stream-delay.</span>
      {:else}OBS currently streams to <code>{status.current_server ?? "?"}</code>.{/if}
    </p>
    {#if status.streaming}
      <p class="notice">OBS is live. Stop streaming before changing its stream settings.</p>
    {/if}
    {#if !status.configured}
      <label class="inline"><input type="checkbox" bind:checked={importKey} /> Move my Twitch stream key from OBS into stream-delay</label>
      <label class="inline"><input type="checkbox" bind:checked={addOverlay} /> Add the overlay to my current scene</label>
      <div class="row">
        <button class="primary" disabled={busy || status.streaming} onclick={configure}>Set up OBS automatically</button>
      </div>
    {/if}
    {#if status.has_backup}
      <div class="row">
        <button disabled={busy || status.streaming} onclick={restore}>Restore my original OBS settings</button>
      </div>
    {/if}
  {/if}
  {#if status?.error && !status.reachable}<p class="muted small">{status.error}</p>{/if}
  {#if message}<p class="ok">{message}</p>{/if}
  {#if error}<p class="error" role="alert">{error}</p>{/if}
</div>

<style>
  p {
    margin: 0;
  }
  .small {
    font-size: 0.85rem;
  }
  .ok {
    color: var(--live);
  }
</style>

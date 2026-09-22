<script lang="ts">
  import CopyField from "../../components/CopyField.svelte";
  import { clearStreamKey, setStreamKey, updateConfig } from "../../lib/api";
  import { live } from "../../lib/live.svelte";
  import ObsWizard from "./ObsWizard.svelte";

  const pc = $derived(live.config);
  let service = $state("");
  let url = $state("");
  let passthrough = $state(false);
  let key = $state("");
  let message = $state("");
  let error = $state("");
  let loaded = false;

  $effect(() => {
    if (pc && !loaded) {
      loaded = true;
      service = pc.config.destination.service;
      url = pc.config.destination.url;
      passthrough = pc.config.destination.key_mode === "passthrough";
    }
  });

  function pickService(id: string) {
    service = id;
    const s = pc?.services.find((x) => x.id === id);
    if (s && s.url) url = s.url;
  }

  async function save(e: Event) {
    e.preventDefault();
    error = message = "";
    try {
      await updateConfig({
        destination: { service, url: url.trim(), key_mode: passthrough ? "passthrough" : "stored" },
      });
      if (key.trim()) {
        await setStreamKey(key.trim());
        key = "";
      }
      message = "Saved.";
    } catch (err) {
      error = (err as Error).message;
    }
  }

  async function forgetKey() {
    error = message = "";
    try {
      await clearStreamKey();
      message = "Stream key removed.";
    } catch (err) {
      error = (err as Error).message;
    }
  }
</script>

{#if pc}
  <div class="stack">
    <section class="panel stack" aria-labelledby="dest-h">
      <h2 id="dest-h">1. Where to stream</h2>
      <form class="stack" onsubmit={save}>
        <label>
          Service
          <select value={service} onchange={(e) => pickService((e.target as HTMLSelectElement).value)}>
            {#each pc.services as s (s.id)}<option value={s.id}>{s.name}</option>{/each}
          </select>
        </label>
        <label>
          Server URL
          <input bind:value={url} placeholder="rtmp://live.twitch.tv/app" required spellcheck="false" />
        </label>
        <label class="inline">
          <input type="checkbox" bind:checked={passthrough} />
          Use the stream key entered in OBS instead (passthrough)
        </label>
        {#if !passthrough}
          <label>
            Stream key {pc.destination_key_set ? "(saved, enter a new one to replace it)" : ""}
            <input
              type="password"
              bind:value={key}
              autocomplete="off"
              placeholder={pc.destination_key_set ? "••••••••••••" : "Paste your stream key"}
            />
          </label>
          <p class="muted small">
            Find it in your Twitch Creator Dashboard → Settings → Stream. It is stored in {pc.secrets_backend}
            and is never shown again. Tip: add <code>?bandwidthtest=true</code> to the end of a Twitch key to test
            without going live.
          </p>
        {/if}
        <div class="row">
          <button class="primary" type="submit">Save</button>
          {#if pc.destination_key_set && !passthrough}
            <button type="button" class="danger" onclick={forgetKey}>Remove saved key</button>
          {/if}
          {#if message}<span class="ok">{message}</span>{/if}
          {#if error}<span class="error" role="alert">{error}</span>{/if}
        </div>
      </form>
    </section>

    <section class="panel stack" aria-labelledby="obs-h">
      <h2 id="obs-h">2. Point OBS at stream-delay</h2>
      <ObsWizard />
      <details>
        <summary>Set up OBS by hand instead</summary>
        <ol class="steps">
          <li>
            In OBS, open <b>Settings → Stream</b>, set <b>Service</b> to <b>Custom…</b> and paste:
            <div class="stack pad">
              <CopyField label="Server" value={pc.urls.obs_server} />
              <CopyField label="Stream Key" value={passthrough ? "(your real stream key)" : "streamdelay"} />
            </div>
          </li>
          <li>In <b>Settings → Output</b>, set the keyframe interval to <b>2 s</b>.</li>
        </ol>
      </details>
    </section>

    <section class="panel stack" aria-labelledby="dock-h">
      <h2 id="dock-h">3. Add the controls to OBS</h2>
      <p class="muted small">
        Dock: OBS → <b>Docks → Custom Browser Docks…</b>, name it “Stream Delay” and paste the URL.<br />
        Overlay: add a <b>Browser</b> source to your scenes with the overlay URL at your canvas size (for example
        1920×1080). It shows the delay badge and the slate used by Mask mode.
      </p>
      <CopyField label="Dock URL" value={pc.urls.dock} />
      <CopyField label="Overlay URL" value={pc.urls.overlay} />
      <p class="muted small">These links contain your private access token. Don't share them on stream.</p>
    </section>
  </div>
{/if}

<style>
  h2 {
    margin: 0;
    font-size: 1.05rem;
  }
  .small {
    font-size: 0.85rem;
    margin: 0;
  }
  .ok {
    color: var(--live);
  }
  .steps {
    display: grid;
    gap: 0.75rem;
    padding-left: 1.2rem;
  }
  .pad {
    margin-top: 0.5rem;
  }
  summary {
    cursor: pointer;
    color: var(--muted);
  }
</style>

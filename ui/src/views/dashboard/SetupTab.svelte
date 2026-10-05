<script lang="ts">
  import { untrack } from "svelte";
  import CopyField from "../../components/CopyField.svelte";
  import { clearStreamKey, setStreamKey, updateConfig } from "../../lib/api";
  import { adminConfig, live } from "../../lib/live.svelte";
  import ObsWizard from "./ObsWizard.svelte";

  const pc = $derived(adminConfig());
  let service = $state("");
  let url = $state("");
  let passthrough = $state(false);
  let key = $state("");
  let message = $state("");
  let error = $state("");
  // The saved destination the fields were last loaded from. It can change
  // while the tab is open: setting up OBS below and moving its Twitch key in
  // makes Twitch the destination, with the key stored.
  let loaded: [string, string, boolean] | null = null;
  const same = (a: [string, string, boolean], b: [string, string, boolean]) =>
    a.every((v, i) => v === b[i]);

  $effect(() => {
    const d = pc?.config.destination;
    if (!d) return;
    const saved: [string, string, boolean] = [d.service, d.url, d.key_mode === "passthrough"];
    untrack(() => {
      if (loaded && same(loaded, saved)) return;
      // Fields the streamer has not edited since follow the saved settings;
      // edits are kept, and saving them is theirs to do.
      if (!loaded || same(loaded, [service, url, passthrough])) {
        [service, url, passthrough] = saved;
      }
      loaded = saved;
    });
  });

  // Where the stream key is found, and Twitch's bandwidth test, depend on the service.
  const twitch = $derived(service.startsWith("twitch"));

  function pickService(id: string) {
    service = id;
    const s = pc?.services.find((x) => x.id === id);
    if (s && s.url) url = s.url;
  }

  async function save(e: Event) {
    e.preventDefault();
    error = message = "";
    try {
      const hadKey = pc?.destination_key_set ?? false;
      const saved = await updateConfig({
        destination: { service, url: url.trim(), key_mode: passthrough ? "passthrough" : "stored" },
      });
      // The server field may have held the key (rtmp://host/app/<key>); it is stored separately.
      url = saved.config.destination.url;
      if (key.trim()) {
        await setStreamKey(key.trim());
        key = "";
        message = "Saved.";
      } else if (hadKey && !saved.destination_key_set && !passthrough) {
        message = "Saved. The stream key was removed because the server changed: enter the key for the new server.";
      } else {
        message = "Saved.";
      }
    } catch (err) {
      error = (err as Error).message;
    }
  }

  let bufferMessage = $state("");
  let bufferError = $state("");

  async function setKeepBuffer(keep: boolean) {
    bufferMessage = bufferError = "";
    if (!pc) return;
    try {
      await updateConfig({ delay: { ...pc.config.delay, keep_buffer: keep } });
      bufferMessage = keep ? "Rewind is available." : "Delay will be added behind the slate (Mask).";
    } catch (err) {
      bufferError = (err as Error).message;
    }
  }

  // Memory the rolling buffer needs at the current bitrate, if known.
  const bufferMb = $derived.by(() => {
    const kbps = live.state?.delay.ingest.bitrate_kbps ?? 0;
    const max = pc?.config.delay.max_seconds ?? 120;
    return kbps > 0 ? Math.round((kbps * max) / 8 / 1000) : null;
  });

  // Sending to the destination, or about to: without its key, the
  // connection is refused, and the broadcast ends.
  const streaming = $derived.by(() => {
    const s = live.state;
    return !!s && !s.ended && (s.ingest.connected || ["connecting", "live", "retrying"].includes(s.egress.status));
  });

  // Removing the key cannot be undone: a second click confirms.
  let confirmForget = $state(false);
  let disarmForget: ReturnType<typeof setTimeout> | undefined;

  async function forgetKey() {
    error = message = "";
    if (!confirmForget) {
      confirmForget = true;
      clearTimeout(disarmForget);
      disarmForget = setTimeout(() => (confirmForget = false), 5000);
      return;
    }
    confirmForget = false;
    clearTimeout(disarmForget);
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
          <input bind:value={url} placeholder="rtmps://ingest.example.com/live" required spellcheck="false" />
        </label>
        <label class="inline">
          <input type="checkbox" bind:checked={passthrough} />
          Use the stream key entered in OBS instead (passthrough)
        </label>
        {#if passthrough && pc.ingest_key_required}
          <p class="notice small" role="status">
            OBS has to stream to stream-delay with its ingest key (step 2), and passthrough forwards the key OBS
            streams with: the destination gets the ingest key, which only works if it is your stream key. Untick
            passthrough and enter your stream key here instead.
          </p>
        {/if}
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
            {#if twitch}
              Find it in your Twitch Creator Dashboard → Settings → Stream.
            {:else if service.startsWith("youtube")}
              Find it in YouTube Studio, where you go live, under the stream settings.
            {:else}
              Your streaming service shows it with its server URL.
            {/if}
            It is stored in {pc.secrets_backend} and is never shown again.
            {#if twitch}
              Tip: add <code>?bandwidthtest=true</code> to the end of the key to test without going live.
            {/if}
          </p>
        {/if}
        <div class="row">
          <button class="primary" type="submit">Save</button>
          {#if pc.destination_key_set && !passthrough}
            <button type="button" class="danger" class:armed={confirmForget} onclick={forgetKey}>
              {confirmForget ? "Click again to remove" : "Remove saved key"}
            </button>
          {/if}
          {#if confirmForget}
            <span class={streaming ? "error" : "muted small"} role={streaming ? "alert" : "status"}>
              {streaming
                ? "You are streaming: without the key, the destination refuses the stream, which ends your broadcast now."
                : "You will need to enter the key again to stream."}
            </span>
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
              <CopyField
                label="Stream Key"
                value={passthrough && !pc.ingest_key_required ? "(your real stream key)" : pc.urls.obs_key}
              />
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
      <p class="muted small">
        The dock link can change the delay, dump the buffer and end the stream; the overlay link can only show the
        delay. Neither can change your settings or stream key. Keep them private, and never show them on stream.
      </p>
    </section>

    <section class="panel stack" aria-labelledby="buffer-h">
      <h2 id="buffer-h">4. Instant delay</h2>
      <label class="inline">
        <input
          type="checkbox"
          checked={pc.config.delay.keep_buffer}
          onchange={(e) => setKeepBuffer((e.currentTarget as HTMLInputElement).checked)}
        />
        Keep a rolling buffer so delay can be added instantly (Rewind)
      </label>
      <p class="muted small">
        <b>On:</b> stream-delay keeps the last {Math.round(pc.config.delay.max_seconds / 60)} min of your stream in
        memory{#if bufferMb}&nbsp;(about {bufferMb} MB at your current bitrate){/if}, so <b>Rewind</b> adds delay at
        once. Viewers see the last few seconds again.<br />
        <b>Off:</b> only what the current delay needs is kept. Adding delay then always uses <b>Mask</b>: the overlay
        slate covers the stream while the delay builds up, so add the overlay to your scenes. Lowering the delay,
        going live and ending the stream work the same.
      </p>
      {#if bufferMessage}<p class="ok small">{bufferMessage}</p>{/if}
      {#if bufferError}<p class="error small" role="alert">{bufferError}</p>{/if}
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

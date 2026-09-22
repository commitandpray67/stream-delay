<script lang="ts">
  import { getToken, updateConfig } from "../../lib/api";
  import { live } from "../../lib/live.svelte";
  import type { OverlayConfig } from "../../lib/types";

  let form = $state<OverlayConfig | null>(null);
  let preview = $state<"mask" | "badge">("mask");
  let message = $state("");
  let error = $state("");

  $effect(() => {
    const c = live.config?.config.overlay;
    if (c && !form) form = structuredClone($state.snapshot(c)) as OverlayConfig;
  });

  async function save(e: Event) {
    e.preventDefault();
    message = error = "";
    try {
      await updateConfig({ overlay: $state.snapshot(form) });
      message = "Saved. Open overlays update right away.";
    } catch (err) {
      error = (err as Error).message;
    }
  }
</script>

{#if form}
  <div class="grid">
    <form class="panel stack" onsubmit={save}>
      <h2>Overlay</h2>
      <label class="inline"><input type="checkbox" bind:checked={form.badge} /> Show a delay badge while delayed</label>
      <label>
        Badge position
        <select bind:value={form.badge_position}>
          <option value="top-left">Top left</option>
          <option value="top-right">Top right</option>
          <option value="bottom-left">Bottom left</option>
          <option value="bottom-right">Bottom right</option>
        </select>
      </label>
      <label class="inline"><input type="checkbox" bind:checked={form.popup} /> Pop up a notice when the delay changes</label>
      <h3>Mask slate</h3>
      <label>Title <input bind:value={form.mask_title} maxlength="200" /></label>
      <label>Subtitle <input bind:value={form.mask_subtitle} maxlength="400" /></label>
      <label>Image URL (optional) <input bind:value={form.mask_image} placeholder="https://…/logo.png" /></label>
      <div class="colors">
        <label>Accent <input type="color" bind:value={form.accent_color} /></label>
        <label>Background <input type="color" bind:value={form.background_color} /></label>
        <label>Text <input type="color" bind:value={form.text_color} /></label>
      </div>
      <div class="row">
        <button class="primary" type="submit">Save</button>
        {#if message}<span class="ok">{message}</span>{/if}
        {#if error}<span class="error" role="alert">{error}</span>{/if}
      </div>
    </form>
    <section class="panel stack" aria-label="Preview">
      <div class="row">
        <button class:active={preview === "mask"} onclick={() => (preview = "mask")}>Preview slate</button>
        <button class:active={preview === "badge"} onclick={() => (preview = "badge")}>Preview badge</button>
      </div>
      <div class="frame">
        <iframe title="Overlay preview" src="/overlay?preview={preview}&token={encodeURIComponent(getToken())}"></iframe>
      </div>
      <p class="muted small">Saved settings are shown. The preview uses a 16:9 frame over a checkerboard.</p>
    </section>
  </div>
{/if}

<style>
  .grid {
    display: grid;
    grid-template-columns: minmax(0, 1fr) minmax(0, 1.2fr);
    gap: 1rem;
  }
  @media (max-width: 860px) {
    .grid {
      grid-template-columns: 1fr;
    }
  }
  h2,
  h3 {
    margin: 0;
    font-size: 1.05rem;
  }
  h3 {
    font-size: 0.95rem;
  }
  .colors {
    display: flex;
    gap: 1rem;
  }
  .colors input {
    width: 3.5rem;
    height: 2.2rem;
    padding: 0.1rem;
  }
  .frame {
    aspect-ratio: 16 / 9;
    border-radius: var(--radius);
    overflow: hidden;
    border: 1px solid var(--border);
    background: repeating-conic-gradient(#3a3a40 0 25%, #2a2a2f 0 50%) 0 0 / 24px 24px;
  }
  iframe {
    width: 100%;
    height: 100%;
    border: 0;
  }
  button.active {
    border-color: var(--accent);
  }
  .small {
    font-size: 0.85rem;
    margin: 0;
  }
  .ok {
    color: var(--live);
  }
</style>

<script lang="ts">
  // Transparent browser source for OBS: delay badge, change popups and the mask slate.
  // Add it to your scenes at the canvas size (for example 1920x1080).
  import { formatDelay } from "../lib/format";
  import { live } from "../lib/live.svelte";
  import { badgeDelay, DelayAnnouncer } from "../lib/overlay";

  const params = new URLSearchParams(location.search);
  // ?preview=mask or ?preview=badge renders a static preview (settings page).
  const preview = params.get("preview");

  const overlay = $derived(live.config?.config.overlay);
  const masked = $derived(preview === "mask" || (live.state?.delay.mask_visible ?? false));
  const badge = $derived(preview === "badge" ? 30 : preview ? null : badgeDelay(live.state));

  let popup = $state("");
  let timer: ReturnType<typeof setTimeout> | undefined;
  const announcer = new DelayAnnouncer();
  $effect(() => {
    const message = announcer.update(live.state);
    if (!message || !overlay?.popup || preview) return;
    popup = message;
    clearTimeout(timer);
    timer = setTimeout(() => (popup = ""), 3500);
  });
</script>

{#if overlay}
  <div
    class="root"
    style:--accent={overlay.accent_color}
    style:--bg={overlay.background_color}
    style:--fg={overlay.text_color}
  >
    {#if masked}
      <div class="mask">
        {#if overlay.mask_image}<img src={overlay.mask_image} alt="" />{/if}
        <h1>{overlay.mask_title}</h1>
        {#if overlay.mask_subtitle}<p>{overlay.mask_subtitle}</p>{/if}
        <div class="spinner" aria-hidden="true"></div>
      </div>
    {/if}
    {#if overlay.badge && badge !== null && !masked}
      <div class="badge {overlay.badge_position}">
        ⏱ {formatDelay(badge * 1000)} delay
      </div>
    {/if}
    {#if popup && !masked}
      <div class="popup">{popup}</div>
    {/if}
  </div>
{/if}

<style>
  .root {
    position: fixed;
    inset: 0;
    font-family: Inter, system-ui, sans-serif;
    color: var(--fg);
  }
  .mask {
    position: absolute;
    inset: 0;
    background: var(--bg);
    display: grid;
    place-content: center;
    justify-items: center;
    text-align: center;
    gap: 1.2vw;
    animation: fade 0.35s ease-out;
  }
  .mask h1 {
    font-size: 4vw;
    margin: 0;
  }
  .mask p {
    font-size: 2vw;
    margin: 0;
    opacity: 0.8;
  }
  .mask img {
    max-width: 30vw;
    max-height: 30vh;
  }
  .spinner {
    width: 3vw;
    height: 3vw;
    border-radius: 50%;
    border: 0.4vw solid color-mix(in srgb, var(--accent) 30%, transparent);
    border-top-color: var(--accent);
    animation: spin 1s linear infinite;
  }
  .badge {
    position: absolute;
    background: color-mix(in srgb, var(--bg) 85%, transparent);
    border-left: 0.3vw solid var(--accent);
    padding: 0.5vw 1vw;
    font-size: 1.3vw;
    font-weight: 700;
    border-radius: 0.4vw;
  }
  .top-left {
    top: 2vw;
    left: 2vw;
  }
  .top-right {
    top: 2vw;
    right: 2vw;
  }
  .bottom-left {
    bottom: 2vw;
    left: 2vw;
  }
  .bottom-right {
    bottom: 2vw;
    right: 2vw;
  }
  .popup {
    position: absolute;
    left: 50%;
    bottom: 8vh;
    transform: translateX(-50%);
    background: var(--accent);
    color: #fff;
    font-size: 1.8vw;
    font-weight: 700;
    padding: 0.8vw 1.6vw;
    border-radius: 0.6vw;
    animation: pop 0.3s ease-out;
  }
  @keyframes fade {
    from {
      opacity: 0;
    }
  }
  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }
  @keyframes pop {
    from {
      transform: translate(-50%, 1vh);
      opacity: 0;
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .mask,
    .popup,
    .spinner {
      animation: none;
    }
  }
</style>

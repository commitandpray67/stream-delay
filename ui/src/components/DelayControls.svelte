<script lang="ts">
  import { applyPreset, cancel, endStream, goLive, resumeStream, setDelay } from "../lib/api";
  import { formatDelay, formatSecondsLabel } from "../lib/format";
  import { t } from "../lib/i18n";
  import { live } from "../lib/live.svelte";
  import type { DelayMode } from "../lib/types";
  import StatusBadge from "./StatusBadge.svelte";

  let { compact = false }: { compact?: boolean } = $props();

  const snap = $derived(live.state?.delay ?? null);
  const ended = $derived(live.state?.ended ?? false);
  const presets = $derived(live.config?.config.delay.presets ?? []);
  // The dock has no separate "Go live now" button, so its preset row always offers
  // one ("Live"), even if no 0 s preset is configured. Index -1 marks that one.
  const presetButtons = $derived.by(() => {
    const list = presets.map((p, index) => ({ index, seconds: p.seconds }));
    if (compact && !presets.some((p) => p.seconds <= 0)) list.unshift({ index: -1, seconds: 0 });
    return list;
  });
  const maxSeconds = $derived(live.config?.config.delay.max_seconds ?? 120);
  // Without the rolling buffer there is nothing to rewind into, so every increase
  // is covered by the Mask slate.
  const keepBuffer = $derived(live.config?.config.delay.keep_buffer ?? true);
  let mode = $state<DelayMode | null>(null);
  const effectiveMode = $derived<DelayMode>(
    keepBuffer ? (mode ?? live.config?.config.delay.default_mode ?? "rewind") : "mask",
  );
  // Going live only does something while there is a delay to drop.
  const canGoLive = $derived(
    !!snap && snap.phase !== "offline" && (snap.target_ms > 0 || snap.effective_ms >= 500),
  );
  const canEnd = $derived(!!snap && (snap.phase !== "offline" || live.state?.egress.status === "live"));
  // "End stream" needs a second click within a few seconds.
  let armed = $state(false);
  let disarm: ReturnType<typeof setTimeout> | undefined;

  function endClick() {
    if (armed) {
      armed = false;
      clearTimeout(disarm);
      run(endStream);
    } else {
      armed = true;
      disarm = setTimeout(() => (armed = false), 3000);
    }
  }
  let custom = $state("");
  let error = $state("");
  let busy = $state(false);

  const pending = $derived(
    snap ? ["adding", "going-live", "reducing"].includes(snap.phase) : false,
  );

  async function run(action: () => Promise<unknown>) {
    error = "";
    busy = true;
    try {
      const ack = (await action()) as { history_short?: boolean } | undefined;
      if (ack?.history_short) error = "Not enough of the stream is buffered yet; the delay is shorter than asked.";
    } catch (e) {
      error = (e as Error).message;
    } finally {
      busy = false;
    }
  }

  function presetActive(seconds: number): boolean {
    if (!snap) return false;
    return Math.round(snap.target_ms / 1000) === Math.round(seconds);
  }

  function presetClick(index: number, seconds: number) {
    // Presets store their own mode; the toggle overrides it when set explicitly.
    if (seconds <= 0) return run(() => goLive("now"));
    if (!keepBuffer) return run(() => setDelay(seconds, "mask"));
    if (mode) return run(() => setDelay(seconds, mode!));
    return run(() => applyPreset(index));
  }

  function submitCustom(e: Event) {
    e.preventDefault();
    const s = Number(custom);
    if (!Number.isFinite(s) || s < 0) {
      error = "Enter a number of seconds.";
      return;
    }
    run(() => (s === 0 ? goLive("now") : setDelay(s, effectiveMode)));
  }
</script>

<div class="controls" class:compact>
  <StatusBadge {snap} {ended} large={!compact} />

  <div class="presets" role="group" aria-label="Delay presets">
    {#each presetButtons as p (p.index)}
      <button
        class:active={presetActive(p.seconds)}
        aria-pressed={presetActive(p.seconds)}
        disabled={busy}
        onclick={() => presetClick(p.index, p.seconds)}
      >
        {formatSecondsLabel(p.seconds)}
      </button>
    {/each}
  </div>

  {#if keepBuffer}
    <div class="mode" role="radiogroup" aria-label="How to add delay">
      {#each ["rewind", "mask"] as const as m (m)}
        <button
          role="radio"
          aria-checked={effectiveMode === m}
          class:active={effectiveMode === m}
          title={t(`mode.${m}.help`)}
          onclick={() => (mode = m)}
        >
          {t(`mode.${m}`)}
        </button>
      {/each}
    </div>
    {#if !compact}
      <p class="muted small">{t(`mode.${effectiveMode}.help`)}</p>
    {/if}
  {:else}
    <p class="muted small">{t("mode.maskOnly")}</p>
  {/if}

  <form class="custom" onsubmit={submitCustom}>
    <label class="sr-only" for="custom-delay">{t("custom.label")}</label>
    <input
      id="custom-delay"
      type="number"
      min="0"
      max={maxSeconds}
      step="1"
      placeholder={compact ? "sec" : t("custom.label")}
      bind:value={custom}
    />
    <button type="submit" disabled={busy || custom === ""}>{t("action.set")}</button>
  </form>

  {#if ended}
    <div class="golive">
      <button class="primary" disabled={busy} onclick={() => run(resumeStream)}>{t("action.resume")}</button>
    </div>
  {:else}
    <div class="golive">
      {#if !compact}
        <button class="primary" disabled={busy || !canGoLive} onclick={() => run(() => goLive("now"))}>
          {t("action.goLive")}
        </button>
        <button
          disabled={busy || !canGoLive}
          title={t("action.goLiveAfter.help")}
          onclick={() => run(() => goLive("after-air"))}
        >
          {t("action.goLiveAfter")}
        </button>
      {/if}
      <button
        class="danger"
        class:armed
        disabled={busy || !canEnd}
        title={t("action.endStream.help")}
        onclick={endClick}
      >
        {armed ? t("action.endStream.confirm") : t("action.endStream")}
      </button>
      {#if pending}
        <button onclick={() => run(cancel)}>{t("action.cancel")}</button>
      {/if}
    </div>
    {#if !compact}
      <ul class="muted small explain">
        <li><b>{t("action.goLiveAfter")}:</b> {t("action.goLiveAfter.help")}</li>
        <li><b>{t("action.endStream")}:</b> {t("action.endStream.help")}</li>
      </ul>
    {/if}
  {/if}

  {#if snap}
    <div class="buffer" title="How far back the buffer reaches">
      <div class="bar" aria-hidden="true">
        <div class="fill" style:width="{Math.min(100, (snap.history_ms / Math.max(1, snap.max_delay_ms)) * 100)}%"></div>
      </div>
      <span class="muted small">
        {keepBuffer
          ? t("buffer.label", { history: formatDelay(snap.history_ms), max: formatDelay(snap.max_delay_ms) })
          : t("buffer.labelNoHistory", { history: formatDelay(snap.history_ms) })}
      </span>
    </div>
    {#each snap.warnings as w (w)}
      <p class="notice">{w}</p>
    {/each}
  {/if}

  {#if error}<p class="error" role="alert">{error}</p>{/if}
</div>

<style>
  .controls {
    display: grid;
    gap: 0.75rem;
  }
  .presets {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(3.2rem, 1fr));
    gap: 0.4rem;
  }
  .presets button {
    font-weight: 600;
    padding: 0.6rem 0.4rem;
  }
  button.active {
    border-color: var(--accent);
    background: color-mix(in srgb, var(--accent) 22%, var(--panel-2));
  }
  .mode {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 0.4rem;
  }
  .custom {
    display: grid;
    grid-template-columns: 1fr auto;
    gap: 0.4rem;
  }
  .golive {
    display: grid;
    gap: 0.4rem;
  }
  /* One row of equal buttons: End stream, plus Cancel while a change is pending. */
  .compact .golive {
    grid-auto-flow: column;
    grid-auto-columns: 1fr;
  }
  button.danger.armed {
    background: var(--danger);
    color: #fff;
    font-weight: 700;
  }
  .buffer {
    display: grid;
    gap: 0.3rem;
  }
  .bar {
    height: 6px;
    border-radius: 3px;
    background: var(--panel-2);
    overflow: hidden;
  }
  .fill {
    height: 100%;
    background: var(--accent);
    transition: width 0.25s;
  }
  .small {
    font-size: 0.85rem;
    margin: 0;
  }
  p {
    margin: 0;
  }
  .explain {
    margin: 0;
    padding-left: 1.1rem;
    display: grid;
    gap: 0.2rem;
  }
</style>

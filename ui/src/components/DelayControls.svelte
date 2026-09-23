<script lang="ts">
  import { applyPreset, cancel, dumpBuffer, endStream, goLive, resumeStream, setDelay } from "../lib/api";
  import { formatDelay, formatSecondsLabel } from "../lib/format";
  import { t, type Key } from "../lib/i18n";
  import { live } from "../lib/live.svelte";
  import type { DelayMode } from "../lib/types";
  import StatusBadge from "./StatusBadge.svelte";

  let { compact = false }: { compact?: boolean } = $props();

  const snap = $derived(live.state?.delay ?? null);
  const ended = $derived(live.state?.ended ?? false);
  const ending = $derived(live.state?.ending ?? false);
  // A broadcast is running, or about to: OBS is sending, or the destination is
  // connected (still airing the end of a stream). Starting one is up to OBS.
  const streaming = $derived.by(() => {
    const s = live.state;
    if (!s || s.ended) return false;
    return s.ingest.connected || ["connecting", "live", "retrying"].includes(s.egress.status);
  });
  const presets = $derived(live.config?.config.delay.presets ?? []);
  // The dock always offers "0 s", even if no 0 s preset is configured. Index -1
  // marks that one.
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
  const canRemoveDelay = $derived(
    !!snap && snap.phase !== "offline" && (snap.target_ms > 0 || snap.effective_ms >= 500),
  );
  // What a dump throws away and builds back: the delay asked for, or while it is
  // being removed, the one still in effect.
  const dumpDelayMs = $derived(snap ? (snap.target_ms > 0 ? snap.target_ms : snap.effective_ms) : 0);
  const canDump = $derived(streaming && !ending && dumpDelayMs >= 500);
  // A dump replays the stretch before what it throws away, if the buffer reaches
  // back that far; otherwise the slate covers the stream while the delay rebuilds.
  const dumpReplays = $derived(
    !!snap && keepBuffer && effectiveMode === "rewind" && snap.history_ms >= snap.effective_ms + dumpDelayMs + 2000,
  );
  const noOverlay = $derived(live.overlays === 0);
  const unaired = $derived(formatDelay(snap?.effective_ms ?? 0));

  let custom = $state("");
  let error = $state("");
  let busy = $state(false);

  const pending = $derived(snap ? ["adding", "going-live", "reducing"].includes(snap.phase) : false);

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

  // ----- stream actions: a second click confirms ------------------------------

  type Action = "dump" | "end" | "end-now";
  const perform: Record<Action, () => Promise<unknown>> = {
    dump: () => dumpBuffer(effectiveMode),
    end: () => endStream("after-air"),
    "end-now": () => endStream("now"),
  };
  // Actions whose first use explains itself first.
  const explained: Partial<Record<Action, true>> = { dump: true, "end-now": true };

  let armed = $state<Action | null>(null);
  let disarm: ReturnType<typeof setTimeout> | undefined;
  let explaining = $state<Action | null>(null);
  let dontShow = $state(false);

  const ackKey = (a: Action) => `stream-delay-understood:${a}`;
  function understood(a: Action): boolean {
    try {
      return localStorage.getItem(ackKey(a)) === "1";
    } catch {
      // No storage (some OBS setups): explain every time.
      return false;
    }
  }

  function click(a: Action) {
    if (armed === a || explaining === a) {
      if (explaining === a && dontShow) {
        try {
          localStorage.setItem(ackKey(a), "1");
        } catch {
          // Explained again next time.
        }
      }
      armed = explaining = null;
      clearTimeout(disarm);
      run(perform[a]);
      return;
    }
    clearTimeout(disarm);
    armed = a;
    explaining = null;
    dontShow = false;
    if (explained[a] && !understood(a)) {
      // Stays armed while the explanation is open.
      explaining = a;
    } else {
      disarm = setTimeout(() => (armed = null), 3000);
    }
  }

  function closeExplanation() {
    armed = explaining = null;
  }

  function label(a: Action, base: Key, confirm: Key): string {
    return armed === a ? t(confirm) : t(base);
  }

  // ----- delay -----------------------------------------------------------------

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
  <StatusBadge {snap} {ended} {ending} large={!compact} />

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

  {#if pending || (!compact && !ended)}
    <div class="row-buttons">
      {#if !compact && !ended}
        <button
          disabled={busy || !canRemoveDelay}
          title={t("action.removeDelayAfter.help")}
          onclick={() => run(() => goLive("after-air"))}
        >
          {t("action.removeDelayAfter")}
        </button>
      {/if}
      {#if pending}
        <button onclick={() => run(cancel)}>{t("action.cancel")}</button>
      {/if}
    </div>
  {/if}

  {#if ended}
    {#if compact}
      <p class="notice">{t("hint.ended")}</p>
    {:else}
      <div class="row-buttons">
        <button class="primary" disabled={busy} title={t("action.resume.help")} onclick={() => run(resumeStream)}>
          {t("action.resume")}
        </button>
      </div>
      <p class="muted small">{t("action.resume.help")}</p>
    {/if}
  {:else if ending}
    {#if compact}<p class="muted small">{t("hint.ending")}</p>{/if}
    <div class="row-buttons">
      <button class="primary" disabled={busy} onclick={() => run(resumeStream)}>{t("action.keepStreaming")}</button>
      <button class="danger" class:armed={armed === "end-now"} disabled={busy} onclick={() => click("end-now")}>
        {label("end-now", "action.endNow", "action.endNow.confirm")}
      </button>
    </div>
  {:else if streaming}
    <div class="stream-actions" role="group" aria-label="Stream">
      <button
        class="warn wide"
        class:armed={armed === "dump"}
        disabled={busy || !canDump}
        title={canDump ? t("action.dump.help") : t("action.dump.noDelay")}
        onclick={() => click("dump")}
      >
        {label("dump", "action.dump", "action.dump.confirm")}
      </button>
      <button
        class="danger"
        class:armed={armed === "end"}
        disabled={busy}
        title={t("action.endStream.help")}
        onclick={() => click("end")}
      >
        {label("end", "action.endStream", "action.endStream.confirm")}
      </button>
      <button
        class="danger"
        class:armed={armed === "end-now"}
        disabled={busy}
        title={t("action.endNow.help")}
        onclick={() => click("end-now")}
      >
        {label("end-now", "action.endNow", "action.endNow.confirm")}
      </button>
    </div>
  {/if}

  {#if explaining}
    <div class="explain-popup" role="alertdialog" aria-labelledby="explain-title">
      {#if explaining === "end-now"}
        <p id="explain-title">
          {t("action.endNow.warning", { delay: unaired })}
        </p>
      {:else}
        <p id="explain-title"><b>{t("action.dump")}:</b> {t("action.dump.help")}</p>
        <p>
          {#if dumpReplays}
            {t("action.dump.replay", { delay: formatDelay(dumpDelayMs) })}
          {:else if noOverlay}
            <span class="error">{t("action.dump.noOverlay", { delay: formatDelay(dumpDelayMs) })}</span>
          {:else}
            {t("action.dump.slate", { delay: formatDelay(dumpDelayMs) })}
          {/if}
        </p>
      {/if}
      <label class="inline small"><input type="checkbox" bind:checked={dontShow} /> {t("popup.dontShow")}</label>
      <div class="row-buttons">
        <button class={explaining === "dump" ? "warn armed" : "danger armed"} onclick={() => click(explaining!)}>
          {explaining === "dump" ? t("action.dump") : t("action.endNow")}
        </button>
        <button onclick={closeExplanation}>{t("action.cancel")}</button>
      </div>
    </div>
  {/if}

  {#if !compact && !ended}
    <ul class="muted small explain">
      <li><b>{t("action.removeDelayAfter")}:</b> {t("action.removeDelayAfter.help")}</li>
      <li><b>{t("action.dump")}:</b> {t("action.dump.help")}</li>
      <li><b>{t("action.endStream")}:</b> {t("action.endStream.help")}</li>
      <li><b>{t("action.endNow")}:</b> {t("action.endNow.help")}</li>
    </ul>
  {/if}

  {#if snap}
    <div class="buffer" title="How far back the buffer reaches">
      <div class="bar" aria-hidden="true">
        <div class="fill" style:width="{Math.min(100, (snap.history_ms / Math.max(1, snap.max_delay_ms)) * 100)}%"></div>
      </div>
      <span class="muted small">
        {keepBuffer
          ? t("buffer.label", {
              history: formatDelay(Math.min(snap.history_ms, snap.max_delay_ms)),
              max: formatDelay(snap.max_delay_ms),
            })
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
  .row-buttons {
    display: grid;
    grid-auto-flow: column;
    grid-auto-columns: 1fr;
    gap: 0.4rem;
  }
  /* Dump (the stream goes on) on its own row, above the two ways to end it. */
  .stream-actions {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 0.4rem;
  }
  .stream-actions .wide {
    grid-column: 1 / -1;
  }
  button.warn {
    border-color: var(--busy);
    color: var(--busy);
  }
  button.danger.armed {
    background: var(--danger);
    color: #fff;
    font-weight: 700;
  }
  button.warn.armed {
    background: var(--busy);
    color: #fff;
    font-weight: 700;
  }
  .explain-popup {
    display: grid;
    gap: 0.5rem;
    padding: 0.7rem;
    border: 1px solid var(--danger);
    border-radius: var(--radius);
    background: var(--panel-2);
  }
  .explain-popup p {
    font-size: 0.9rem;
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

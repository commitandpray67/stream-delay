<script lang="ts">
  import { formatDelay, phaseTone } from "../lib/format";
  import { t } from "../lib/i18n";
  import type { Snapshot } from "../lib/types";

  let { snap, large = false }: { snap: Snapshot | null; large?: boolean } = $props();

  const phase = $derived(snap?.phase ?? "offline");
  const delay = $derived(formatDelay(snap?.effective_ms ?? 0));
  const label = $derived(t(`phase.${phase}`, { delay }));
  const hint = $derived(t(`hint.${phase}`, { delay }));
</script>

<div class="badge {phaseTone(phase)}" class:large role="status" aria-live="polite">
  <span class="dot" aria-hidden="true"></span>
  <div>
    <div class="label">{label}</div>
    {#if large}<div class="hint">{hint}</div>{/if}
  </div>
</div>

<style>
  .badge {
    display: flex;
    align-items: center;
    gap: 0.6rem;
    border-radius: var(--radius);
    padding: 0.55rem 0.8rem;
    border: 1px solid var(--tone);
    background: color-mix(in srgb, var(--tone) 14%, transparent);
    --tone: var(--muted);
  }
  .live {
    --tone: var(--live);
  }
  .delayed {
    --tone: var(--delayed);
  }
  .busy {
    --tone: var(--busy);
  }
  .dot {
    width: 0.7rem;
    height: 0.7rem;
    border-radius: 50%;
    background: var(--tone);
    flex: none;
  }
  .busy .dot {
    animation: pulse 1s ease-in-out infinite;
  }
  .label {
    font-weight: 700;
    letter-spacing: 0.01em;
  }
  .large .label {
    font-size: 1.5rem;
  }
  .hint {
    color: var(--muted);
    font-size: 0.9rem;
  }
  @keyframes pulse {
    50% {
      opacity: 0.3;
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .busy .dot {
      animation: none;
    }
  }
</style>

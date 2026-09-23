<script lang="ts">
  import StatusBadge from "../components/StatusBadge.svelte";
  import { t } from "../lib/i18n";
  import { adminConfig, live } from "../lib/live.svelte";
  import AdvancedTab from "./dashboard/AdvancedTab.svelte";
  import ControlTab from "./dashboard/ControlTab.svelte";
  import DelayTab from "./dashboard/DelayTab.svelte";
  import OverlayTab from "./dashboard/OverlayTab.svelte";
  import SetupTab from "./dashboard/SetupTab.svelte";

  const tabs = [
    { id: "control", label: "Control" },
    { id: "setup", label: "Setup" },
    { id: "delay", label: "Delay settings" },
    { id: "overlay", label: "Overlay" },
    { id: "advanced", label: "Advanced" },
  ] as const;
  type TabId = (typeof tabs)[number]["id"];

  const initial = (location.hash.slice(1) || (location.pathname === "/setup" ? "setup" : "control")) as TabId;
  let tab = $state<TabId>(tabs.some((x) => x.id === initial) ? initial : "control");
  $effect(() => {
    history.replaceState(null, "", `#${tab}`);
  });

  const admin = $derived(adminConfig());
  // A dock or overlay link opened as the dashboard: its token cannot change settings.
  const limited = $derived(!!live.config && !admin);
  // First run: nudge towards setup when nothing is configured yet.
  const needsSetup = $derived(
    !!admin && !admin.destination_key_set && admin.config.destination.key_mode === "stored",
  );
</script>

<div class="app">
  <header>
    <div class="brand">
      <img src="/favicon.svg" alt="" width="28" height="28" />
      <h1>stream-delay</h1>
    </div>
    <StatusBadge snap={live.state?.delay ?? null} ended={live.state?.ended ?? false} />
  </header>

  <nav aria-label="Sections">
    {#each tabs as x (x.id)}
      <button class:current={tab === x.id} aria-current={tab === x.id ? "page" : undefined} onclick={() => (tab = x.id)}>
        {x.label}
      </button>
    {/each}
  </nav>

  <main>
    {#if live.unauthorized}
      <p class="notice">{t("conn.unauthorized")}</p>
    {:else if !live.connected}
      <p class="notice">{t("conn.lost")}</p>
    {/if}
    {#if limited}
      <p class="notice">
        This link can't open the dashboard. Use the dashboard link from stream-delay (the tray menu, or
        <code>streamdelayd urls</code>).
      </p>
    {/if}
    {#if admin?.restart_required}
      <p class="notice">Some changes take effect after you restart stream-delay.</p>
    {/if}
    {#if needsSetup && tab === "control"}
      <p class="notice">
        No stream key yet. <button class="link" onclick={() => (tab = "setup")}>Open Setup</button> to connect your
        Twitch account's stream key and OBS.
      </p>
    {/if}

    {#if limited}
      <!-- Nothing else works with a dock or overlay token. -->
    {:else if tab === "control"}<ControlTab />
    {:else if tab === "setup"}<SetupTab />
    {:else if tab === "delay"}<DelayTab />
    {:else if tab === "overlay"}<OverlayTab />
    {:else}<AdvancedTab />{/if}
  </main>
</div>

<style>
  .app {
    max-width: 1100px;
    margin: 0 auto;
    padding: 1rem;
    display: grid;
    gap: 1rem;
  }
  header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 1rem;
    flex-wrap: wrap;
  }
  .brand {
    display: flex;
    align-items: center;
    gap: 0.6rem;
  }
  h1 {
    font-size: 1.3rem;
    margin: 0;
  }
  nav {
    display: flex;
    gap: 0.3rem;
    flex-wrap: wrap;
    border-bottom: 1px solid var(--border);
    padding-bottom: 0.5rem;
  }
  nav button {
    border: none;
    background: none;
    color: var(--muted);
  }
  nav button.current {
    color: var(--text);
    background: var(--panel-2);
    font-weight: 600;
  }
  main {
    display: grid;
    gap: 1rem;
  }
  .notice {
    margin: 0;
  }
  button.link {
    border: none;
    background: none;
    padding: 0;
    color: var(--accent);
    text-decoration: underline;
  }
</style>

<script lang="ts">
  let { label, value, secret = false }: { label: string; value: string; secret?: boolean } = $props();
  let copied = $state(false);
  let input: HTMLInputElement;

  async function copy() {
    try {
      await navigator.clipboard.writeText(value);
    } catch {
      input.select();
      document.execCommand("copy");
    }
    copied = true;
    setTimeout(() => (copied = false), 1500);
  }
</script>

<label>
  {label}
  <span class="field">
    <input bind:this={input} readonly {value} type={secret ? "password" : "text"} onfocus={() => input.select()} />
    <button type="button" onclick={copy} aria-label="Copy {label}">{copied ? "Copied" : "Copy"}</button>
  </span>
</label>

<style>
  .field {
    display: grid;
    grid-template-columns: 1fr auto;
    gap: 0.4rem;
  }
  input {
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    font-size: 0.85rem;
  }
</style>

<script lang="ts">
  let { label, value, secret = false }: { label: string; value: string; secret?: boolean } = $props();
  let copied = $state<boolean | null>(null);
  let input: HTMLInputElement;

  /**
   * Without the clipboard API (a page over plain HTTP from another device), copies
   * from a text field made for it: browsers copy nothing from a password field.
   */
  function copyFromText(text: string): boolean {
    const field = document.createElement("textarea");
    field.value = text;
    field.readOnly = true;
    field.style.position = "fixed";
    field.style.opacity = "0";
    document.body.append(field);
    field.focus();
    field.select();
    let ok = false;
    try {
      ok = document.execCommand("copy");
    } catch {
      ok = false;
    }
    field.remove();
    return ok;
  }

  async function copy() {
    try {
      await navigator.clipboard.writeText(value);
      copied = true;
    } catch {
      copied = copyFromText(value);
    }
    setTimeout(() => (copied = null), 1500);
  }
</script>

<label>
  {label}
  <span class="field">
    <input bind:this={input} readonly {value} type={secret ? "password" : "text"} onfocus={() => input.select()} />
    <button type="button" onclick={copy} aria-label="Copy {label}">
      {copied === null ? "Copy" : copied ? "Copied" : "Not copied"}
    </button>
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

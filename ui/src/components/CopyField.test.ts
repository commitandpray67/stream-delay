// @vitest-environment jsdom
import { fireEvent, render, screen } from "@testing-library/svelte";
import { afterEach, describe, expect, it, vi } from "vitest";
import CopyField from "./CopyField.svelte";

/** No clipboard API, as on a page over plain HTTP from another device. */
function withoutClipboardApi(copies: (selected: string, from: Element | null) => boolean) {
  Object.defineProperty(navigator, "clipboard", { value: undefined, configurable: true });
  document.execCommand = vi.fn(() => copies(String(document.getSelection() ?? ""), document.activeElement));
}

afterEach(() => {
  vi.restoreAllMocks();
});

describe("copy field", () => {
  it("copies a hidden value from text the browser can copy, not from the password field", async () => {
    let copied: string | null = null;
    withoutClipboardApi((_, from) => {
      // Browsers copy nothing from a password field.
      if (from instanceof HTMLInputElement && from.type === "password") return false;
      const field = from as HTMLInputElement | HTMLTextAreaElement | null;
      copied = field ? field.value.slice(field.selectionStart ?? 0, field.selectionEnd ?? 0) : null;
      return copied !== null;
    });
    render(CopyField, { label: "Dashboard link", value: "http://h/?token=secret", secret: true });
    await fireEvent.click(screen.getByRole("button", { name: "Copy Dashboard link" }));
    expect(copied).toBe("http://h/?token=secret");
    expect(screen.getByRole("button").textContent).toBe("Copied");
  });

  it("does not say it copied what it could not", async () => {
    withoutClipboardApi(() => false);
    render(CopyField, { label: "Dock URL", value: "http://h/dock?token=t" });
    await fireEvent.click(screen.getByRole("button", { name: "Copy Dock URL" }));
    expect(screen.getByRole("button").textContent).not.toBe("Copied");
  });
});

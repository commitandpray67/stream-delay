// @vitest-environment jsdom
import { fireEvent, render, screen } from "@testing-library/svelte";
import { afterEach, describe, expect, it, vi } from "vitest";
import * as api from "../../lib/api";
import { live } from "../../lib/live.svelte";
import type { OverlayConfig, PublicConfig } from "../../lib/types";
import OverlayTab from "./OverlayTab.svelte";

vi.mock("../../lib/api", () => ({ updateConfig: vi.fn() }));

/** The dashboard's settings, with these overlay colours (as `config.toml` may hold them). */
function config(colors: Partial<OverlayConfig>): PublicConfig {
  return {
    scope: "admin",
    urls: { overlay: "http://127.0.0.1:7788/overlay?token=r" },
    config: {
      overlay: {
        badge: true,
        badge_position: "top-right",
        popup: true,
        mask_title: "Be right back",
        mask_subtitle: "",
        accent_color: "#9147ff",
        background_color: "#0e0e10",
        text_color: "#efeff1",
        mask_image: "",
        ...colors,
      },
    },
  } as unknown as PublicConfig;
}

const field = (name: string) => screen.getByLabelText(name) as HTMLInputElement;

afterEach(() => {
  vi.clearAllMocks();
});

describe("colours", () => {
  it("shows every colour the settings take, and a new one keeps the transparency", async () => {
    live.config = config({ background_color: "#0e0e10cc", text_color: "#fff" });
    render(OverlayTab);
    // A colour field holds #rrggbb only: others showed as black.
    expect(field("Text").value).toBe("#ffffff");
    expect(field("Background").value).toBe("#0e0e10");
    await fireEvent.input(field("Background"), { target: { value: "#223344" } });
    await fireEvent.click(screen.getByRole("button", { name: "Save" }));
    const saved = vi.mocked(api.updateConfig).mock.calls[0][0] as { overlay: OverlayConfig };
    // The field has no alpha: the one set is kept. The others are saved as set.
    expect(saved.overlay.background_color).toBe("#223344cc");
    expect(saved.overlay.text_color).toBe("#fff");
    expect(saved.overlay.accent_color).toBe("#9147ff");
  });
});

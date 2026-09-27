import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";
import { svelteTesting } from "@testing-library/svelte/vite";

// During `pnpm dev`, API calls go to a running streamdelayd.
const api = process.env.STREAMDELAY_API ?? "http://127.0.0.1:7788";

export default defineConfig({
  // svelteTesting: component tests get Svelte's browser build, and a clean page.
  plugins: [svelte(), svelteTesting()],
  build: { outDir: "dist", emptyOutDir: true, target: "es2020" },
  server: {
    proxy: {
      "/api": { target: api, ws: true, changeOrigin: true },
    },
  },
  // Component tests ask for a simulated page (`@vitest-environment jsdom`).
  test: { environment: "node", include: ["src/**/*.test.ts"] },
});

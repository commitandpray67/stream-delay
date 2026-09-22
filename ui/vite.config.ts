import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";

// During `pnpm dev`, API calls go to a running streamdelayd.
const api = process.env.STREAMDELAY_API ?? "http://127.0.0.1:7788";

export default defineConfig({
  plugins: [svelte()],
  build: { outDir: "dist", emptyOutDir: true, target: "es2020" },
  server: {
    proxy: {
      "/api": { target: api, ws: true, changeOrigin: true },
    },
  },
  test: { environment: "node", include: ["src/**/*.test.ts"] },
});

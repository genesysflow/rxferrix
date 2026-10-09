import { svelte } from "@sveltejs/vite-plugin-svelte";
import { defineConfig } from "vite";

// The dashboard is served by the ferrix binary at /_/ from crates/ferrix/dashboard-dist (embedded at compile time).
// `bun run dev` serves it on :5174 and proxies /api (including the /api/sync WebSocket) to a running server.
const FERRIX = process.env.FERRIX_URL ?? "http://127.0.0.1:8090";
const src = (path: string) => decodeURIComponent(new URL(path, import.meta.url).pathname);

export default defineConfig({
  base: "/_/",
  plugins: [svelte()],
  resolve: {
    // Bundle the workspace packages from their sources: no build order between packages, and the stale-dist check
    // (scripts/source-hash.ts) covers exactly what goes into the bundle.
    alias: [
      { find: /^@ferrix\/client$/, replacement: src("../client/src/index.ts") },
      { find: /^@ferrix\/svelte$/, replacement: src("../svelte/src/lib/index.ts") },
    ],
  },
  build: {
    outDir: src("../../crates/ferrix/dashboard-dist"),
    emptyOutDir: true,
    target: "es2022",
    sourcemap: false,
    chunkSizeWarningLimit: 1000,
  },
  server: {
    port: 5174,
    strictPort: true,
    proxy: { "/api": { target: FERRIX, ws: true, changeOrigin: true } },
  },
});

import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";

// The RxFerrix server (cargo run -p todo-server -- serve --dev) listens on 8090. Proxying /api through Vite keeps the
// app same-origin, so the client can simply connect to window.location.origin.
const RXFERRIX = process.env.RXFERRIX_URL ?? "http://127.0.0.1:8090";

export default defineConfig({
  plugins: [svelte()],
  server: {
    port: Number(process.env.VITE_PORT ?? 5173),
    proxy: { "/api": { target: RXFERRIX, ws: true, changeOrigin: true } },
  },
});

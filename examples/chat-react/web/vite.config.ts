import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// The Ferrix server (cargo run -p chat-server -- serve --dev --http 127.0.0.1:8097) listens on 8097. Proxying /api
// through Vite keeps the app same-origin, so the client simply connects to window.location.origin.
const FERRIX = process.env.FERRIX_URL ?? "http://127.0.0.1:8097";

export default defineConfig({
  plugins: [react()],
  server: {
    port: Number(process.env.VITE_PORT ?? 5180),
    strictPort: true,
    proxy: { "/api": { target: FERRIX, ws: true, changeOrigin: true } },
  },
});

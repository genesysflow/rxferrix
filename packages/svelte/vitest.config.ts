import { svelte } from "@sveltejs/vite-plugin-svelte";
import { defineConfig } from "vitest/config";

const clientSrc = (file: string) => decodeURIComponent(new URL(`../client/src/${file}`, import.meta.url).pathname);

export default defineConfig({
  plugins: [svelte()],
  resolve: {
    // Use Svelte's client (browser) build so components can be mounted in jsdom.
    conditions: ["browser"],
    // Test against the client's sources so no build step is needed.
    alias: [
      { find: /^@rxferrix\/client$/, replacement: clientSrc("index.ts") },
      { find: /^@rxferrix\/client\/testing$/, replacement: clientSrc("testing.ts") },
    ],
  },
  test: {
    include: ["test/**/*.test.ts"],
    environment: "jsdom",
  },
});

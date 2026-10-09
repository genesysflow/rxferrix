import { svelte } from "@sveltejs/vite-plugin-svelte";
import { defineConfig } from "vitest/config";

const src = (path: string) => decodeURIComponent(new URL(path, import.meta.url).pathname);

export default defineConfig({
  plugins: [svelte()],
  resolve: {
    conditions: ["browser"],
    alias: [
      { find: /^@ferrix\/client$/, replacement: src("../client/src/index.ts") },
      { find: /^@ferrix\/client\/testing$/, replacement: src("../client/src/testing.ts") },
      { find: /^@ferrix\/svelte$/, replacement: src("../svelte/src/lib/index.ts") },
    ],
  },
  test: {
    include: ["test/**/*.test.ts"],
    environment: "jsdom",
  },
});

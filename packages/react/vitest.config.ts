import { defineConfig } from "vitest/config";

const clientSrc = (file: string) => decodeURIComponent(new URL(`../client/src/${file}`, import.meta.url).pathname);

export default defineConfig({
  resolve: {
    // Test against the client's sources so no build step is needed.
    alias: [
      { find: /^@rxferrix\/client$/, replacement: clientSrc("index.ts") },
      { find: /^@rxferrix\/client\/testing$/, replacement: clientSrc("testing.ts") },
    ],
  },
  test: {
    include: ["test/**/*.test.{ts,tsx}"],
    environment: "jsdom",
  },
});

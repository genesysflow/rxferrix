// The hash of the sources the committed dashboard bundle is built from.
//
//   bun scripts/source-hash.ts --write   after `vite build`: stamp crates/rxferrix/dashboard-dist/source-hash.txt
//   bun scripts/source-hash.ts --check   exit 1 if the committed bundle was built from other sources
//
// The Rust test `dashboard::assets::tests::committed_dist_is_fresh` computes the same hash (keep the two in sync):
// SHA-256 over every input file in path order, each as `<path relative to the repo root>\0<content, CRLF → LF>\0`.
// Inputs: this package's index.html, package.json, vite.config.ts, tsconfig.json, svelte.config.js and src/**, plus
// the sources it bundles: packages/client/src/** and packages/svelte/src/lib/**.
import { createHash } from "node:crypto";
import { existsSync, readdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { join, relative, sep } from "node:path";

const here = decodeURIComponent(new URL("..", import.meta.url).pathname);
const repo = join(here, "..", "..");
const stampPath = join(repo, "crates", "rxferrix", "dashboard-dist", "source-hash.txt");

function walk(dir: string, out: string[]): void {
  for (const name of readdirSync(dir)) {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) walk(path, out);
    else out.push(path);
  }
}

export function sourceHash(): string {
  const files = ["index.html", "package.json", "vite.config.ts", "tsconfig.json", "svelte.config.js"]
    .map((f) => join(here, f))
    .filter((p) => existsSync(p));
  for (const dir of [join(here, "src"), join(repo, "packages/client/src"), join(repo, "packages/svelte/src/lib")]) {
    walk(dir, files);
  }
  const entries = files.map((p) => [relative(repo, p).split(sep).join("/"), p] as const);
  // Byte order of the (ASCII) paths, like Rust's String ordering.
  entries.sort((a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0));
  const h = createHash("sha256");
  for (const [name, path] of entries) {
    h.update(name);
    h.update(Buffer.from([0]));
    h.update(readFileSync(path, "utf8").replace(/\r\n/g, "\n"));
    h.update(Buffer.from([0]));
  }
  return h.digest("hex");
}

if (import.meta.main) {
  const mode = process.argv[2];
  const hash = sourceHash();
  if (mode === "--write") {
    writeFileSync(stampPath, `${hash}\n`);
    console.log(`dashboard sources ${hash.slice(0, 12)}… stamped into ${relative(repo, stampPath)}`);
  } else if (mode === "--check") {
    const stamped = existsSync(stampPath) ? readFileSync(stampPath, "utf8").trim() : "(missing)";
    if (stamped !== hash) {
      console.error(`crates/rxferrix/dashboard-dist is stale (built from ${stamped.slice(0, 12)}, sources are ${hash.slice(0, 12)}).`);
      console.error("Run `bun run build:dashboard` at the repository root and commit the result.");
      process.exit(1);
    }
    console.log("dashboard-dist is up to date");
  } else {
    console.log(hash);
  }
}

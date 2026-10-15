// The version of RxFerrix: the crates and the npm packages share one, which a release tag (`v0.2.0`) publishes.
//
//   bun scripts/version.ts          # prints it; fails if any of the places below disagree
//   bun scripts/version.ts 0.2.0    # sets it everywhere, updates Cargo.lock and bun.lock, rebuilds the dashboard
//
// The places: the workspace's `version` (Cargo.toml, every crate), the `rxferrix-macros` requirement of `rxferrix`,
// the package.json of each package in packages/, and the client's `CLIENT_VERSION` (sent in `Connect`). The committed
// dashboard build (crates/rxferrix/dashboard-dist) bundles the client, so it is rebuilt.

import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const ROOT = join(import.meta.dir, "..");
const PACKAGES = ["client", "react", "svelte", "dashboard"];

type Place = { file: string; pattern: RegExp };

// Each pattern captures the text before the version, the version, and the text after it.
const places: Place[] = [
  { file: "Cargo.toml", pattern: /(\[workspace\.package\][^[]*?\nversion = ")([^"]+)(")/ },
  { file: "crates/rxferrix/Cargo.toml", pattern: /(\nrxferrix-macros = \{[^}\n]*version = ")([^"]+)(")/ },
  ...PACKAGES.map((p) => ({ file: `packages/${p}/package.json`, pattern: /(\n {2}"version": ")([^"]+)(")/ })),
  { file: "packages/client/src/protocol.ts", pattern: /(\nexport const CLIENT_VERSION = ")([^"]+)(")/ },
];

function read(place: Place): string {
  const m = readFileSync(join(ROOT, place.file), "utf8").match(place.pattern);
  if (!m) throw new Error(`${place.file}: no version found`);
  return m[2];
}

function check(): string {
  const found = places.map((p) => ({ file: p.file, version: read(p) }));
  const versions = new Set(found.map((f) => f.version));
  if (versions.size !== 1) {
    for (const f of found) console.error(`${f.version}\t${f.file}`);
    throw new Error("the versions disagree: set them with `bun scripts/version.ts <version>`");
  }
  return found[0].version;
}

function run(cmd: string[]) {
  console.error(`==> ${cmd.join(" ")}`);
  const r = Bun.spawnSync(cmd, { cwd: ROOT, stdout: "inherit", stderr: "inherit" });
  if (r.exitCode !== 0) throw new Error(`${cmd.join(" ")} failed`);
}

const next = process.argv[2];
try {
  if (next !== undefined) {
    if (!/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(next)) throw new Error(`not a version: ${next}`);
    for (const place of places) {
      const path = join(ROOT, place.file);
      const text = readFileSync(path, "utf8");
      writeFileSync(path, text.replace(place.pattern, (_, before, _old, after) => `${before}${next}${after}`));
    }
    run(["cargo", "update", "--workspace"]);
    run(["bun", "install"]);
    run(["bun", "run", "build"]);
    run(["bun", "run", "build:dashboard"]);
  }
  console.log(check());
} catch (e) {
  console.error(`error: ${(e as Error).message}`);
  process.exit(1);
}

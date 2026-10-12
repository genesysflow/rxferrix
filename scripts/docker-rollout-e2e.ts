// End-to-end test of a zero-downtime rollout with Docker Compose and Traefik (deploy/docker), under write load:
//
//   docker build -t todo-server . && bun scripts/docker-rollout-e2e.ts
//
// Needs a Docker daemon you can use (and the traefik image, pulled on first use). Runs its own Compose
// project (`ferrix-e2e`, Traefik on port 18080) and removes it, volume included, at the end. `COMPOSE_EXTRA=file.yaml`
// adds a Compose file (e.g. Traefik with `--log.level=DEBUG`); on failure the logs go to /tmp/docker-rollout-e2e.log.
//
// 1. `deploy.sh` with the same image: a second container takes over, the first is removed. Zero failed requests
//    through Traefik, every acknowledged write present afterwards.
// 2. `deploy.sh` with a version that fails to start (`todo-server:e2e-broken`, built here: an invalid setting makes it
//    exit at once): the script fails, removes it, and the running container carries on, with zero failed requests.

import { join } from "node:path";

const DIR = join(import.meta.dir, "../deploy/docker");
const env = { ...process.env, COMPOSE_PROJECT_NAME: "ferrix-e2e", HTTP_PORT: "18080" };
const BASE = "http://127.0.0.1:18080";
const failures: string[] = [];

async function run(cmd: string[], extraEnv: Record<string, string> = {}): Promise<{ code: number; out: string }> {
  const p = Bun.spawn(cmd, { cwd: DIR, env: { ...env, ...extraEnv }, stdout: "pipe", stderr: "pipe" });
  const [out, err] = await Promise.all([new Response(p.stdout).text(), new Response(p.stderr).text()]);
  return { code: await p.exited, out: out + err };
}

const files = ["-f", "compose.yaml", ...(process.env.COMPOSE_EXTRA ? ["-f", process.env.COMPOSE_EXTRA] : [])];
const compose = (...args: string[]) => run(["docker", "compose", ...files, ...args]);
const appContainers = async () => (await compose("ps", "-q", "app")).out.split("\n").filter(Boolean);

async function waitHealthy(ms = 60_000) {
  const deadline = Date.now() + ms;
  while (Date.now() < deadline) {
    try {
      if ((await fetch(`${BASE}/api/health`)).ok) return;
    } catch {}
    await Bun.sleep(200);
  }
  throw new Error("the app did not become healthy behind Traefik");
}

/** Adds a todo: `true`, or what went wrong. */
async function add(text: string): Promise<true | string> {
  const r = await fetch(`${BASE}/api/mutation`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ path: "todos:add", args: { text } }),
  });
  const body = await r.text();
  return (r.ok && (JSON.parse(body) as { status?: string }).status === "success") || `${r.status} ${body.slice(0, 200)}`;
}

async function remaining(): Promise<number> {
  const r = await fetch(`${BASE}/api/query`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ path: "todos:remaining", args: {} }),
  });
  return ((await r.json()) as { value: number }).value;
}

function load(writers = 4) {
  let stopped = false;
  const stats = { ok: 0, failed: 0, errors: new Set<string>() };
  const done = Promise.all(
    Array.from({ length: writers }, async (_, w) => {
      let n = 0;
      while (!stopped) {
        try {
          const r = await add(`w${w}-${n++}`);
          if (r === true) stats.ok++;
          else {
            stats.failed++;
            stats.errors.add(r);
          }
        } catch (e) {
          stats.failed++;
          stats.errors.add(String(e));
        }
      }
    }),
  );
  return { stats, stop: async () => ((stopped = true), await done) };
}

function check(name: string, cond: boolean, detail: string) {
  console.log(`${cond ? "✓" : "✗"} ${name}${cond ? "" : `: ${detail}`}`);
  if (!cond) failures.push(`${name}: ${detail}`);
}

try {
  await compose("down", "-v", "--remove-orphans");
  const up = await compose("up", "-d");
  if (up.code !== 0) throw new Error(`compose up failed:\n${up.out}`);
  await waitHealthy();
  const [first] = await appContainers();
  let written = 0;

  // 1. A rollout.
  let l = load();
  await Bun.sleep(1500);
  const started = Date.now();
  const deploy = await run(["./deploy.sh"]);
  const took = Date.now() - started;
  await Bun.sleep(1500);
  await l.stop();
  written += l.stats.ok;
  check("rollout: deploy.sh succeeded", deploy.code === 0, deploy.out);
  console.log(`  (${took} ms, ${l.stats.ok} writes during it)`);
  check("rollout: no failed request", l.stats.failed === 0, `${l.stats.failed} failed ${[...l.stats.errors].join("; ")}`);
  check("rollout: every write kept", (await remaining()) === written, `${await remaining()} vs ${written}`);
  const after = await appContainers();
  check("rollout: one container left, the new one", after.length === 1 && after[0] !== first, after.join(" "));

  // 2. A version that fails to start.
  const broken = Bun.spawn(["docker", "build", "-t", "todo-server:e2e-broken", "-"], { stdin: "pipe", stdout: "ignore", stderr: "pipe" });
  broken.stdin.write("FROM todo-server:latest\nENV FERRIX_LOG_FORMAT=nonsense\n");
  broken.stdin.end();
  if ((await broken.exited) !== 0) throw new Error(`building the broken image failed: ${await new Response(broken.stderr).text()}`);
  l = load();
  await Bun.sleep(1000);
  const bad = await run(["./deploy.sh"], { IMAGE: "todo-server:e2e-broken" });
  await Bun.sleep(1000);
  await l.stop();
  written += l.stats.ok;
  check("bad version: deploy.sh failed", bad.code !== 0, bad.out);
  check("bad version: no failed request", l.stats.failed === 0, `${l.stats.failed} failed ${[...l.stats.errors].join("; ")}`);
  check("bad version: every write kept", (await remaining()) === written, `${await remaining()} vs ${written}`);
  const still = await appContainers();
  check("bad version: the running container carries on", still.join() === after.join(), still.join(" "));
} catch (e) {
  failures.push(e instanceof Error ? (e.stack ?? e.message) : String(e));
} finally {
  if (failures.length) await Bun.write("/tmp/docker-rollout-e2e.log", (await compose("logs", "-t")).out);
  await compose("down", "-v", "--remove-orphans");
  await run(["docker", "image", "rm", "-f", "todo-server:e2e-broken"]);
}
if (failures.length) {
  console.error(`FAILED:\n${failures.join("\n")}`);
  process.exit(1);
}
console.log("PASS: docker rollout e2e");

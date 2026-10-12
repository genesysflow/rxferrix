// End-to-end test of zero-downtime takeovers with the real binary (`serve --takeover`), under write load:
//
//   cargo build -p todo-server && bun scripts/takeover-e2e.ts [todo-server binary]
//
// 1. Same host, same port: the new process takes the listening socket (systemd's `reload`).
// 2. Containers: old and new on their own ports behind a proxy that routes to healthy upstreams and retries refused
//    connections, as Traefik does. `--takeover=auto` in both, `--after-handover=wait`: the old
//    process stays alive after handing over until it gets SIGTERM.
// 3. Another network namespace (what a container or pod is), same port number: the new process, started with
//    `unshare --net`, must notice and bind its own socket instead of using the old server's. Skipped where
//    unprivileged namespaces aren't available.
//
// Every scenario: zero failed requests (through the proxy in 2), every acknowledged write present afterwards.

import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const BIN = resolve(process.argv[2] ?? join(import.meta.dir, "../target/debug/todo-server"));
const failures: string[] = [];
const procs: ReturnType<typeof Bun.spawn>[] = [];
const dirs: string[] = [];

function freePort(): number {
  const s = Bun.serve({ port: 0, hostname: "127.0.0.1", fetch: () => new Response() });
  const p = s.port!;
  s.stop(true);
  return p;
}

function dataDir(): string {
  const d = mkdtempSync(join(tmpdir(), "rxferrix-takeover-"));
  dirs.push(d);
  return d;
}

function spawn(args: string[], log: string[]): ReturnType<typeof Bun.spawn> {
  const p = Bun.spawn(args, { stdout: "pipe", stderr: "pipe" });
  procs.push(p);
  for (const stream of [p.stdout, p.stderr]) {
    void (async () => {
      const dec = new TextDecoder();
      for await (const chunk of stream as ReadableStream<Uint8Array>) log.push(dec.decode(chunk));
    })();
  }
  return p;
}

async function waitHealthy(base: string, ms = 20_000) {
  const deadline = Date.now() + ms;
  while (Date.now() < deadline) {
    try {
      if ((await fetch(`${base}/api/health`)).ok) return;
    } catch {}
    await Bun.sleep(50);
  }
  throw new Error(`${base} did not become healthy`);
}

async function add(base: string, text: string): Promise<boolean> {
  const r = await fetch(`${base}/api/mutation`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ path: "todos:add", args: { text } }),
  });
  const j = (await r.json()) as { status?: string };
  return r.ok && j.status === "success";
}

async function remaining(base: string): Promise<number> {
  const r = await fetch(`${base}/api/query`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ path: "todos:remaining", args: {} }),
  });
  return ((await r.json()) as { value: number }).value;
}

/** Writers hammering `base` until `stop()`; counts successes and failures. */
function load(base: string, writers = 4) {
  let stopped = false;
  const stats = { ok: 0, failed: 0, errors: new Set<string>() };
  const done = Promise.all(
    Array.from({ length: writers }, async (_, w) => {
      let n = 0;
      while (!stopped) {
        try {
          if (await add(base, `w${w}-${n++}`)) stats.ok++;
          else stats.failed++;
        } catch (e) {
          stats.failed++;
          stats.errors.add(String(e));
        }
      }
    }),
  );
  return {
    stats,
    stop: async () => {
      stopped = true;
      await done;
    },
  };
}

function check(name: string, cond: boolean, detail: string) {
  console.log(`${cond ? "✓" : "✗"} ${name}${cond ? "" : `: ${detail}`}`);
  if (!cond) failures.push(`${name}: ${detail}`);
}

/**
 * A Traefik stand-in with its rules: requests go to the healthy upstreams only (one joins on its first passing health
 * check, as Docker's start interval or a readiness probe has it, and leaves after 3 failing ones); with none, a 503 at
 * once; a request is retried only when the connection was refused (Traefik doesn't retry once it sent the request),
 * 40 times, 100 ms growing to 200 ms apart.
 */
function proxy(upstreams: string[]) {
  const healthy = new Set<string>();
  const failing = new Map<string, number>();
  const probe = setInterval(() => {
    for (const u of upstreams) {
      fetch(`${u}/api/health`, { signal: AbortSignal.timeout(500) })
        .then((r) => r.ok)
        .catch(() => false)
        .then((ok) => {
          const n = ok ? 0 : (failing.get(u) ?? 0) + 1;
          failing.set(u, n);
          if (ok) healthy.add(u);
          else if (n >= 3) healthy.delete(u);
        });
    }
  }, 250);
  const attempts = 40;
  let next = 0;
  const server = Bun.serve({
    port: 0,
    hostname: "127.0.0.1",
    async fetch(req) {
      const url = new URL(req.url);
      const body = await req.arrayBuffer();
      let interval = 100;
      for (let attempt = 1; ; attempt++) {
        const pool = [...healthy];
        if (!pool.length) return new Response("no available server", { status: 503 });
        const target = pool[next++ % pool.length]!;
        try {
          return await fetch(`${target}${url.pathname}${url.search}`, { method: req.method, headers: req.headers, body });
        } catch (e) {
          const refused = (e as { code?: string }).code === "ConnectionRefused";
          if (!refused || attempt >= attempts) return new Response(`bad gateway: ${e}`, { status: 502 });
          await Bun.sleep(interval);
          interval *= 2 ** (1 / (attempts - 1));
        }
      }
    },
  });
  return { base: `http://127.0.0.1:${server.port}`, stop: () => (clearInterval(probe), server.stop(true)) };
}

async function sameHost() {
  const dir = dataDir();
  const port = freePort();
  const base = `http://127.0.0.1:${port}`;
  const oldLog: string[] = [];
  const newLog: string[] = [];
  const old = spawn([BIN, "serve", "--dev", "--http", `127.0.0.1:${port}`, "--dir", dir], oldLog);
  await waitHealthy(base);
  const l = load(base);
  await Bun.sleep(1000);
  spawn([BIN, "serve", "--dev", "--http", `127.0.0.1:${port}`, "--dir", dir, "--takeover"], newLog);
  await old.exited;
  await Bun.sleep(700);
  await l.stop();
  check("same host: no failed request", l.stats.failed === 0, `${l.stats.failed} failed ${[...l.stats.errors].join("; ")}`);
  check("same host: every write kept", (await remaining(base)) === l.stats.ok, `${await remaining(base)} vs ${l.stats.ok}`);
  check("same host: the socket was handed over", newLog.join("").includes('"sockets":true') || oldLog.join("").includes('"sockets":true'), "no socket handover logged");
}

async function containers() {
  const dir = dataDir();
  const [a, b] = [freePort(), freePort()];
  const A = `http://127.0.0.1:${a}`;
  const B = `http://127.0.0.1:${b}`;
  const p = proxy([A, B]);
  const oldLog: string[] = [];
  const newLog: string[] = [];
  const env = ["--takeover=auto", "--after-handover=wait"];
  const old = spawn([BIN, "serve", "--dev", "--http", `127.0.0.1:${a}`, "--dir", dir, ...env], oldLog);
  await waitHealthy(A);
  await waitHealthy(p.base);
  const l = load(p.base);
  await Bun.sleep(1000);
  spawn([BIN, "serve", "--dev", "--http", `127.0.0.1:${b}`, "--dir", dir, ...env], newLog);
  await waitHealthy(B).catch((e) => {
    throw new Error(`${e}\n--- new:\n${newLog.join("").slice(-1500)}\n--- old:\n${oldLog.join("").slice(-1500)}`);
  });
  await Bun.sleep(700);
  await l.stop();
  p.stop();
  check("containers: no failed request through the proxy", l.stats.failed === 0, `${l.stats.failed} failed ${[...l.stats.errors].join("; ")}`);
  check("containers: every write kept", (await remaining(B)) === l.stats.ok, `${await remaining(B)} vs ${l.stats.ok}`);
  const waiting = oldLog.join("").includes("waiting for SIGTERM") && old.exitCode === null;
  check("containers: the old process waits after handing over", waiting, oldLog.join("").slice(-500));
  const refused = await fetch(`${A}/api/health`).then(
    (r) => `answered ${r.status}`,
    (e) => (e as { code?: string }).code ?? String(e),
  );
  check("containers: while waiting its port refuses connections", refused === "ConnectionRefused", refused);
  old.kill("SIGTERM");
  check("containers: and exits cleanly on SIGTERM", (await old.exited) === 0, `exit ${old.exitCode}`);
  // auto with nothing running: a normal start.
  const fresh = dataDir();
  const c = freePort();
  const solo = spawn([BIN, "serve", "--dev", "--http", `127.0.0.1:${c}`, "--dir", fresh, "--takeover=auto"], []);
  await waitHealthy(`http://127.0.0.1:${c}`);
  check("containers: --takeover=auto starts normally when nothing runs", true, "");
  solo.kill("SIGTERM");
  await solo.exited;
}

async function otherNamespace() {
  const probe = Bun.spawnSync(["unshare", "--user", "--map-root-user", "--net", "true"]);
  if (probe.exitCode !== 0) {
    console.log("- other network namespace: skipped (no unprivileged namespaces here)");
    return;
  }
  const dir = dataDir();
  const port = freePort();
  const base = `http://127.0.0.1:${port}`;
  const old = spawn([BIN, "serve", "--dev", "--http", `127.0.0.1:${port}`, "--dir", dir], []);
  await waitHealthy(base);
  for (let i = 0; i < 25; i++) await add(base, `before ${i}`);
  // The new process in its own network namespace, on the same port number; it checks itself from inside.
  const script = `ip link set lo up
${BIN} serve --dev --http 127.0.0.1:${port} --dir ${dir} --takeover=auto > ${dir}/new.log 2>&1 &
for i in $(seq 1 200); do curl -sf http://127.0.0.1:${port}/api/health >/dev/null && break; sleep 0.05; done
curl -s -X POST http://127.0.0.1:${port}/api/query -H 'content-type: application/json' -d '{"path":"todos:remaining","args":{}}'
kill %1; wait`;
  const inside = Bun.spawnSync(["unshare", "--user", "--map-root-user", "--net", "sh", "-c", script]);
  await old.exited;
  const log = await Bun.file(`${dir}/new.log`).text();
  const out = inside.stdout.toString();
  check("other namespace: noticed and bound its own socket", log.includes("another network namespace"), log.slice(-600));
  check("other namespace: took over with every write", out.includes('"value":25'), `${out} ${log.slice(-400)}`);
}

try {
  await sameHost();
  await containers();
  await otherNamespace();
} catch (e) {
  failures.push(e instanceof Error ? (e.stack ?? e.message) : String(e));
} finally {
  for (const p of procs) p.kill("SIGKILL");
  for (const d of dirs) rmSync(d, { recursive: true, force: true });
}
if (failures.length) {
  console.error(`FAILED:\n${failures.join("\n")}`);
  process.exit(1);
}
console.log("PASS: takeover e2e");

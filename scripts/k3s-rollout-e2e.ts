// End-to-end test of a zero-downtime rollout on Kubernetes (deploy/k8s/ferrix.yaml), under write load, in a k3s
// cluster that runs in a Docker container (k3s's own Traefik in front, local-path storage):
//
//   docker build -t todo-server . && bun scripts/k3s-rollout-e2e.ts
//
// Needs a Docker daemon you can use (the k3s container is privileged). Removes the cluster at the end. On failure the
// pods' and Traefik's logs go to /tmp/k3s-rollout-e2e.log; `TRAEFIK_DEBUG=1` turns on Traefik's debug and access logs
// (the rollout's part in /tmp/k3s-traefik.log).
//
// 1. `kubectl set image` to a new tag: the new pod takes over from the old one, the old one is terminated. Zero
//    failed requests through the ingress, every acknowledged write present afterwards, the new pod serving.
// 2. A version that fails to start: the rollout doesn't finish, the old pod carries on serving with zero failed
//    requests, and `kubectl rollout undo` ends it.

import { join } from "node:path";

const K3S = process.env.K3S_IMAGE ?? "rancher/k3s:v1.33.5-k3s1";
const NAME = "ferrix-k3s-e2e";
const PORT = 18081;
const BASE = `http://127.0.0.1:${PORT}`;
const HOST = "app.example.com";
const MANIFEST = join(import.meta.dir, "../deploy/k8s/ferrix.yaml");
const failures: string[] = [];

async function run(cmd: string[], stdin?: string | ReadableStream): Promise<{ code: number; out: string }> {
  const p = Bun.spawn(cmd, { stdin: typeof stdin === "string" ? new Blob([stdin]) : (stdin ?? "ignore"), stdout: "pipe", stderr: "pipe" });
  const [out, err] = await Promise.all([new Response(p.stdout).text(), new Response(p.stderr).text()]);
  return { code: await p.exited, out: out + err };
}
const kubectl = (args: string[], stdin?: string) => run(["docker", "exec", "-i", NAME, "kubectl", ...args], stdin);

async function until(what: string, ms: number, f: () => Promise<boolean>) {
  const deadline = Date.now() + ms;
  while (Date.now() < deadline) {
    if (await f().catch(() => false)) return;
    await Bun.sleep(1000);
  }
  throw new Error(`timed out waiting for ${what}`);
}

async function call(path: string, body: unknown): Promise<Response> {
  return fetch(`${BASE}${path}`, {
    method: "POST",
    headers: { host: HOST, "content-type": "application/json" },
    body: JSON.stringify(body),
  });
}

/** Adds a todo: `true`, or what went wrong. */
async function add(text: string): Promise<true | string> {
  const r = await call("/api/mutation", { path: "todos:add", args: { text } });
  const body = await r.text();
  return (r.ok && (JSON.parse(body) as { status?: string }).status === "success") || `${r.status} ${body.slice(0, 200)}`;
}

async function remaining(): Promise<number> {
  return ((await (await call("/api/query", { path: "todos:remaining", args: {} })).json()) as { value: number }).value;
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

const pods = async () =>
  (await kubectl(["get", "pods", "-l", "app=ferrix", "-o", "jsonpath={range .items[*]}{.metadata.name}={.spec.containers[0].image}={.status.phase} {end}"])).out.trim();

try {
  await run(["docker", "rm", "-f", NAME]);
  const started = await run(["docker", "run", "-d", "--privileged", "--name", NAME, "-p", `${PORT}:80`, K3S, "server", "--disable=metrics-server"]);
  if (started.code !== 0) throw new Error(`starting k3s failed: ${started.out}`);
  await until("the node", 120_000, async () => (await kubectl(["get", "nodes"])).out.includes(" Ready"));
  if (process.env.TRAEFIK_DEBUG) {
    // Traefik's debug log and access log (written to /tmp/k3s-rollout-e2e.log on failure).
    const config = `apiVersion: helm.cattle.io/v1
kind: HelmChartConfig
metadata: { name: traefik, namespace: kube-system }
spec:
  valuesContent: |-
    logs: { general: { level: DEBUG }, access: { enabled: true, filters: { statuscodes: "500-599" } } }
`;
    await until("the Helm CRD", 120_000, async () => (await kubectl(["apply", "-f", "-"], config)).code === 0);
  }
  // The image under two tags (a tag other than `latest`, so the cluster doesn't try to pull it).
  for (const tag of ["v1", "v2"]) {
    await run(["docker", "tag", "todo-server:latest", `todo-server:${tag}`]);
  }
  const save = Bun.spawn(["docker", "save", "todo-server:v1", "todo-server:v2"], { stdout: "pipe" });
  const imported = await run(["docker", "exec", "-i", NAME, "ctr", "images", "import", "-"], save.stdout);
  if (imported.code !== 0) throw new Error(`importing the image failed: ${imported.out}`);
  await until("Traefik", 240_000, async () =>
    (await kubectl(["-n", "kube-system", "get", "deploy", "traefik", "-o", "jsonpath={.status.readyReplicas}"])).out === "1",
  );
  if (process.env.TRAEFIK_DEBUG) {
    await until("Traefik with debug logs", 240_000, async () => {
      const args = (await kubectl(["-n", "kube-system", "get", "deploy", "traefik", "-o", "jsonpath={.spec.template.spec.containers[0].args}"])).out;
      return args.includes("DEBUG") && (await kubectl(["-n", "kube-system", "rollout", "status", "deploy/traefik", "--timeout=5s"])).code === 0;
    });
  }
  await until("Traefik's CRDs", 60_000, async () => (await kubectl(["get", "crd", "middlewares.traefik.io"])).code === 0);

  const manifest = (await Bun.file(MANIFEST).text()).replace("image: todo-server:latest", "image: todo-server:v1");
  const applied = await kubectl(["apply", "-f", "-"], manifest);
  if (applied.code !== 0) throw new Error(`kubectl apply failed: ${applied.out}`);
  await until("the first pod", 180_000, async () => (await kubectl(["rollout", "status", "deployment/ferrix", "--timeout=5s"])).code === 0);
  await until("the ingress", 60_000, async () => (await fetch(`${BASE}/api/health`, { headers: { host: HOST } })).ok);
  let written = 0;

  // 1. A rollout.
  const before = await pods();
  let l = load();
  await Bun.sleep(2000);
  const t0 = Date.now();
  await kubectl(["set", "image", "deployment/ferrix", "app=todo-server:v2"]);
  const status = await kubectl(["rollout", "status", "deployment/ferrix", "--timeout=180s"]);
  const took = Date.now() - t0;
  await until("the old pod to go", 60_000, async () => !(await pods()).includes("Terminating") && (await pods()).split(" ").length === 1);
  await Bun.sleep(2000);
  await l.stop();
  written += l.stats.ok;
  const after = await pods();
  if (process.env.TRAEFIK_DEBUG && l.stats.failed) {
    const t = await kubectl(["-n", "kube-system", "logs", "deploy/traefik", "--since=60s"]);
    const lines = t.out.split("\n").filter((x) => !x.includes("Service selected") && !x.includes("Skipping") && !x.includes('" 200 '));
    await Bun.write("/tmp/k3s-traefik.log", lines.join("\n"));
  }
  check("rollout: finished", status.code === 0, status.out);
  console.log(`  (${took} ms, ${l.stats.ok} writes during it; ${before} -> ${after})`);
  check("rollout: no failed request", l.stats.failed === 0, `${l.stats.failed} failed ${[...l.stats.errors].join("; ")}`);
  check("rollout: every write kept", (await remaining()) === written, `${await remaining()} vs ${written}`);
  check("rollout: the new pod serves", after.includes("todo-server:v2=Running") && !after.includes("v1"), after);

  // 2. A version that fails to start.
  l = load();
  await Bun.sleep(1000);
  await kubectl(["set", "env", "deployment/ferrix", "FERRIX_LOG_FORMAT=nonsense"]);
  const stuck = await kubectl(["rollout", "status", "deployment/ferrix", "--timeout=20s"]);
  await l.stop();
  written += l.stats.ok;
  check("bad version: the rollout doesn't finish", stuck.code !== 0, stuck.out);
  check("bad version: no failed request", l.stats.failed === 0, `${l.stats.failed} failed ${[...l.stats.errors].join("; ")}`);
  check("bad version: every write kept", (await remaining()) === written, `${await remaining()} vs ${written}`);
  await kubectl(["rollout", "undo", "deployment/ferrix"]);
  const undone = await kubectl(["rollout", "status", "deployment/ferrix", "--timeout=60s"]);
  check("bad version: rollout undo ends it", undone.code === 0 && (await remaining()) === written, `${undone.out} ${await pods()}`);
} catch (e) {
  failures.push(e instanceof Error ? (e.stack ?? e.message) : String(e));
} finally {
  if (failures.length) {
    console.error((await kubectl(["get", "pods", "-A", "-o", "wide"])).out);
    const logs = await kubectl(["logs", "-l", "app=ferrix", "--tail", "-1", "--prefix"]);
    const traefik = await kubectl(["-n", "kube-system", "logs", "deploy/traefik", "--tail", "-1"]);
    await Bun.write("/tmp/k3s-rollout-e2e.log", `${logs.out}\n--- traefik:\n${traefik.out}`);
    console.error("logs: /tmp/k3s-rollout-e2e.log");
  }
  await run(["docker", "rm", "-f", NAME]);
  await run(["docker", "image", "rm", "todo-server:v1", "todo-server:v2"]);
}
if (failures.length) {
  console.error(`FAILED:\n${failures.join("\n")}`);
  process.exit(1);
}
console.log("PASS: k3s rollout e2e");

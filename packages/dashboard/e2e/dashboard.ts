// End-to-end test of the embedded dashboard against the real todo-server binary.
//
//   cargo build -p todo-server && bun e2e/dashboard.ts [todo-server binary] [screenshot dir]
//
// Starts the server on a free port with a temp data dir (after `admin create`), then in Chromium: signs in at /_/,
// watches a todo inserted over the HTTP API appear live in the data grid, edits a document in the side panel, runs
// todos:add from the function runner, sees executions arrive in the live logs tail, uploads files on the Files page
// (picker and drag and drop, previews, delete), checks the Scheduler page (the example's cron, a job from
// todos:add_later appearing live, "Run now", "Cancel"), and checks that anonymous callers can't use the system
// functions. A second server with a fresh data dir checks the printed /_/setup#token=… flow.
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { chromium, type Browser, type Page } from "playwright-core";

const here = decodeURIComponent(new URL(".", import.meta.url).pathname);
const bin = resolve(process.argv[2] ?? join(here, "../../../target/debug/todo-server"));
const shots = process.argv[3] ?? process.env.SCREENSHOT_DIR ?? null;
const EMAIL = "admin@example.com";
const PASSWORD = "correct horse battery";

const failures: string[] = [];
const cleanups: (() => void | Promise<void>)[] = [];

function freePort(): number {
  const server = Bun.serve({ port: 0, hostname: "127.0.0.1", fetch: () => new Response() });
  const port = server.port!;
  server.stop(true);
  return port;
}

interface Server {
  base: string;
  output: () => string;
}

async function startServer(dir: string): Promise<Server> {
  const port = freePort();
  const base = `http://127.0.0.1:${port}`;
  const proc = Bun.spawn([bin, "serve", "--dir", dir, "--http", `127.0.0.1:${port}`], { stdout: "pipe", stderr: "pipe" });
  cleanups.push(() => proc.kill());
  let out = "";
  for (const stream of [proc.stdout, proc.stderr]) {
    void (async () => {
      const decoder = new TextDecoder();
      for await (const chunk of stream) out += decoder.decode(chunk);
    })();
  }
  for (let i = 0; i < 200; i++) {
    try {
      if ((await fetch(`${base}/api/health`)).ok) return { base, output: () => out };
    } catch {}
    await Bun.sleep(50);
  }
  throw new Error(`server did not start:\n${out}`);
}

function tempDir(): string {
  const dir = mkdtempSync(join(tmpdir(), "ferrix-dashboard-e2e-"));
  cleanups.push(() => rmSync(dir, { recursive: true, force: true }));
  return dir;
}

async function call(base: string, kind: string, path: string, args: unknown, token?: string) {
  const res = await fetch(`${base}/api/${kind}`, {
    method: "POST",
    headers: { "content-type": "application/json", ...(token ? { authorization: `Bearer ${token}` } : {}) },
    body: JSON.stringify({ path, args }),
  });
  return { status: res.status, body: (await res.json()) as Record<string, any> };
}

async function step<T>(label: string, f: () => Promise<T>): Promise<T> {
  const start = performance.now();
  const out = await f();
  console.log(`✓ ${label} (${(performance.now() - start).toFixed(0)}ms)`);
  return out;
}

async function shot(page: Page, name: string) {
  if (!shots) return;
  const path = join(shots, `dashboard-${name}.png`);
  await page.screenshot({ path });
  console.log(`  screenshot: ${path}`);
}

async function newPage(browser: Browser): Promise<Page> {
  const page = await browser.newPage({ viewport: { width: 1360, height: 820 } });
  page.on("pageerror", (e) => failures.push(`page error: ${e.message}`));
  page.on("console", (m) => {
    if (m.type() === "error") failures.push(`console error: ${m.text()}`);
  });
  page.on("dialog", (d) => void d.accept());
  return page;
}

/** A page link in the sidebar. */
function nav(page: Page, name: string) {
  return page.getByRole("navigation", { name: "Dashboard" }).getByRole("link", { name, exact: true });
}

let browser: Browser | undefined;
try {
  // ---------------------------------------------------------------------------------------------------------------
  // Server with an admin created on the command line.
  const dir = tempDir();
  const created = Bun.spawnSync([bin, "admin", "create", EMAIL, "--password", PASSWORD, "--dir", dir]);
  if (created.exitCode !== 0) throw new Error(`admin create failed: ${created.stderr.toString()}`);
  const server = await step("start todo-server", () => startServer(dir));
  const { base } = server;

  browser = await chromium.launch({ executablePath: process.env.CHROMIUM ?? "/usr/bin/chromium" });
  const page = await newPage(browser);

  // 1. Sign in at /_/.
  await step("sign in at /_/", async () => {
    await page.goto(`${base}/_/`);
    await page.getByLabel("Email").fill(EMAIL);
    await page.getByLabel("Password").fill(PASSWORD);
    await page.getByRole("button", { name: "Sign in" }).click();
    await page.getByRole("heading", { name: "Overview" }).waitFor({ timeout: 10_000 });
    await page.getByText("live", { exact: true }).waitFor({ timeout: 5000 });
  });

  // 2. Open the todos table; a todo inserted over the HTTP API appears live.
  const first = `e2e todo ${Date.now()}`;
  await step("open todos and see an HTTP insert live", async () => {
    await nav(page, "Data").click();
    await page.getByRole("link", { name: /^todos/ }).click();
    await page.getByRole("heading", { name: "todos" }).waitFor();
    await page.getByText("No documents yet").waitFor();
    const r = await call(base, "mutation", "todos:add", { text: first });
    if (r.status !== 200) throw new Error(`todos:add failed: ${JSON.stringify(r.body)}`);
    await page.getByTestId("document-grid").getByText(first).waitFor({ timeout: 3000 });
  });
  await shot(page, "data-grid");

  // 3. Edit the document in the side panel; the change persists.
  const edited = `${first} (edited in the dashboard)`;
  await step("edit a document in the panel", async () => {
    await page.getByTestId("document-grid").getByText(first).click();
    const editor = page.getByLabel("Document JSON");
    await editor.waitFor();
    await page.waitForFunction(() => (document.querySelector("textarea")?.value ?? "").includes("_id"));
    const doc = JSON.parse(await editor.inputValue());
    doc.text = edited;
    doc.done = true;
    await editor.fill(JSON.stringify(doc, null, 2));
    await shot(page, "document-panel");
    await page.getByRole("button", { name: /^Save/ }).click();
    await page.getByText("Saved").waitFor();
    await page.getByTestId("document-grid").getByText(edited).waitFor();
    const list = await call(base, "query", "todos:list", {});
    const stored = (list.body.value as { text: string; done: boolean }[]).find((t) => t.text === edited);
    if (!stored?.done) throw new Error(`edit not persisted: ${JSON.stringify(list.body)}`);
    await page.reload();
    await page.getByTestId("document-grid").getByText(edited).waitFor();
    // Invalid documents are rejected with the schema error.
    await page.getByLabel("Document JSON").fill('{ "text": 42, "done": false }');
    await page.getByRole("button", { name: /^Save/ }).click();
    await page.getByRole("alert").filter({ hasText: "does not match table `todos`" }).waitFor();
    await page.getByRole("button", { name: "Discard" }).click();
    await page.keyboard.press("Escape");
  });

  // 4. Run todos:add from the function runner.
  const fromRunner = `from the runner ${Date.now()}`;
  await step("run todos:add from the function runner", async () => {
    await nav(page, "Functions").click();
    await page.getByRole("link", { name: /add$/ }).first().click();
    await page.getByRole("heading", { name: "todos:add" }).waitFor();
    await page.getByLabel("Arguments JSON").fill(JSON.stringify({ text: fromRunner }));
    await page.getByRole("button", { name: /^Run/ }).click();
    const result = page.getByTestId("run-result");
    await result.getByText("success").waitFor();
    await result.getByText("[INFO] adding a todo").waitFor();
    await shot(page, "function-runner");
    const list = await call(base, "query", "todos:list", {});
    if (!(list.body.value as { text: string }[]).some((t) => t.text === fromRunner)) throw new Error("runner insert missing");
  });

  // 5. The live logs tail shows executions as they happen.
  const live = `logged live ${Date.now()}`;
  await step("see executions in the live logs tail", async () => {
    await nav(page, "Logs").click();
    await page.getByRole("heading", { name: "Logs" }).waitFor();
    await page.locator(".state", { hasText: "live" }).waitFor();
    // History includes the runner's call.
    await page.getByTestId("logs").getByText("todos:add").first().waitFor();
    await page.getByLabel("Path prefix").fill("todos:add");
    await page.locator(".state", { hasText: "live" }).waitFor();
    const rows = page.getByTestId("logs").locator("tbody tr");
    // Wait until the filtered history has loaded (every visible row is a todos:add call).
    await page.waitForFunction(() => {
      const paths = [...document.querySelectorAll("[data-testid=logs] tbody tr td:nth-child(3)")].map((td) => td.textContent);
      return paths.length > 0 && paths.every((p) => p === "todos:add");
    });
    const before = await rows.count();
    const r = await call(base, "mutation", "todos:add", { text: live });
    if (r.status !== 200) throw new Error("mutation failed");
    // The new execution arrives over SSE without a reload; expand it: its args are the ones just sent.
    await page.waitForFunction((n) => document.querySelectorAll("[data-testid=logs] tbody tr").length > n, before, {
      timeout: 3000,
    });
    await rows.first().click();
    await page.getByTestId("logs").getByText(live).waitFor({ timeout: 3000 });
    await shot(page, "logs");
  });

  // Overview and users, for the screenshots (and to make sure they render).
  await step("overview and users render", async () => {
    await nav(page, "Overview").click();
    await page.getByTestId("stat-ts").waitFor();
    await page.getByText("Busiest functions").waitFor();
    await shot(page, "overview");
    await nav(page, "Users").click();
    await page.getByText("No users yet").waitFor();
  });

  // 6. Files: upload through the picker and by drag and drop, preview, serve, delete.
  await step("upload, preview and delete files on the Files page", async () => {
    await nav(page, "Files").click();
    await page.getByRole("heading", { name: "Files" }).waitFor();
    await page.getByText("No files yet").waitFor();
    const text = `hello from the dashboard e2e ${Date.now()}`;
    await page.getByLabel("Upload files").setInputFiles({ name: "hello.txt", mimeType: "text/plain", buffer: Buffer.from(text) });
    const grid = page.getByTestId("files");
    await grid.getByText("hello.txt").waitFor({ timeout: 5000 });
    // The new file is selected: its text preview is fetched from /api/storage/<id>.
    await page.getByTestId("text-preview").getByText(text).waitFor({ timeout: 5000 });
    const id = new URL(page.url()).searchParams.get("file");
    if (!id) throw new Error(`no file selected: ${page.url()}`);
    const served = await fetch(`${base}/api/storage/${id}`);
    if ((await served.text()) !== text || served.headers.get("content-type") !== "text/plain") {
      throw new Error(`served file differs: ${served.status} ${served.headers.get("content-type")}`);
    }
    // Drag and drop a PNG: it gets a thumbnail.
    const png = Buffer.from(
      "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==",
      "base64",
    );
    const dataTransfer = await page.evaluateHandle((b64) => {
      const bytes = Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
      const dt = new DataTransfer();
      dt.items.add(new File([bytes], "pixel.png", { type: "image/png" }));
      return dt;
    }, png.toString("base64"));
    const region = page.getByRole("region", { name: "Files" });
    await region.dispatchEvent("dragover", { dataTransfer });
    await region.dispatchEvent("drop", { dataTransfer });
    await grid.getByText("pixel.png").waitFor({ timeout: 5000 });
    await page.waitForFunction(() =>
      [...document.querySelectorAll<HTMLImageElement>("[data-testid=files] img")].some((i) => i.complete && i.naturalWidth === 1),
    );
    await shot(page, "files");
    // An app document referencing a file (a todo's image): its Id<"_storage"> in the data grid opens the Files page.
    const upload = await call(base, "mutation", "todos:generate_upload_url", {});
    const uploaded = await fetch(upload.body.value as string, { method: "POST", headers: { "content-type": "image/png" }, body: png });
    const { storageId } = (await uploaded.json()) as { storageId: string };
    const todo = await call(base, "mutation", "todos:add", { text: "todo with an image" });
    const attached = await call(base, "mutation", "todos:set_image", { id: todo.body.value, image: storageId });
    if (attached.status !== 200) throw new Error(`set_image failed: ${JSON.stringify(attached.body)}`);
    await nav(page, "Data").click();
    await page.getByRole("link", { name: /^todos/ }).click();
    await page.getByTestId("document-grid").getByRole("link", { name: storageId }).click();
    await page.getByRole("heading", { name: "Files" }).waitFor();
    await page.getByRole("complementary", { name: "File details" }).getByText(storageId, { exact: true }).first().waitFor();
    if (new URL(page.url()).searchParams.get("file") !== storageId) throw new Error(`not linked: ${page.url()}`);
    // Admin-only: anonymous callers can't list files.
    const anon = await call(base, "query", "_system/storage:files", { paginationOpts: { numItems: 10, cursor: null } });
    if (anon.status !== 400 || anon.body.errorData?.code !== "unauthenticated") throw new Error(`files not refused: ${JSON.stringify(anon.body)}`);
    // Delete the text file (the confirm dialog is accepted): it disappears live and is no longer served.
    await grid.getByRole("row").filter({ hasText: "hello.txt" }).getByRole("button", { name: "Delete" }).click();
    await grid.getByText("hello.txt").waitFor({ state: "detached", timeout: 5000 });
    const gone = await fetch(`${base}/api/storage/${id}`);
    if (gone.status !== 404) throw new Error(`deleted file still served: ${gone.status}`);
  });

  // 6b. The Scheduler page: the example's cron, a job scheduled by todos:add_later appearing live, "Run now" and
  // "Cancel".
  await step("scheduler page: crons and live jobs, run now, cancel", async () => {
    await nav(page, "Scheduler").click();
    await page.getByRole("heading", { name: "Scheduler" }).waitFor();
    const crons = page.getByTestId("crons");
    await crons.getByText("purge-completed").waitFor();
    await crons.getByText("0 0 * * * *").waitFor();
    await crons.getByText("todos:purge_completed").waitFor();

    const later = `reminder ${Date.now()}`;
    const r = await call(base, "mutation", "todos:add_later", { text: later, delaySeconds: 3600 });
    if (r.status !== 200) throw new Error(`todos:add_later failed: ${JSON.stringify(r.body)}`);
    const jobId = r.body.value as string;
    const jobs = page.getByTestId("jobs");
    const row = jobs.locator("tbody tr").filter({ hasText: "todos:add" }).filter({ hasText: "pending" });
    await row.first().waitFor({ timeout: 3000 });
    await row.first().getByText("in 59m").or(row.first().getByText("in 1h")).first().waitFor();
    await shot(page, "scheduler");
    // Run it now: it runs in the background and turns into a success, live.
    await row.first().getByRole("button", { name: "Run now" }).click();
    const succeeded = jobs.locator("tbody tr").filter({ hasText: "todos:add" }).filter({ hasText: "success" }).first();
    await succeeded.waitFor({ timeout: 5000 });
    const list = await call(base, "query", "todos:list", {});
    if (!(list.body.value as { text: string }[]).some((t) => t.text === later)) throw new Error("the scheduled todo is missing");
    // The detail row shows the args.
    await succeeded.click();
    await jobs.getByText(later).first().waitFor();

    // Cancel another one; the "Canceled" filter shows it.
    const r2 = await call(base, "mutation", "todos:add_later", { text: "never", delaySeconds: 3600 });
    await row.first().getByRole("button", { name: "Cancel" }).click();
    await page.getByText("Job canceled").waitFor();
    await page.getByRole("button", { name: "Canceled", exact: true }).click();
    await jobs.locator("tbody tr").filter({ hasText: "canceled" }).first().waitFor();
    if (new URL(page.url()).searchParams.get("state") !== "canceled") throw new Error("the filter is not in the URL");
    const canceled = await call(base, "query", "_system/scheduler:jobs", { state: "canceled", paginationOpts: { numItems: 5, cursor: null } });
    if (canceled.status !== 400 || canceled.body.errorData?.code !== "unauthenticated") throw new Error("scheduler functions are not admin-only");
    if (!r2.body.value || jobId === r2.body.value) throw new Error("expected two distinct jobs");
  });

  // 7. Non-admins can't call the system functions.
  await step("anonymous callers are refused", async () => {
    const q = await call(base, "query", "_system/dashboard:tables", {});
    const a = await call(base, "action", "_system/dashboard:run_function", { path: "todos:add", args: { text: "x" } });
    const m = await call(base, "mutation", "_system/dashboard:delete_documents", { ids: [] });
    for (const r of [q, a, m]) {
      if (r.status !== 400 || r.body.errorData?.code !== "unauthenticated") throw new Error(`not refused: ${JSON.stringify(r.body)}`);
    }
    const logs = await fetch(`${base}/api/admin/logs`);
    if (logs.status !== 401) throw new Error(`/api/admin/logs answered ${logs.status} without a token`);
    // And the dark theme renders.
    await page.getByRole("button", { name: /system/ }).click();
    await nav(page, "Data").click();
    await page.getByTestId("document-grid").waitFor();
    await shot(page, "data-dark");
  });

  // ---------------------------------------------------------------------------------------------------------------
  // Setup link flow on a fresh data dir.
  await step("first-admin setup through the printed link", async () => {
    const fresh = await startServer(tempDir());
    let link: string | undefined;
    for (let i = 0; i < 100 && !link; i++) {
      link = /(http:\/\/\S+\/_\/setup#token=[\w-]+)/.exec(fresh.output())?.[1];
      if (!link) await Bun.sleep(50);
    }
    if (!link) throw new Error(`no setup link printed:\n${fresh.output()}`);
    const p = await newPage(browser!);
    await p.goto(link.replace(/^http:\/\/[^/]+/, fresh.base));
    await p.getByRole("heading", { name: "Create the first administrator" }).waitFor();
    if (new URL(p.url()).hash !== "") throw new Error("the token stayed in the address bar");
    await shot(p, "setup");
    await p.getByLabel("Email").fill("first@example.com");
    await p.getByLabel(/^Password/).fill(PASSWORD);
    await p.getByLabel("Confirm password").fill(PASSWORD);
    await p.getByRole("button", { name: "Create administrator" }).click();
    await p.getByRole("heading", { name: "Overview" }).waitFor({ timeout: 10_000 });
    await p.getByText("first@example.com").waitFor();
    // The link works once.
    const again = await fetch(`${fresh.base}/api/auth/admin/setup`).then((r) => r.json());
    if (again.needsSetup !== false) throw new Error("setup still pending");
    await p.close();
  });
} catch (e) {
  failures.push(e instanceof Error ? (e.stack ?? e.message) : String(e));
} finally {
  await browser?.close();
  for (const c of cleanups.reverse()) await c();
}

if (failures.length) {
  console.error(`FAILED:\n${failures.join("\n")}`);
  process.exit(1);
}
console.log("PASS: dashboard e2e");

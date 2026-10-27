// End-to-end test of the embedded dashboard against the real todo-server binary.
//
//   cargo build -p todo-server && bun e2e/dashboard.ts [todo-server binary] [screenshot dir]
//
// Screenshots are written as `<dir>/<SCREENSHOT_PREFIX ?? "dashboard-"><name>.png`.
//
// Starts the server on a free port with a temp data dir (after `admin create`), then in Chromium: signs in at /_/,
// watches a todo inserted over the HTTP API appear live in the data grid, edits a document in the side panel, runs
// todos:add from the function runner, sees executions arrive in the live logs tail, uploads files on the Files page
// (picker and drag and drop, previews, delete), checks the Scheduler page (the example's cron, a job from
// todos:add_later appearing live, "Run now", "Cancel", the job history), and checks that anonymous callers can't use the system
// functions. Then the operations pages: settings (a non-secret edit, the SMTP password round trip without ever
// showing it, a field locked by a command-line flag, inline validation), the audit log, backups (create, download,
// delete; the scheduled backup cron), export and import (2000 seeded todos), the virtualized grid scrolled to the
// end, the search box, a migration (dry run, run), "Run now" on a cron, the latency charts, "open app as user" and
// live session revocation, the command palette, the phone layout, environment variables. A second server with private storage checks thumbnails and
// downloads of signed URLs; others a staged restore with "Restart now", continuous replication (the status card and
// a point-in-time restore by commit ts), and, with a fresh data dir, the printed /_/setup#token=… flow.
import { mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { chromium, type Browser, type Page } from "playwright-core";

const here = decodeURIComponent(new URL(".", import.meta.url).pathname);
const bin = resolve(process.argv[2] ?? join(here, "../../../target/debug/todo-server"));
const shots = process.argv[3] ?? process.env.SCREENSHOT_DIR ?? null;
const shotPrefix = process.env.SCREENSHOT_PREFIX ?? "dashboard-";
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
  port: number;
  output: () => string;
  /** Resolves with the exit code. */
  exited: Promise<number>;
}

async function startServer(dir: string, extraArgs: (base: string) => string[] = () => [], port = freePort()): Promise<Server> {
  const base = `http://127.0.0.1:${port}`;
  const proc = Bun.spawn([bin, "serve", "--dir", dir, "--http", `127.0.0.1:${port}`, ...extraArgs(base)], {
    stdout: "pipe",
    stderr: "pipe",
  });
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
      if ((await fetch(`${base}/api/health`)).ok) return { base, port, output: () => out, exited: proc.exited };
    } catch {}
    await Bun.sleep(50);
  }
  throw new Error(`server did not start:\n${out}`);
}

function tempDir(): string {
  const dir = mkdtempSync(join(tmpdir(), "rxferrix-dashboard-e2e-"));
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

/** How many failed HTTP responses (4xx) the next steps provoke on purpose. */
let expectedHttpErrors = 0;
/** Console errors expected while a server restarts on purpose (refused connections, the ended session). */
let tolerated: RegExp | null = null;

/** The page of the step that runs, for a screenshot when it fails (`FAILURE_SCREENSHOT=<path>`). */
let lastPage: Page | null = null;

async function step<T>(label: string, f: () => Promise<T>): Promise<T> {
  const start = performance.now();
  let out: T;
  try {
    out = await f();
  } catch (e) {
    const path = process.env.FAILURE_SCREENSHOT;
    if (path && lastPage && !lastPage.isClosed()) await lastPage.screenshot({ path }).catch(() => {});
    throw e;
  }
  console.log(`✓ ${label} (${(performance.now() - start).toFixed(0)}ms)`);
  return out;
}

async function shot(page: Page, name: string) {
  if (!shots) return;
  const path = join(shots, `${shotPrefix}${name}.png`);
  await page.screenshot({ path });
  console.log(`  screenshot: ${path}`);
}

async function newPage(browser: Browser): Promise<Page> {
  const page = await browser.newPage({ viewport: { width: 1360, height: 820 } });
  lastPage = page;
  page.on("pageerror", (e) => failures.push(`page error: ${e.message}`));
  page.on("console", (m) => {
    if (m.type() !== "error") return;
    if (tolerated?.test(m.text())) return;
    // A request the test makes fail on purpose (the browser logs every failed response).
    if (expectedHttpErrors > 0 && /^Failed to load resource: the server responded with a status of 4\d\d/.test(m.text())) {
      expectedHttpErrors--;
      return;
    }
    failures.push(`console error: ${m.text()}`);
  });
  page.on("dialog", (d) => void d.accept());
  return page;
}

/** An admin access token for HTTP calls made by the test itself. */
async function adminToken(base: string): Promise<string> {
  const res = await fetch(`${base}/api/auth/admin/signin`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ email: EMAIL, password: PASSWORD }),
  });
  const body = (await res.json()) as { accessToken?: string };
  if (!body.accessToken) throw new Error(`admin sign-in failed: ${JSON.stringify(body)}`);
  return body.accessToken;
}

async function signIn(page: Page, base: string) {
  await page.goto(`${base}/_/`);
  await page.getByLabel("Email").fill(EMAIL);
  await page.getByLabel("Password").fill(PASSWORD);
  await page.getByRole("button", { name: "Sign in" }).click();
  await page.getByRole("heading", { name: "Overview" }).waitFor({ timeout: 10_000 });
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
  // The todo app has no sign-in: an app user to impersonate comes from an (offline) import into `_users`.
  const usersFile = join(dir, "users.jsonl");
  writeFileSync(usersFile, `${JSON.stringify({ email: "ada@example.com", emailVerified: true, name: "Ada", image: null })}\n`);
  const imported = Bun.spawnSync([bin, "import", usersFile, "--table", "_users", "--dir", dir]);
  if (imported.exitCode !== 0) throw new Error(`user import failed: ${imported.stderr.toString()}`);
  // The redirect allowlist comes from the command line, so the Settings page shows it locked (`/api/health` stands
  // in for an app page "open app as user" can land on: the server has no web app); the backup schedule declares the
  // `rxferrix-backup` cron.
  const server = await step("start todo-server", () =>
    startServer(dir, (b) => ["--auth-redirect-url", `${b}/,${b}/api/health`, "--backup-every", "1h", "--backup-keep", "3"]),
  );
  const { base } = server;
  const admin = await adminToken(base);

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
    // A removed document fades out in place for a moment (a "ghost" row), then goes.
    const brief = await call(base, "mutation", "todos:add", { text: "short-lived" });
    await page.getByTestId("document-grid").getByText("short-lived").waitFor({ timeout: 3000 });
    await call(base, "mutation", "todos:remove", { id: brief.body.value });
    await page.getByTestId("removed-row").filter({ hasText: "short-lived" }).waitFor({ timeout: 3000 });
    await page.getByTestId("removed-row").waitFor({ state: "detached", timeout: 3000 });
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
    // The function's own log, live: calls of todos:add arrive, calls of other functions don't.
    const fnLogs = page.getByTestId("function-logs");
    await fnLogs.locator("tbody tr").first().waitFor({ timeout: 3000 });
    await page.waitForTimeout(300); // the history has loaded
    const rows = fnLogs.locator("tbody tr");
    const before = await rows.count();
    await call(base, "mutation", "todos:add", { text: "logged for this function" });
    await call(base, "query", "todos:list", {});
    await page.waitForFunction((n) => document.querySelectorAll("[data-testid=function-logs] tbody tr").length === n + 1, before, {
      timeout: 3000,
    });
    await page.waitForTimeout(300);
    if ((await rows.count()) !== before + 1) throw new Error("another function's call showed up in the function's logs");
    await rows.first().click();
    await fnLogs.getByText("logged for this function").waitFor();
    await page.getByRole("link", { name: "View in Logs →" }).click();
    await page.getByTestId("log-function").filter({ hasText: "todos:add" }).waitFor();
    await page.getByTestId("logs").getByText("todos:add").first().waitFor();
    await page.getByRole("button", { name: "Show all functions" }).click();
    await page.getByLabel("Path prefix").waitFor();
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
    await page.getByRole("row").filter({ hasText: "ada@example.com" }).waitFor();
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
    // The list shows a server-side thumbnail (`thumb=96x96&fit=cover`: the 1×1 pixel scaled to fill 96×96).
    await page.waitForFunction(() =>
      [...document.querySelectorAll<HTMLImageElement>("[data-testid=files] img[data-testid=thumb]")].some(
        (i) => i.complete && i.naturalWidth === 96 && i.src.includes("thumb=96x96"),
      ),
    );
    // Download links come from `downloadUrl`.
    const href = await grid.getByRole("row").filter({ hasText: "pixel.png" }).getByRole("link", { name: "Download" }).getAttribute("href");
    if (!href || !/^\/api\/storage\/\w+\?download=1$/.test(href)) throw new Error(`download link: ${href}`);
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
    await page.getByRole("group", { name: "Filter jobs by state" }).getByRole("button", { name: "Canceled", exact: true }).click();
    await jobs.locator("tbody tr").filter({ hasText: "canceled" }).first().waitFor();
    if (new URL(page.url()).searchParams.get("state") !== "canceled") throw new Error("the filter is not in the URL");
    const canceled = await call(base, "query", "_system/scheduler:jobs", { state: "canceled", paginationOpts: { numItems: 5, cursor: null } });
    if (canceled.status !== 400 || canceled.body.errorData?.code !== "unauthenticated") throw new Error("scheduler functions are not admin-only");
    if (!r2.body.value || jobId === r2.body.value) throw new Error("expected two distinct jobs");
    // The job history (aux.db): nothing has finished an hour ago yet; its filters read it again.
    const history = page.getByRole("region", { name: "History" });
    await history.getByText("The history is empty").waitFor();
    await history.getByRole("group", { name: "Filter the history by state" }).getByRole("button", { name: "Failed", exact: true }).click();
    await history.getByText("No failed jobs in the history").waitFor();
    const anonymous = await fetch(`${base}/api/admin/job-history`);
    if (anonymous.status !== 401) throw new Error(`the job history is not admin-only (${anonymous.status})`);
  });

  // 6c. "Run now" on a cron; the time zone column; system crons grouped last.
  await step("scheduler: run a cron now", async () => {
    const crons = page.getByTestId("crons");
    const cronRow = crons.locator("tbody tr").filter({ hasText: "purge-completed" });
    await cronRow.getByText("UTC", { exact: true }).waitFor();
    await crons.getByText("System crons").waitFor();
    const names = await crons.locator("tbody tr td:first-child strong").allTextContents();
    const sweep = names.indexOf("_storage_sweep");
    if (sweep === -1 || sweep < names.indexOf("purge-completed") || sweep < names.indexOf("rxferrix-backup")) {
      throw new Error(`system crons are not last: ${names.join(", ")}`);
    }
    await cronRow.getByRole("button", { name: /Run now/ }).click();
    await page.getByText("Started purge-completed").waitFor();
    await cronRow.getByText("success", { exact: true }).waitFor({ timeout: 10_000 });
    await shot(page, "scheduler-crons");
  });

  // 8. Settings: a field locked by the command line, inline validation, a non-secret edit, and the SMTP password
  // round trip (stored, never shown, kept when other fields change).
  const SECRET = `s3cret-${Date.now()}`;
  const getSettings = async () => (await call(base, "action", "_system/settings:get", {}, admin)).body.value;
  await step("settings: locked field, validation, edits, masked secret round trip", async () => {
    await nav(page, "Settings").click();
    await page.getByRole("heading", { name: "Settings" }).waitFor();
    const locked = page.getByTestId("locked-note").filter({ hasText: "redirect allowlist" });
    await locked.getByText(`${base}/`).waitFor();
    if ((await page.locator("textarea[aria-label='Redirect URLs']").count()) > 0) throw new Error("the locked redirect URLs are editable");

    await page.getByLabel("Access token lifetime (seconds)").fill("5");
    await page.getByRole("button", { name: "Save changes" }).click();
    await page.getByRole("alert").filter({ hasText: "from 30 to 86400" }).waitFor();
    await page.getByLabel("Access token lifetime (seconds)").fill("");

    await page.getByLabel("Session lifetime (seconds)").fill("7200");
    await page.getByTestId("unsaved").waitFor();
    await page.getByRole("button", { name: "Save changes" }).click();
    await page.getByTestId("unsaved").waitFor({ state: "detached" });
    const v1 = await getSettings();
    if (v1.sessionTtlSecs !== 7200 || v1.effective.sessionTtlSecs !== 7200) throw new Error(`not saved: ${JSON.stringify(v1)}`);

    await page.getByLabel("SMTP URL").fill("smtp://mailer@127.0.0.1:2525");
    await page.getByLabel("From", { exact: true }).fill("Todo <noreply@example.com>");
    await page.getByRole("button", { name: "Set smtp password" }).click();
    await page.getByLabel("SMTP password", { exact: true }).fill(SECRET);
    await page.getByRole("button", { name: "Save changes" }).click();
    await page.getByLabel("SMTP password (stored, hidden)").waitFor();
    if ((await page.content()).includes(SECRET)) throw new Error("the secret is on the page");
    const v2 = await getSettings();
    if (v2.smtp?.password !== "••••" || v2.smtp?.url.includes(SECRET)) throw new Error(`smtp: ${JSON.stringify(v2.smtp)}`);
    await shot(page, "settings");

    // Another SMTP field changes; the stored password is kept (sent back as the mask).
    await page.getByLabel("From", { exact: true }).fill("Todo App <noreply@example.com>");
    await page.getByRole("button", { name: "Save changes" }).click();
    await page.getByTestId("unsaved").waitFor({ state: "detached" });
    const v3 = await getSettings();
    if (v3.smtp?.from !== "Todo App <noreply@example.com>" || v3.smtp?.password !== "••••") {
      throw new Error(`the password was not kept: ${JSON.stringify(v3.smtp)}`);
    }
    // The secret is nowhere the dashboard can read: not in _settings documents, not in the execution log.
    const rows = await call(base, "query", "_system/dashboard:documents", { table: "_settings", paginationOpts: { numItems: 20, cursor: null } }, admin);
    const logs = await fetch(`${base}/api/admin/logs?pathPrefix=_system/settings&limit=50`, { headers: { authorization: `Bearer ${admin}` } });
    if (JSON.stringify(rows.body).includes(SECRET) || (await logs.text()).includes(SECRET)) throw new Error("the secret leaked");
    if (!JSON.stringify(rows.body).includes("••••")) throw new Error("the encrypted column is not masked");
  });

  // 9. The audit log: the settings changes and the earlier document edit, live, with diffs and filters.
  await step("audit log: entries, live updates, filters, diff", async () => {
    await nav(page, "Audit log").click();
    await page.getByRole("heading", { name: "Audit log" }).waitFor();
    const audit = page.getByTestId("audit");
    await audit.getByText("settings", { exact: true }).first().waitFor();
    await audit.getByText("replace", { exact: true }).first().waitFor();
    // A new write shows up live.
    const list = await call(base, "query", "todos:list", {});
    const todo = (list.body.value as { _id: string }[])[0]!;
    const patched = await call(base, "mutation", "_system/dashboard:patch_document", { id: todo._id, patch: { text: "audited live" } }, admin);
    if (patched.status !== 200) throw new Error(`patch failed: ${JSON.stringify(patched.body)}`);
    const row = audit.locator("tbody tr").filter({ hasText: "patch" }).first();
    await row.waitFor({ timeout: 5000 });
    await row.locator("td").first().click();
    await page.getByTestId("audit-diff").first().getByText("audited live").waitFor();
    await shot(page, "audit");
    // Filter by action: only settings changes, in the URL; the diff shows the password masked.
    await page.getByLabel("Action", { exact: true }).selectOption("settings_update");
    await page.waitForFunction(() => {
      const cells = [...document.querySelectorAll("[data-testid=audit] tbody tr:not(.detail) td:nth-child(3)")];
      return cells.length > 0 && cells.every((c) => c.textContent?.trim() === "settings");
    });
    if (new URL(page.url()).searchParams.get("action") !== "settings_update") throw new Error("the filter is not in the URL");
    await audit.locator("tbody tr").first().locator("td").first().click();
    await page.getByTestId("audit-diff").first().getByText("smtp", { exact: true }).waitFor();
    if ((await page.content()).includes(SECRET)) throw new Error("the secret is in the audit view");
    await page.getByRole("button", { name: "Clear filters" }).click();
  });

  // 10. Backups: the target and the schedule, create, download (a zstd archive), delete.
  await step("backups: create, download, delete", async () => {
    await nav(page, "Backups").click();
    await page.getByRole("heading", { name: "Backups and data" }).waitFor();
    await page.getByTestId("backup-target").getByText(/backups$/).waitFor();
    await page.getByTestId("backup-schedule").getByText(/keeping the last 3/).waitFor();
    await page.getByRole("button", { name: "Create backup" }).click();
    const row = page.getByTestId("backups").locator("tbody tr").filter({ hasText: "manual" }).first();
    await row.waitFor({ timeout: 30_000 });
    const name = (await row.locator("td").first().textContent())!.trim();
    await shot(page, "backups");
    const [download] = await Promise.all([page.waitForEvent("download"), row.getByRole("button", { name: "Download" }).click()]);
    const bytes = readFileSync((await download.path())!);
    if (download.suggestedFilename() !== name) throw new Error(`download name ${download.suggestedFilename()} ≠ ${name}`);
    if (bytes.length < 100 || bytes.readUInt32LE(0) !== 0xfd2fb528) throw new Error(`not a zstd archive (${bytes.length} bytes)`);
    await row.getByRole("button", { name: "Delete" }).click();
    await page.locator("[data-testid=backups] td", { hasText: name }).waitFor({ state: "detached" });
    const listed = await fetch(`${base}/api/admin/backups`, { headers: { authorization: `Bearer ${admin}` } }).then((r) => r.json());
    if ((listed as { backups: { name: string }[] }).backups.some((b) => b.name === name)) throw new Error("not deleted");
  });

  // 10b. The destination set from the dashboard (the schedule is fixed by --backup-every, so it is locked).
  await step("backups: destination configured from the dashboard", async () => {
    const elsewhere = tempDir();
    await page.getByTestId("configure-backups").click();
    const dialog = page.getByTestId("backup-settings");
    await dialog.getByText("Set on the command line").first().waitFor();
    await page.getByLabel("Another directory on the server").check();
    await page.getByLabel("Directory", { exact: true }).fill(elsewhere);
    await page.getByRole("button", { name: "Test destination" }).click();
    await page.getByTestId("backup-test-result").filter({ hasText: "✓" }).waitFor();
    await shot(page, "backup-settings");
    await page.getByRole("button", { name: "Save", exact: true }).click();
    await page.getByTestId("backup-target").filter({ hasText: elsewhere }).waitFor();
    await page.getByRole("button", { name: "Create backup" }).click();
    for (let i = 0; i < 100 && !readdirSync(elsewhere).some((f) => f.endsWith(".tar.zst")); i++) await Bun.sleep(100);
    if (!readdirSync(elsewhere).some((f) => f.endsWith(".tar.zst"))) throw new Error("the backup did not go to the configured directory");
    // Removing the settings goes back to <data dir>/backups.
    await page.getByTestId("configure-backups").click();
    await page.getByRole("button", { name: "Remove settings" }).click();
    await page.getByTestId("backup-target").filter({ hasText: /backups$/ }).waitFor();
  });

  // 11. Export the todos table (JSONL and CSV); import JSONL: a bad file is refused whole, then 2000 seeded todos.
  const SEED = 2000;
  await step("export and import", async () => {
    await page.getByLabel("Table to export").selectOption("todos");
    const [jsonl] = await Promise.all([page.waitForEvent("download"), page.getByRole("button", { name: "Export", exact: true }).click()]);
    const lines = readFileSync((await jsonl.path())!, "utf8").trim().split("\n").map((l) => JSON.parse(l) as { text: string });
    if (jsonl.suggestedFilename() !== "todos.jsonl" || !lines.some((t) => t.text === "audited live")) {
      throw new Error(`export: ${jsonl.suggestedFilename()} ${lines.length} lines`);
    }
    await page.getByLabel("Export format").selectOption("csv");
    const [csv] = await Promise.all([page.waitForEvent("download"), page.getByRole("button", { name: "Export", exact: true }).click()]);
    const csvText = readFileSync((await csv.path())!, "utf8");
    const header = csvText.split("\r\n")[0];
    if (header !== "_id,_creationTime,completedAt,done,image,text") throw new Error(`csv header: ${header}`);

    // The CSV export goes back in (upsert: same ids, same values): the format follows the file name.
    await page.getByLabel("Table", { exact: true }).selectOption("todos");
    await page.getByLabel("Mode", { exact: true }).selectOption("upsert");
    await page.getByLabel("File", { exact: true }).setInputFiles({ name: "todos.csv", mimeType: "text/csv", buffer: Buffer.from(csvText) });
    if ((await page.getByLabel("Format", { exact: true }).inputValue()) !== "csv") throw new Error("CSV not picked from the name");
    await page.getByRole("button", { name: "Import", exact: true }).click();
    const rows = csvText.trim().split("\r\n").length - 1;
    await page.getByTestId("import-result").getByText(`${rows} document${rows === 1 ? "" : "s"} imported into`).waitFor({ timeout: 10_000 });
    await page.getByTestId("import-result").getByText(`${rows} replaced`).waitFor();
    const after = await call(base, "query", "todos:list", {});
    if (!JSON.stringify(after.body).includes("audited live")) throw new Error(`todos after the CSV import: ${JSON.stringify(after.body)}`);
    await page.getByLabel("Mode", { exact: true }).selectOption("insert");
    await page.getByLabel("File", { exact: true }).setInputFiles({
      name: "bad.jsonl",
      mimeType: "application/x-ndjson",
      buffer: Buffer.from('{"text":"fine","done":false}\n{"text":42,"done":false}\n'),
    });
    expectedHttpErrors++;
    await page.getByRole("button", { name: "Import", exact: true }).click();
    await page.getByTestId("import-error").getByText(/Nothing was imported/).waitFor();
    await page.getByTestId("import-error").getByText(/2:/).waitFor();

    const seed = Array.from({ length: SEED }, (_, i) =>
      JSON.stringify({ text: `seed ${String(i).padStart(4, "0")}`, done: false, image: null, completedAt: null }),
    ).join("\n");
    await page.getByLabel("File", { exact: true }).setInputFiles({ name: "seed.jsonl", mimeType: "application/x-ndjson", buffer: Buffer.from(seed) });
    await page.getByRole("button", { name: "Import", exact: true }).click();
    await page.getByTestId("import-result").getByText(`${SEED} documents imported into`).waitFor({ timeout: 30_000 });
    await shot(page, "import");
  });

  // 12. The virtualized grid: scroll through every row (pages load as it scrolls), few rows in the DOM, live updates,
  // the quick filter, CSV export from the grid.
  await step("virtualized grid over the seeded rows", async () => {
    await nav(page, "Data").click();
    await page.getByRole("link", { name: /^todos/ }).click();
    const grid = page.getByTestId("document-grid");
    await grid.waitFor();
    const tables = await call(base, "query", "_system/dashboard:tables", {}, admin);
    const total = (tables.body.value as { name: string; documents: number }[]).find((t) => t.name === "todos")!.documents;
    if (total < SEED) throw new Error(`only ${total} todos`);
    const scroller = grid.locator("xpath=..");
    const status = page.getByTestId("grid-status");
    const started = Date.now();
    while (!((await status.textContent()) ?? "").includes("end of results")) {
      if (Date.now() - started > 30_000) throw new Error(`stuck at: ${await status.textContent()}`);
      await scroller.evaluate((el) => (el.scrollTop = el.scrollHeight));
      await Bun.sleep(100);
    }
    await status.getByText(`${total} shown · end of results`).waitFor();
    await page.waitForFunction((n) => document.querySelector("[data-testid=document-grid] tbody tr[data-row]:last-child")?.getAttribute("data-row") === String(n - 1), total);
    const rendered = await grid.locator("tbody tr[data-row]").count();
    if (rendered > 120) throw new Error(`${rendered} rows in the DOM: not virtualized`);
    // The middle, then back to the top, where a new todo appears live.
    await scroller.evaluate((el) => (el.scrollTop = 32 * 1000));
    await grid.locator('tbody tr[data-row="1000"]').waitFor();
    await shot(page, "data-virtual");
    // A write (which re-sends the table list) leaves the scroll position alone.
    const marker = `while scrolled ${Date.now()}`;
    await call(base, "mutation", "todos:add", { text: marker });
    await page.getByTestId("document-grid").locator("tbody tr").first().waitFor();
    await Bun.sleep(300);
    const kept = await scroller.evaluate((el) => el.scrollTop);
    if (Math.abs(kept - 32 * 1000) > 1) throw new Error(`the grid scrolled to ${kept} after a write`);
    await scroller.evaluate((el) => (el.scrollTop = 0));
    const fresh = `live while virtualized ${Date.now()}`;
    await call(base, "mutation", "todos:add", { text: fresh });
    await grid.getByText(fresh).waitFor({ timeout: 5000 });
    // The quick filter matches loaded rows only, client-side.
    await page.getByLabel("Filter loaded rows").fill("seed 1234");
    await status.getByText(/^1 of \d+ loaded rows match/).waitFor();
    await grid.getByText("seed 1234").waitFor();
    await page.getByLabel("Filter loaded rows").fill("");
    const [csv] = await Promise.all([page.waitForEvent("download"), page.getByRole("button", { name: "⇩ CSV" }).click()]);
    const csvLines = readFileSync((await csv.path())!, "utf8").trim().split("\r\n");
    if (csvLines.length !== total + 3) throw new Error(`grid CSV export has ${csvLines.length} lines, want ${total + 3}`); // header, two new todos
  });

  // 13. The search box (the todos' search index), with a filter field.
  await step("search box", async () => {
    await page.getByRole("button", { name: "Search", exact: true }).click();
    await page.getByTestId("search-info").getByText(/search_text · unicode61/).waitFor();
    await page.getByLabel("Search documents").fill("seed 1999");
    const grid = page.getByTestId("document-grid");
    await grid.getByText("seed 1999").waitFor();
    if ((await grid.locator("tbody tr[data-row]").count()) !== 1) throw new Error("expected exactly one result");
    await page.getByTestId("grid-status").getByText("1 shown · end of results").waitFor();
    await shot(page, "search");
    await page.getByLabel("Search filter done").fill("true");
    await page.getByText("No documents match this search").waitFor();
    await page.getByLabel("Search filter done").fill("");
    await page.getByRole("button", { name: "Browse", exact: true }).click();
    await page.getByRole("button", { name: "Schema" }).click();
    await page.getByTestId("search-indexes").getByText(/tokenizer unicode61/).waitFor();
    await page.getByRole("button", { name: "Schema" }).click();
  });

  // 14. A migration: pending, a dry run with before/after examples, then a real run to done.
  await step("migrations: dry run and run", async () => {
    const padded = await call(
      base,
      "mutation",
      "_system/dashboard:insert_document",
      { table: "todos", document: { text: "  padded todo  ", done: false, image: null, completedAt: null } },
      admin,
    );
    if (padded.status !== 200) throw new Error(`insert failed: ${JSON.stringify(padded.body)}`);
    await nav(page, "Migrations").click();
    await page.getByRole("heading", { name: "Migrations" }).waitFor();
    const row = page.getByTestId("migrations").locator("tbody tr").filter({ hasText: "0001_trim_todo_text" });
    await row.getByText("pending", { exact: true }).waitFor();
    await row.getByRole("button", { name: "Dry run" }).click();
    const report = page.getByTestId("dry-run-report");
    await report.getByText("would complete").waitFor({ timeout: 15_000 });
    await report.getByText(/^1$/).first().waitFor();
    await report.getByText("padded todo").first().waitFor();
    await shot(page, "migrations-dry-run");
    await page.keyboard.press("Escape");
    await report.waitFor({ state: "detached" });
    await row.getByRole("button", { name: "Run", exact: true }).click();
    await row.getByText("done", { exact: true }).waitFor({ timeout: 20_000 });
    const bar = row.getByRole("progressbar");
    if ((await bar.getAttribute("aria-valuenow")) !== (await bar.getAttribute("aria-valuemax"))) throw new Error("progress is not complete");
    const doc = await call(base, "query", "_system/dashboard:document", { id: padded.body.value }, admin);
    if (doc.body.value.document.text !== "padded todo") throw new Error(`not migrated: ${JSON.stringify(doc.body.value)}`);
    await shot(page, "migrations");
  });

  // 15. Per-function latency charts from the execution log.
  await step("function latency charts", async () => {
    await nav(page, "Functions").click();
    await page.getByRole("link", { name: /add$/ }).first().click();
    await page.getByRole("heading", { name: "todos:add" }).waitFor();
    await page.waitForFunction(() => (document.querySelector("[data-testid=chart-latency] path.series")?.getAttribute("d") ?? "").startsWith("M"));
    await page.getByTestId("chart-volume").locator("rect.bar").first().waitFor();
    const totals = (await page.getByTestId("activity-totals").textContent()) ?? "";
    if (!/[1-9]\d* calls/.test(totals)) throw new Error(`totals: ${totals}`);
    const svg = page.getByTestId("chart-latency").locator("svg");
    const box = (await svg.boundingBox())!;
    await svg.hover({ position: { x: box.width - 20, y: 60 } });
    await page.getByTestId("chart-latency").getByRole("status").getByText(/calls/).waitFor();
    await page.getByTestId("activity-totals").scrollIntoViewIfNeeded();
    await shot(page, "functions-chart");
  });

  // 16. "Open app as user": the endpoint, then the button (a new tab at the app with the session in the fragment),
  // the impersonation session listed and revoked live.
  await step("open app as user, live session revocation", async () => {
    const users = await call(base, "query", "_system/dashboard:users", { paginationOpts: { numItems: 10, cursor: null } }, admin);
    const userId = (users.body.value.page as { _id: string; email: string }[]).find((u) => u.email === "ada@example.com")!._id;
    const imp = await fetch(`${base}/api/admin/impersonate`, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${admin}` },
      body: JSON.stringify({ user: userId, ttlSeconds: 300, redirectTo: `${base}/` }),
    }).then((r) => r.json() as Promise<{ url: string; expiresAt: number; userId: string }>);
    if (!imp.url?.startsWith(`${base}/#rxferrix_access_token=`) || Math.abs(imp.expiresAt - (Date.now() + 300_000)) > 30_000) {
      throw new Error(`impersonate: ${JSON.stringify({ ...imp, url: imp.url?.slice(0, 40) })}`);
    }
    await nav(page, "Users").click();
    const row = page.getByRole("row").filter({ hasText: "ada@example.com" });
    await row.getByRole("button", { name: "Open app as user" }).click();
    const dialog = page.getByRole("dialog");
    await dialog.getByText("all of this user's rights").waitFor();
    await dialog.getByRole("combobox", { name: "App URL" }).selectOption(`${base}/api/health`);
    await shot(page, "impersonate");
    const [tab] = await Promise.all([
      page.context().waitForEvent("page"),
      dialog.getByRole("button", { name: "Open app as user" }).click(),
    ]);
    const opened = () => tab.url().startsWith(`${base}/api/health#rxferrix_access_token=`) && tab.url().includes(`rxferrix_user_id=${userId}`);
    for (let i = 0; i < 100 && !opened(); i++) await Bun.sleep(50);
    if (!opened()) throw new Error(`the new tab is at ${tab.url().slice(0, 60)}`);
    await tab.close();
    await page.getByTestId("impersonation-banner").getByText("ada@example.com").waitFor();
    // Both impersonation sessions are listed; revoking one removes it live.
    await row.locator("td").first().click();
    const sessions = page.getByTestId("sessions");
    const imps = sessions.locator("tbody tr").filter({ hasText: "impersonated by you" });
    await page.waitForFunction(() => document.querySelectorAll("[data-testid=sessions] tbody tr").length === 2);
    if ((await imps.count()) !== 2) throw new Error(`${await imps.count()} impersonation sessions`);
    await shot(page, "users");
    await imps.first().getByRole("button", { name: "Revoke" }).click();
    await page.waitForFunction(() => document.querySelectorAll("[data-testid=sessions] tbody tr").length === 1);
  });

  // 17. The overview's live counters and the command palette / shortcuts.
  await step("overview live stats, command palette and shortcuts", async () => {
    await nav(page, "Overview").click();
    await page.getByTestId("stat-commits").waitFor();
    await page.getByTestId("stat-queue").waitFor();
    await shot(page, "overview");
    await page.keyboard.press("Control+k");
    await page.getByRole("combobox", { name: "Command" }).fill("audit");
    await shot(page, "palette");
    await page.keyboard.press("Enter");
    await page.getByRole("heading", { name: "Audit log" }).waitFor();
    await page.keyboard.press("Control+k");
    await page.getByRole("combobox", { name: "Command" }).fill("todos");
    await page.keyboard.press("Enter");
    await page.getByRole("heading", { name: "todos" }).waitFor();
    await page.keyboard.press("Alt+8");
    await page.getByRole("heading", { name: "Workflows" }).waitFor();
    await page.getByText("No workflows").waitFor();
    await page.keyboard.press("Alt+9");
    await page.getByRole("heading", { name: "Migrations" }).waitFor();
  });

  // 17b. A phone-sized window: the sidebar is a drawer behind the top bar, master/detail pages show one pane.
  await step("phone layout: drawer, function list and detail, table picker", async () => {
    await page.setViewportSize({ width: 390, height: 844 });
    try {
      const menu = page.getByRole("button", { name: "Menu" });
      await nav(page, "Functions").waitFor({ state: "hidden", timeout: 2000 }); // the drawer, closed
      await menu.click();
      await nav(page, "Functions").click();
      await page.getByRole("searchbox", { name: "Search functions" }).or(page.getByLabel("Search functions")).waitFor();
      if ((await menu.getAttribute("aria-expanded")) !== "false") throw new Error("the drawer stayed open after navigating");
      await page.getByRole("link", { name: /add$/ }).first().click();
      await page.getByRole("heading", { name: "todos:add" }).waitFor();
      if (await page.getByLabel("Search functions").isVisible()) throw new Error("the function list is shown next to the function");
      await shot(page, "phone-function");
      await page.getByRole("link", { name: "← Functions" }).click();
      await page.getByLabel("Search functions").waitFor();
      await menu.click();
      await nav(page, "Data").click();
      await page.getByRole("combobox", { name: "Table" }).selectOption("_users");
      await page.getByRole("heading", { name: "_users" }).waitFor();
      await shot(page, "phone-data");
      const width = await page.evaluate(() => document.documentElement.scrollWidth);
      if (width > 390) throw new Error(`the page is ${width}px wide on a 390px screen`);
    } finally {
      await page.setViewportSize({ width: 1360, height: 820 });
    }
  });

  // 17c. Environment variables: add, reveal, edit, paste a .env, remove; values masked in the data browser.
  await step("environment variables: add, reveal, edit, paste a .env, remove", async () => {
    await nav(page, "Environment").click();
    await page.getByRole("heading", { name: "Environment" }).waitFor();
    await page.getByLabel("Name").fill("E2E_KEY");
    await page.getByLabel("Value", { exact: true }).fill("first value");
    await page.getByRole("button", { name: "Add" }).click();
    const row = page.locator("[data-env=E2E_KEY]");
    await row.waitFor();
    if (await row.getByText("first value").count()) throw new Error("the value is shown before it is revealed");
    await row.getByRole("button", { name: "Show E2E_KEY" }).click();
    await row.getByText("first value").waitFor();
    await row.getByRole("button", { name: "Edit E2E_KEY" }).click();
    await row.getByLabel("Value of E2E_KEY").fill("second value");
    await row.getByRole("button", { name: "Save" }).click();
    await row.getByText("second value").waitFor();
    const list = await call(base, "query", "_system/env:list", {}, admin);
    if ((list.body.value as { name: string; value: string }[]).find((v) => v.name === "E2E_KEY")?.value !== "second value") {
      throw new Error(`env:list: ${JSON.stringify(list.body)}`);
    }
    await page.getByRole("button", { name: "Paste .env…" }).click();
    await page.getByLabel(".env contents").fill("# pasted\nA_ONE=1\nexport B_TWO='two'");
    await page.getByRole("button", { name: "Set 2 variables" }).click();
    await page.locator("[data-env=B_TWO]").waitFor();
    await shot(page, "environment");
    // The data browser shows the sealed value masked.
    await page.goto(`${base}/_/data/_env`);
    await page.getByTestId("document-grid").getByText("E2E_KEY").waitFor();
    if (await page.getByTestId("document-grid").getByText("second value").count()) throw new Error("the data browser shows a value");
    await nav(page, "Environment").click();
    await row.getByRole("button", { name: "Remove E2E_KEY" }).click();
    await row.waitFor({ state: "detached" });
  });

  // 18. Private storage: list thumbnails and downloads use the signed URLs the API returns.
  await step("private storage: signed thumbnails and downloads", async () => {
    const pdir = tempDir();
    const made = Bun.spawnSync([bin, "admin", "create", EMAIL, "--password", PASSWORD, "--dir", pdir]);
    if (made.exitCode !== 0) throw new Error(`admin create failed: ${made.stderr.toString()}`);
    const ps = await startServer(pdir, () => ["--storage-private"]);
    const png = Buffer.from(
      "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==",
      "base64",
    );
    const upload = await call(ps.base, "mutation", "todos:generate_upload_url", {});
    const stored = await fetch(upload.body.value as string, { method: "POST", headers: { "content-type": "image/png" }, body: png });
    const { storageId } = (await stored.json()) as { storageId: string };
    if ((await fetch(`${ps.base}/api/storage/${storageId}`)).status !== 403) throw new Error("private storage served an unsigned URL");
    const p2 = await newPage(browser!);
    await signIn(p2, ps.base);
    await nav(p2, "Files").click();
    const thumb = p2.locator("[data-testid=files] img[data-testid=thumb]").first();
    await p2.waitForFunction(() => {
      const i = document.querySelector<HTMLImageElement>("[data-testid=files] img[data-testid=thumb]");
      return i !== null && i.complete && i.naturalWidth === 96;
    });
    const src = (await thumb.getAttribute("src"))!;
    if (!/^\/api\/storage\/\w+\?exp=\d+&sig=[\w-]+&thumb=96x96&fit=cover$/.test(src)) throw new Error(`thumb src: ${src}`);
    const href = (await p2.getByTestId("files").getByRole("link", { name: "Download" }).first().getAttribute("href"))!;
    if (!/^\/api\/storage\/\w+\?exp=\d+&sig=[\w-]+&download=1$/.test(href)) throw new Error(`download href: ${href}`);
    const res = await fetch(`${ps.base}${href}`);
    if (res.status !== 200 || !res.headers.get("content-disposition")?.startsWith("attachment")) {
      throw new Error(`signed download: ${res.status} ${res.headers.get("content-disposition")}`);
    }
    await shot(p2, "files-private");
    await p2.close();
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
  // Staged restore on a server of its own (it restarts): a corrupt upload is refused, a stored backup is checked and
  // staged, canceled, staged again, then "Restart now": the server exits with --restart-exit-code, comes back (here
  // started again by the test, as a supervisor would) with the backup's data.
  await step("staged restore: refuse, stage, cancel, restart", async () => {
    const rdir = tempDir();
    const made = Bun.spawnSync([bin, "admin", "create", EMAIL, "--password", PASSWORD, "--dir", rdir]);
    if (made.exitCode !== 0) throw new Error(`admin create failed: ${made.stderr.toString()}`);
    const rs = await startServer(rdir, () => ["--restart-exit-code", "75"]);
    const token = await adminToken(rs.base);
    const auth = { authorization: `Bearer ${token}` };
    await call(rs.base, "mutation", "todos:add", { text: "in the backup" });
    const backup = (await fetch(`${rs.base}/api/admin/backups`, { method: "POST", headers: auth }).then((r) => r.json())) as { name: string };
    await call(rs.base, "mutation", "todos:add", { text: "after the backup" });

    const p = await newPage(browser!);
    await signIn(p, rs.base);
    await nav(p, "Backups").click();
    await p.getByRole("heading", { name: "Backups and data" }).waitFor();
    // A corrupt archive from this computer: refused with the reason, nothing staged.
    expectedHttpErrors++;
    await p.getByLabel("Restore an archive from this computer:").setInputFiles({
      name: "rxferrix-broken.tar.zst",
      mimeType: "application/zstd",
      buffer: Buffer.from([0x28, 0xb5, 0x2f, 0xfd, 1, 2, 3, 4, 5]),
    });
    await p.getByTestId("restore-error").getByText(/Not restored: This backup can't be restored/).waitFor({ timeout: 20_000 });
    if (await p.getByTestId("pending-restore").count()) throw new Error("a corrupt archive was staged");

    // The stored backup: checked, staged, shown as pending; the data is unchanged meanwhile.
    const row = p.getByTestId("backups").locator("tbody tr").filter({ hasText: backup.name });
    await row.getByRole("button", { name: "Restore" }).click();
    const banner = p.getByTestId("pending-restore");
    await banner.getByText(/Restore pending:/).waitFor({ timeout: 30_000 });
    await banner.getByText(backup.name).waitFor();
    await shot(p, "restore-pending");
    if ((await row.getByRole("button", { name: "Restore" }).isEnabled())) throw new Error("Restore stays enabled while one is pending");
    // Cancel, then stage again.
    await banner.getByRole("button", { name: "Cancel restore" }).click();
    await banner.waitFor({ state: "detached" });
    await row.getByRole("button", { name: "Restore" }).click();
    await banner.getByText(/Restore pending:/).waitFor({ timeout: 30_000 });

    // Restart now: the server exits with 75; the supervisor (the test) starts it again on the same port. Meanwhile
    // the page sees refused connections, then its session (not in the backup) is refused.
    tolerated = /ERR_CONNECTION_REFUSED|ERR_EMPTY_RESPONSE|WebSocket|status of 40[13]/;
    await banner.getByRole("button", { name: "Restart now" }).click();
    await p.getByTestId("restarting").waitFor();
    const code = await Promise.race([rs.exited, Bun.sleep(30_000).then(() => -1)]);
    if (code !== 75) throw new Error(`exit code ${code}, expected 75:\n${rs.output()}`);
    const again = await startServer(rdir, () => ["--restart-exit-code", "75"], rs.port);
    const list = await call(again.base, "query", "todos:list", {});
    const texts = (list.body.value as { text: string }[]).map((t) => t.text);
    if (!texts.includes("in the backup") || texts.includes("after the backup")) throw new Error(`after the restore: ${texts}`);
    // The page reloads by itself once the server answers again; the restored database has the admin (made before
    // the backup) but not this browser's session (made after it): sign in again.
    await p.getByLabel("Email").waitFor({ timeout: 30_000 });
    await signIn(p, again.base);
    await nav(p, "Backups").click();
    await p.getByRole("heading", { name: "Backups and data" }).waitFor();
    await p.getByTestId("backups").waitFor();
    if (await p.getByTestId("pending-restore").count()) throw new Error("still pending after the restart");
    tolerated = null;
    await p.close();
  });

  // ---------------------------------------------------------------------------------------------------------------
  // Continuous replication on a server of its own (it restarts): the status card (target, healthy, caught up, last
  // segment and snapshot), then a point-in-time restore picked by commit ts: checked, staged, "Restart now", and the
  // server comes back with the data of that point; the restored database ships as a new generation.
  await step("replication: status and a point-in-time restore", async () => {
    const rdir = tempDir();
    const replica = tempDir();
    const made = Bun.spawnSync([bin, "admin", "create", EMAIL, "--password", PASSWORD, "--dir", rdir]);
    if (made.exitCode !== 0) throw new Error(`admin create failed: ${made.stderr.toString()}`);
    const args = () => ["--replicate-to", replica, "--replicate-every", "1s", "--restart-exit-code", "75"];
    const rs = await startServer(rdir, args);
    const token = await adminToken(rs.base);
    const auth = { authorization: `Bearer ${token}` };
    const status = async (base: string, t = token) =>
      (await fetch(`${base}/api/admin/replication`, { headers: { authorization: `Bearer ${t}` } }).then((r) => r.json())) as {
        enabled: boolean;
        lagCommits: number;
        shippedTs: number;
        generation: string | null;
        generations: unknown[];
      };
    const added = await call(rs.base, "mutation", "todos:add", { text: "before the point" });
    const point = added.body.ts as number;
    for (let i = 0; i < 100 && (await status(rs.base)).shippedTs < point; i++) await Bun.sleep(100);
    if ((await status(rs.base)).shippedTs < point) throw new Error("the point was never shipped");
    await call(rs.base, "mutation", "todos:add", { text: "after the point" });
    const firstGeneration = (await status(rs.base)).generation;

    const p = await newPage(browser!);
    await signIn(p, rs.base);
    await nav(p, "Backups").click();
    const card = p.getByTestId("replication");
    await card.getByTestId("replication-target").getByText(replica).waitFor();
    await card.getByTestId("replication-health").getByText("healthy").waitFor({ timeout: 10_000 });
    await card.getByTestId("replication-lag").getByText(/caught up/).waitFor({ timeout: 10_000 });
    await card.getByTestId("replication-last-snapshot").getByText(/rxferrix-auto-/).waitFor();
    await card.getByTestId("replication-last-segment").getByText(/shipped/).waitFor();
    await card.getByTestId("replication-generations").getByText("current").waitFor();
    await shot(p, "replication");

    // Pick the point by commit ts and restore it: checked and staged, the server keeps its data meanwhile.
    await card.getByLabel("at a commit ts").check();
    await card.getByLabel("Restore commit ts").fill(String(point));
    await card.getByRole("button", { name: "Restore to this point" }).click();
    const banner = p.getByTestId("pending-restore");
    await banner.getByText(/Restore pending:/).waitFor({ timeout: 30_000 });
    await banner.getByText(new RegExp(`point in time: ts ${point}`)).waitFor();
    await banner.getByText(/replayed commit/).waitFor();
    await shot(p, "pitr-pending");
    const listed = await fetch(`${rs.base}/api/admin/restore`, { headers: auth }).then((r) => r.json());
    if (listed.pendingRestore?.pointInTime?.ts !== point) throw new Error(`staged: ${JSON.stringify(listed)}`);

    tolerated = /ERR_CONNECTION_REFUSED|ERR_EMPTY_RESPONSE|WebSocket|status of 40[13]/;
    await banner.getByRole("button", { name: "Restart now" }).click();
    await p.getByTestId("restarting").waitFor();
    const code = await Promise.race([rs.exited, Bun.sleep(30_000).then(() => -1)]);
    if (code !== 75) throw new Error(`exit code ${code}, expected 75:\n${rs.output()}`);
    const again = await startServer(rdir, args, rs.port);
    const list = await call(again.base, "query", "todos:list", {});
    const texts = (list.body.value as { text: string }[]).map((t) => t.text);
    if (!texts.includes("before the point") || texts.includes("after the point")) throw new Error(`after the restore: ${texts}`);
    // This browser's session was made after the point: sign in again. The restored database replicates as a new
    // generation next to the first one.
    await p.getByLabel("Email").waitFor({ timeout: 30_000 });
    await signIn(p, again.base);
    await nav(p, "Backups").click();
    await p.getByTestId("replication-generations").waitFor();
    tolerated = null;
    const fresh = await adminToken(again.base);
    for (let i = 0; i < 100 && (await status(again.base, fresh)).generations.length < 2; i++) await Bun.sleep(100);
    const after = await status(again.base, fresh);
    if (after.generations.length !== 2 || after.generation === firstGeneration) throw new Error(`generations: ${JSON.stringify(after)}`);
    await p.getByTestId("replication-generations").locator("tbody tr").nth(1).waitFor({ timeout: 10_000 });
    if (await p.getByTestId("pending-restore").count()) throw new Error("still pending after the restart");
    await p.close();
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

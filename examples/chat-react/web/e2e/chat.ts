// End-to-end test of the chat example: three users in separate browser contexts exercise auth (password and email
// code), live messages, reactions, edits, deletes, unread badges, attachments, search, infinite scroll, typing
// indicators, `/remind`, and private-channel authorization.
//
// Usage (from examples/chat-react/web, after `cargo build -p chat-server` and `bun run build` at the root):
//   bun e2e/chat.ts                 starts its own backend (fresh data dir) and Vite, and stops them afterwards
//   E2E_EXTERNAL=1 bun e2e/chat.ts  uses a backend and Vite that are already running (backend with --dev)
// Env: BACKEND_PORT (8097), VITE_PORT (5180), CHAT_SERVER (the binary, default ../../../target/debug/chat-server),
// CHROMIUM (/usr/bin/chromium), SCREENSHOTS (a directory to save chat-*.png into).
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { chromium, type Browser, type BrowserContext, type Locator, type Page } from "playwright-core";

const BACKEND_PORT = Number(process.env.BACKEND_PORT ?? 8097);
const VITE_PORT = Number(process.env.VITE_PORT ?? 5180);
const BACKEND = `http://127.0.0.1:${BACKEND_PORT}`;
const APP = `http://localhost:${VITE_PORT}`;
const EXTERNAL = process.env.E2E_EXTERNAL === "1";
const SHOTS = process.env.SCREENSHOTS;
const webDir = resolve(import.meta.dir, "..");
const BIN = process.env.CHAT_SERVER ?? resolve(webDir, "../../../target/debug/chat-server");

const run = Date.now().toString(36);
const failures: string[] = [];
const started: { proc: ReturnType<typeof Bun.spawn>; port: number }[] = [];
let dataDir: string | null = null;

// ---------------------------------------------------------------------------------------------------------------------
// Process management

async function waitFor(url: string, what: string) {
  for (let i = 0; i < 150; i++) {
    try {
      if ((await fetch(url)).ok) return;
    } catch {}
    await Bun.sleep(100);
  }
  throw new Error(`${what} did not start (${url})`);
}

async function startServers() {
  dataDir = mkdtempSync(join(tmpdir(), "chat-e2e-"));
  const server = Bun.spawn(
    [BIN, "serve", "--dev", "--http", `127.0.0.1:${BACKEND_PORT}`, "--dir", dataDir],
    { stdout: "ignore", stderr: "ignore" },
  );
  started.push({ proc: server, port: BACKEND_PORT });
  await waitFor(`${BACKEND}/api/health`, "backend");
  const vite = Bun.spawn(["bun", "x", "vite", "--port", String(VITE_PORT), "--strictPort"], {
    cwd: webDir,
    env: { ...process.env, FERRIX_URL: BACKEND },
    stdout: "ignore",
    stderr: "ignore",
  });
  started.push({ proc: vite, port: VITE_PORT });
  await waitFor(APP, "vite");
}

/** Stops what we started: the spawned process, then anything still listening on its port (Vite's node child). */
async function stopServers() {
  for (const { proc, port } of started.reverse()) {
    proc.kill();
    await proc.exited;
    const out = Bun.spawnSync(["ss", "-ltnpH", `sport = :${port}`]).stdout.toString();
    for (const [, pid] of out.matchAll(/pid=(\d+)/g)) {
      try {
        process.kill(Number(pid));
      } catch {}
    }
  }
  if (dataDir) rmSync(dataDir, { recursive: true, force: true });
}

// ---------------------------------------------------------------------------------------------------------------------
// Helpers

async function timed<T>(label: string, f: () => Promise<T>): Promise<T> {
  const start = performance.now();
  const out = await f();
  console.log(`  ${label}: ${(performance.now() - start).toFixed(0)}ms`);
  return out;
}

async function shot(page: Page, name: string) {
  if (SHOTS) await page.screenshot({ path: join(SHOTS, `chat-${name}.png`) });
}

async function newUser(browser: Browser): Promise<{ ctx: BrowserContext; page: Page }> {
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const page = await ctx.newPage();
  const tag = `[page ${browser.contexts().length}]`;
  page.on("pageerror", (e) => failures.push(`${tag} page error: ${e.message}`));
  page.on("console", (m) => {
    if (m.type() === "error") failures.push(`${tag} console error: ${m.text()}`);
  });
  await page.goto(APP);
  return { ctx, page };
}

async function signUp(page: Page, name: string, email: string) {
  await page.getByRole("tab", { name: "Sign up" }).click();
  await page.getByLabel("Name").fill(name);
  await page.getByLabel("Email").fill(email);
  await page.getByLabel("Password").fill("correct horse battery");
  await page.getByRole("button", { name: "Create account" }).click();
  await page.getByTestId("me").filter({ hasText: name }).waitFor({ timeout: 10000 });
  await page.getByTestId("connection").filter({ hasText: "Live" }).waitFor();
}

const message = (page: Page, text: string): Locator => page.getByTestId("message").filter({ hasText: text });

async function send(page: Page, text: string) {
  await page.getByLabel("Message", { exact: true }).fill(text);
  await page.getByLabel("Message", { exact: true }).press("Enter");
}

async function openChannel(page: Page, name: string) {
  await page.locator(`.sidebar [data-channel="${name}"]`).click();
  await page.locator(".channel-head h2", { hasText: name }).waitFor();
}

/** The HTTP function API, called as the user signed in on `page` (its access token from localStorage). */
async function callAs(page: Page, kind: "query" | "mutation" | "action", path: string, args: unknown) {
  const token = await page.evaluate(() => {
    for (let i = 0; i < localStorage.length; i++) {
      const key = localStorage.key(i)!;
      if (key.startsWith("ferrix.auth:")) return JSON.parse(localStorage.getItem(key)!).accessToken as string;
    }
    return null;
  });
  const res = await fetch(`${BACKEND}/api/${kind}`, {
    method: "POST",
    headers: { "content-type": "application/json", ...(token ? { authorization: `Bearer ${token}` } : {}) },
    body: JSON.stringify({ path, args }),
  });
  return { status: res.status, body: (await res.json()) as { value?: unknown; errorData?: { code: string } | null } };
}

/** A 480x320 PNG with a gradient, drawn in the page. */
async function makePng(page: Page): Promise<Buffer> {
  const dataUrl = await page.evaluate(() => {
    const c = document.createElement("canvas");
    c.width = 480;
    c.height = 320;
    const g = c.getContext("2d")!;
    const grad = g.createLinearGradient(0, 0, 480, 320);
    grad.addColorStop(0, "#6d4aff");
    grad.addColorStop(1, "#22d3ee");
    g.fillStyle = grad;
    g.fillRect(0, 0, 480, 320);
    g.fillStyle = "white";
    g.font = "bold 48px sans-serif";
    g.fillText("ferrix ⚡", 120, 175);
    return c.toDataURL("image/png");
  });
  return Buffer.from(dataUrl.split(",")[1]!, "base64");
}

// ---------------------------------------------------------------------------------------------------------------------
// The test

if (!EXTERNAL) await startServers();
const browser = await chromium.launch({ executablePath: process.env.CHROMIUM ?? "/usr/bin/chromium" });
try {
  const ann = { name: `Ann ${run}`, email: `ann-${run}@example.com` };
  const bob = { name: `Bob ${run}`, email: `bob-${run}@example.com` };
  const cat = { email: `cat-${run}@example.com` };
  const room = `e2e-${run}`;

  console.log("auth: two users sign up with passwords");
  const A = await newUser(browser);
  await A.page.getByRole("tab", { name: "Sign up" }).waitFor();
  await shot(A.page, "auth");
  await signUp(A.page, ann.name, ann.email);
  const B = await newUser(browser);
  await signUp(B.page, bob.name, bob.email);
  const a = A.page;
  const b = B.page;

  console.log("channels: Ann creates one, Bob finds it live and joins");
  await a.getByRole("button", { name: "New channel" }).click();
  await a.getByLabel("Channel name").fill(room);
  await a.getByLabel("Topic").fill("Where the e2e test hangs out");
  await a.getByRole("button", { name: "Create", exact: true }).click();
  await a.locator(".channel-head h2", { hasText: room }).waitFor();
  await timed("new channel -> in Bob's sidebar", () => b.locator(`.sidebar .browse [data-channel="${room}"]`).waitFor());
  await openChannel(b, room);
  await b.getByRole("button", { name: `Join #${room}` }).click();
  await b.getByLabel("Message", { exact: true }).waitFor();
  await a.getByRole("button", { name: "Members" }).filter({ hasText: "2" }).waitFor();

  console.log("messages: live delivery, optimistic send");
  const hello = `Hello Bob, the kumquat${run} report is ready`;
  await send(a, hello);
  await timed("send in A -> visible in A (optimistic)", () => message(a, hello).waitFor({ timeout: 1000 }));
  await timed("send in A -> visible in B", () => message(b, hello).waitFor({ timeout: 3000 }));
  await message(a, hello).and(a.locator(":not(.pending)")).waitFor();

  console.log("reactions: Bob reacts, Ann sees it live");
  await message(b, hello).hover();
  await message(b, hello).getByRole("button", { name: "Add reaction" }).click();
  await message(b, hello).getByRole("button", { name: "React with 🎉" }).click();
  await timed("react in B -> pressed in B", () =>
    message(b, hello).locator('.reaction[aria-pressed="true"]', { hasText: "🎉" }).waitFor({ timeout: 1000 }),
  );
  await timed("react in B -> visible in A", () =>
    message(a, hello).getByRole("button", { name: "🎉 1" }).waitFor({ timeout: 3000 }),
  );
  // Ann adds hers: the count goes to 2 for both.
  await message(a, hello).getByRole("button", { name: "🎉 1" }).click();
  await message(b, hello).getByRole("button", { name: "🎉 2" }).waitFor({ timeout: 3000 });

  console.log("edit and delete");
  const edited = `${hello} (now with charts)`;
  await message(a, hello).hover();
  await message(a, hello).getByRole("button", { name: "Edit" }).click();
  await a.getByLabel("Edit message").fill(edited);
  await a.getByLabel("Edit message").press("Enter");
  await timed("edit in A -> visible in B", () => message(b, "now with charts").filter({ hasText: "(edited)" }).waitFor({ timeout: 3000 }));
  // Bob can't edit Ann's message: no Edit button for him.
  await message(b, edited).hover();
  if (await message(b, edited).getByRole("button", { name: "Edit" }).count()) failures.push("Bob sees Edit on Ann's message");
  const doomed = `this message will self-destruct ${run}`;
  await send(a, doomed);
  await message(b, doomed).waitFor({ timeout: 3000 });
  await message(a, doomed).hover();
  await message(a, doomed).getByRole("button", { name: "Delete", exact: true }).click();
  await message(a, doomed).getByRole("button", { name: "Delete for everyone" }).click();
  await timed("delete in A -> gone in B", () => message(b, doomed).waitFor({ state: "detached", timeout: 3000 }));

  console.log("unread: Bob reads #general while Ann posts in the room");
  await openChannel(b, "general");
  await send(a, `First unread ${run}`);
  await send(a, `Second unread ${run}`);
  const badge = b.getByTestId(`unread-${room}`);
  await timed("2 messages in A -> badge 2 in B", () => badge.filter({ hasText: /^2$/ }).waitFor({ timeout: 3000 }));
  await Bun.sleep(300); // let the badge's pop-in animation finish
  await shot(b, "unread");
  await openChannel(b, room);
  await timed("open channel in B -> badge cleared", () => badge.waitFor({ state: "detached", timeout: 3000 }));
  await message(b, `Second unread ${run}`).waitFor();

  console.log("attachments: upload with progress, shown to Bob");
  const png = await makePng(a);
  await a.getByTestId("file-input").setInputFiles({ name: "chart.png", mimeType: "image/png", buffer: png });
  await a.getByRole("button", { name: "Send" }).and(a.locator(":enabled")).waitFor({ timeout: 5000 });
  const caption = `Here is the chart ${run}`;
  await send(a, caption);
  await timed("attachment in A -> image in B", () => message(b, caption).locator(".attachment img").waitFor({ timeout: 5000 }));
  await b.waitForFunction(
    (text) =>
      [...document.querySelectorAll<HTMLElement>("[data-testid=message]")].some(
        (m) => m.textContent?.includes(text) && [...m.querySelectorAll("img")].some((i) => i.complete && i.naturalWidth === 480),
      ),
    caption,
    { timeout: 5000 },
  );
  // In --dev without --public-url, file URLs are root-relative, so they load through Vite's /api proxy.
  const src = (await message(b, caption).locator(".attachment img").getAttribute("src")) ?? "";
  if (!src.startsWith("/api/storage/")) failures.push(`expected a root-relative image URL, got ${src}`);
  await shot(b, "attachment");

  console.log("search: finds the message, and new matches live");
  await b.getByRole("button", { name: "Search messages" }).click();
  await b.getByLabel("Search this channel").fill(`kumquat${run.slice(0, -2)}`); // prefix of the last word
  await timed("search -> result", () => b.getByTestId("search-result").filter({ hasText: "now with charts" }).waitFor({ timeout: 3000 }));
  await send(a, `Another kumquat${run} sighting`);
  await timed("matching message in A -> in B's results", () =>
    b.getByTestId("search-result").filter({ hasText: "sighting" }).waitFor({ timeout: 3000 }),
  );
  await shot(b, "search");
  await b.getByRole("button", { name: "Close search" }).click();

  console.log("typing: indicator appears, then expires on the scheduler");
  await a.getByLabel("Message", { exact: true }).fill("I am typing something long");
  const typing = b.getByTestId("typing").filter({ hasText: `${ann.name} is typing` });
  await timed("typing in A -> indicator in B", () => typing.waitFor({ timeout: 3000 }));
  await shot(b, "typing");
  await timed("indicator expires (no keystrokes)", () => typing.waitFor({ state: "detached", timeout: 10000 }));
  await a.getByLabel("Message", { exact: true }).fill("");

  console.log("/remind: scheduled message arrives");
  await send(a, "/remind me in 3 seconds to check the oven");
  await a.getByTestId("notice").filter({ hasText: "remind you in 3 seconds" }).waitFor({ timeout: 3000 });
  const t0 = performance.now();
  await message(b, "check the oven").and(b.locator(".reminder")).waitFor({ timeout: 15000 });
  const waited = performance.now() - t0;
  console.log(`  reminder arrived after ${waited.toFixed(0)}ms`);
  if (waited < 1500) failures.push(`the reminder came too early (${waited}ms)`);
  await message(a, "check the oven").waitFor({ timeout: 3000 });
  await shot(a, "reminder");

  console.log("infinite scroll: seed 200 messages, scroll up to the first");
  await send(a, "/seed 200");
  await a.getByTestId("notice").filter({ hasText: "Added 200 sample messages" }).waitFor({ timeout: 15000 });
  const first = message(b, "Sample message #1 of 200");
  await message(b, "Sample message #200 of 200").waitFor({ timeout: 5000 });
  const initial = await b.getByTestId("message").count();
  if ((await first.count()) > 0) failures.push(`the oldest seeded message was loaded before scrolling (${initial} messages)`);
  console.log(`  messages before scrolling: ${initial}`);
  await timed("scroll to the top -> oldest message", async () => {
    // Time-based, not a number of scrolls: on a slow CI runner each page takes longer to load and render.
    const deadline = performance.now() + 20_000;
    while ((await first.count()) === 0 && performance.now() < deadline) {
      await b.getByTestId("messages").evaluate((el) => el.scrollTo({ top: -el.scrollHeight }));
      await Bun.sleep(150);
    }
    if ((await first.count()) === 0) {
      // Say where it stopped: still loading, waiting for a scroll, or showing the button.
      const state = await b.getByTestId("messages").evaluate((el) => ({
        scrollTop: Math.round(el.scrollTop),
        scrollHeight: el.scrollHeight,
        clientHeight: el.clientHeight,
        edge: el.querySelector(".history-edge")?.textContent?.trim(),
      }));
      const count = await b.getByTestId("message").count();
      await shot(b, "scroll-stuck");
      throw new Error(`the oldest message never loaded: ${count} messages after 20 s, ${JSON.stringify(state)}`);
    }
  });
  const loaded = await b.getByTestId("message").count();
  console.log(`  messages loaded: ${initial} -> ${loaded}`);
  if (loaded < 200) failures.push(`expected 200+ messages after scrolling, got ${loaded}`);
  await b.getByTestId("channel-start").waitFor({ timeout: 3000 });
  await b.getByTestId("messages").evaluate((el) => el.scrollTo({ top: -el.scrollHeight }));
  await shot(b, "history");
  // New messages still arrive at the bottom of a fully loaded history.
  await send(a, `After the history ${run}`);
  await message(b, `After the history ${run}`).waitFor({ timeout: 3000 });

  console.log("auth: a third user signs in with an emailed code");
  const C = await newUser(browser);
  const c = C.page;
  await c.getByRole("tab", { name: "Email code" }).click();
  await c.getByLabel("Email").fill(cat.email);
  await c.getByRole("button", { name: "Email me a code" }).click();
  await c.getByLabel("Code").waitFor();
  let code: string | null = null;
  for (let i = 0; i < 50 && !code; i++) {
    code = (await callAs(c, "action", "dev:last_code", { email: cat.email })).body.value as string | null;
    if (!code) await Bun.sleep(100);
  }
  if (!code) throw new Error("no sign-in code in the dev inbox");
  await c.getByLabel("Code").fill(code);
  await c.getByRole("button", { name: "Verify code" }).click();
  await c.getByTestId("me").filter({ hasText: `cat-${run}` }).waitFor({ timeout: 10000 });

  console.log("authorization: a private channel is closed to non-members");
  const secret = `secret-${run}`;
  await a.getByRole("button", { name: "New channel" }).click();
  await a.getByLabel("Channel name").fill(secret);
  await a.getByLabel("Private").check();
  await a.getByRole("button", { name: "Create", exact: true }).click();
  await a.locator(".channel-head h2", { hasText: secret }).waitFor();
  await send(a, `classified ${run}`);
  await message(a, `classified ${run}`).and(a.locator(":not(.pending)")).waitFor();
  const secretId = new URL(a.url()).hash.replace("#/c/", "");
  if (await c.locator(`.sidebar [data-channel="${secret}"]`).count()) failures.push("the private channel is listed for Cat");
  await c.goto(`${APP}/#/c/${secretId}`);
  const denied = c.getByRole("alert").filter({ hasText: "This channel is private" });
  await timed("non-member opens the private channel -> error shown", () => denied.waitFor({ timeout: 5000 }));
  if ((await c.locator('[data-code="not_a_member"]').count()) !== 1) failures.push("no not_a_member error code shown");
  if (await message(c, `classified ${run}`).count()) failures.push("Cat can see a private message");
  await shot(c, "private-denied");
  const api = await callAs(c, "query", "messages:list", { channel: secretId, paginationOpts: { numItems: 10, cursor: null } });
  if (api.status !== 400 || api.body.errorData?.code !== "not_a_member") {
    failures.push(`messages:list as a non-member: expected 400 not_a_member, got ${api.status} ${JSON.stringify(api.body)}`);
  }
  // Ann invites Cat: Cat's open page turns into the channel, live.
  await a.getByRole("button", { name: "Members" }).click();
  await a.getByLabel("Invite by email").fill(cat.email);
  await a.getByRole("button", { name: "Invite", exact: true }).click();
  await timed("invite in A -> channel opens for C", () => message(c, `classified ${run}`).waitFor({ timeout: 5000 }));

  await openChannel(a, room);
  await shot(a, "main");
  await A.ctx.close();
  await B.ctx.close();
  await C.ctx.close();
} catch (e) {
  failures.push(e instanceof Error ? (e.stack ?? e.message) : String(e));
} finally {
  await browser.close();
  if (!EXTERNAL) await stopServers();
}

if (failures.length) {
  console.error("FAILED:\n" + failures.join("\n"));
  process.exit(1);
}
console.log("PASS: chat e2e");

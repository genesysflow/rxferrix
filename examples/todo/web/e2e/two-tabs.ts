// End-to-end check of the Phase 0 goal: two tabs, a change in one shows up live in the other.
// Needs the backend (todo-server serve --dev) and Vite running. Usage: bun e2e/two-tabs.ts [url]
import { chromium, type Page } from "playwright-core";

const url = process.argv[2] ?? "http://localhost:5173";
const browser = await chromium.launch({ executablePath: process.env.CHROMIUM ?? "/usr/bin/chromium" });
const failures: string[] = [];

async function open(): Promise<Page> {
  const page = await browser.newPage();
  page.on("pageerror", (e) => failures.push(`page error: ${e.message}`));
  page.on("console", (m) => m.type() === "error" && failures.push(`console error: ${m.text()}`));
  await page.goto(url);
  await page.getByText("live").waitFor({ timeout: 5000 });
  return page;
}

async function timed<T>(label: string, f: () => Promise<T>): Promise<T> {
  const start = performance.now();
  const out = await f();
  console.log(`${label}: ${(performance.now() - start).toFixed(0)}ms`);
  return out;
}

try {
  const a = await open();
  const b = await open();
  const item = `e2e item ${Date.now()}`;

  // Add in A, see it in B.
  await a.getByLabel("New todo").fill(item);
  await a.getByRole("button", { name: "Add" }).click();
  await timed("add in A -> visible in B", () => b.getByText(item).waitFor({ timeout: 2000 }));

  // Toggle in B, see it done in A (the `done` class on the <li>).
  const rowB = b.locator("li", { hasText: item });
  await rowB.getByRole("checkbox").check();
  await timed("toggle in B -> done in A", () =>
    a.locator("li.done", { hasText: item }).waitFor({ timeout: 2000 }),
  );

  // Filter in A to "open": the done item disappears there, but B (unfiltered) still shows it.
  await a.getByRole("button", { name: "open" }).click();
  await a.locator("li", { hasText: item }).waitFor({ state: "detached", timeout: 2000 });
  await b.getByText(item).waitFor({ timeout: 2000 });

  // A user error from the server is shown in the UI.
  await b.getByLabel("New todo").fill("   ");
  await b.getByRole("button", { name: "Add" }).click();
  await timed("user error shown", () => b.getByRole("alert").filter({ hasText: "needs some text" }).waitFor({ timeout: 2000 }));

  // Action (+3 samples) in B: both tabs' "remaining" counters converge on the same new value.
  const openCount = async (p: Page) => parseInt((await p.getByTestId("remaining").textContent()) ?? "", 10);
  const before = await openCount(a);
  await b.getByRole("button", { name: "+3 samples" }).click();
  const expected = `${before + 3} open`;
  await timed("action in B -> counters in A and B", () =>
    Promise.all([
      a.getByTestId("remaining").filter({ hasText: expected }).waitFor({ timeout: 2000 }),
      b.getByTestId("remaining").filter({ hasText: expected }).waitFor({ timeout: 2000 }),
    ]),
  );
  console.log(`remaining: ${expected}`);

  // Delete in A, gone in B.
  await a.getByRole("button", { name: "all" }).click();
  await a.locator("li", { hasText: item }).getByRole("button", { name: "Delete" }).click();
  await timed("delete in A -> gone in B", () =>
    b.locator("li", { hasText: item }).waitFor({ state: "detached", timeout: 2000 }),
  );
} catch (e) {
  failures.push(String(e));
} finally {
  await browser.close();
}

if (failures.length) {
  console.error("FAILED:\n" + failures.join("\n"));
  process.exit(1);
}
console.log("PASS: two-tab live sync works");

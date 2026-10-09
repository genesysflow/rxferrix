// Server restart while a tab is open: the tab goes offline, reconnects, re-subscribes, and stays live.
// Usage: bun e2e/reconnect.ts <todo-server binary> <data dir>   (Vite must be running; the backend must NOT be.)
import { chromium } from "playwright-core";

const [bin, dir] = process.argv.slice(2);
if (!bin || !dir) throw new Error("usage: bun e2e/reconnect.ts <todo-server binary> <data dir>");
const start = () => Bun.spawn([bin, "serve", "--dev", "--dir", dir], { stdout: "ignore", stderr: "ignore" });
const waitHealthy = async () => {
  for (let i = 0; i < 100; i++) {
    try { if ((await fetch("http://127.0.0.1:8090/api/health")).ok) return; } catch {}
    await Bun.sleep(100);
  }
  throw new Error("backend did not start");
};
const mutate = (path: string, args: unknown) =>
  fetch("http://127.0.0.1:8090/api/mutation", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ path, args }) });

let server = start();
await waitHealthy();
const browser = await chromium.launch({ executablePath: process.env.CHROMIUM ?? "/usr/bin/chromium" });
let ok = false;
try {
  const page = await browser.newPage();
  await page.goto("http://localhost:5173");
  await page.getByText("live").waitFor({ timeout: 5000 });

  server.kill();
  await server.exited;
  await page.getByText("offline").waitFor({ timeout: 35000 });
  console.log("tab went offline");

  server = start();
  await waitHealthy();
  await page.getByText("live").waitFor({ timeout: 35000 });
  console.log("tab reconnected");

  const item = `after restart ${Date.now()}`;
  await mutate("todos:add", { text: item });
  await page.getByText(item).waitFor({ timeout: 2000 });
  console.log("update from another client arrived after reconnect");

  // And the tab's own mutations work on the new socket.
  const own = `own after restart ${Date.now()}`;
  await page.getByLabel("New todo").fill(own);
  await page.getByRole("button", { name: "Add" }).click();
  await page.getByText(own).waitFor({ timeout: 2000 });
  ok = true;
} finally {
  await browser.close();
  server.kill();
}
console.log(ok ? "PASS: reconnect works" : "FAILED");
process.exit(ok ? 0 : 1);

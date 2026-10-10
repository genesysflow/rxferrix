import { FerrixClient } from "@ferrix/client";
import { FakeServer, flushMicrotasks } from "@ferrix/client/testing";
import { flushSync, mount, unmount } from "svelte";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { MASK, type SettingsView } from "../src/lib/api.js";
import type { ChartPoint } from "../src/lib/chart.js";
import type { Command } from "../src/lib/commands.js";
import { DASHBOARD_CONTEXT, type DashboardContext } from "../src/lib/context.js";
import CommandPalette from "../src/components/CommandPalette.svelte";
import TimeChart from "../src/components/TimeChart.svelte";
import Backups from "../src/pages/Backups.svelte";
import Settings from "../src/pages/Settings.svelte";
import Workflows from "../src/pages/Workflows.svelte";

let target: HTMLElement;
let mounted: ReturnType<typeof mount> | null = null;

beforeEach(() => {
  target = document.createElement("div");
  document.body.appendChild(target);
  // jsdom has no layout: element size bindings and scrolling are no-ops.
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      unobserve() {}
      disconnect() {}
    },
  );
  Element.prototype.scrollIntoView ??= () => {};
});

afterEach(() => {
  if (mounted) unmount(mounted);
  mounted = null;
  target.remove();
  vi.unstubAllGlobals();
});

async function settle() {
  for (let i = 0; i < 5; i++) await new Promise((r) => setTimeout(r, 0));
  await flushMicrotasks();
  flushSync();
}

function type(el: HTMLInputElement | HTMLTextAreaElement, value: string) {
  el.value = value;
  el.dispatchEvent(new Event("input", { bubbles: true }));
  flushSync();
}

function button(name: string | RegExp): HTMLButtonElement {
  const b = [...target.querySelectorAll("button")].find((x) =>
    typeof name === "string" ? x.textContent?.trim() === name : name.test(x.textContent ?? ""),
  );
  if (!b) throw new Error(`no button ${name}`);
  return b;
}

describe("Settings page", () => {
  let server: FakeServer;
  let client: FerrixClient;
  const toast = vi.fn();
  const ctx: DashboardContext = {
    auth: {} as DashboardContext["auth"],
    tables: [],
    tableByNumber: () => undefined,
    toast,
  };
  const loaded: SettingsView = {
    oauth: [],
    smtp: { url: "smtps://me@mail.example.com:465", from: "App <a@example.com>", password: MASK },
    redirectUrls: null,
    signups: null,
    accessTokenTtlSecs: null,
    sessionTtlSecs: null,
    corsOrigins: null,
    locked: ["redirectUrls"],
    effective: {
      signups: true,
      accessTokenTtlSecs: 900,
      sessionTtlSecs: 2_592_000,
      redirectUrls: ["https://app.example.com/"],
      oauthProviders: [],
      emailConfigured: true,
      corsOrigins: null,
    },
  };

  beforeEach(() => {
    server = new FakeServer();
    client = new FerrixClient("http://localhost:3210", { webSocketConstructor: server.WebSocket, unsavedChangesWarning: false });
    toast.mockClear();
  });
  afterEach(() => client.close());

  /** Answers the pending action call to `path` with `result` (or an error); returns its args. */
  async function answer(path: string, result: unknown, error?: { message: string; code: string }) {
    await settle();
    const msg = server.drain().find((m) => m.type === "Action" && m.udfPath === path);
    if (!msg || msg.type !== "Action") throw new Error(`no call to ${path}`);
    server.push(
      error
        ? { type: "ActionResponse", requestId: msg.requestId, success: false, errorMessage: error.message, errorData: { code: error.code } }
        : { type: "ActionResponse", requestId: msg.requestId, success: true, result },
    );
    await settle();
    return msg.args;
  }

  it("shows locked fields read-only, keeps secrets masked, validates inline and saves only what changed", async () => {
    mounted = mount(Settings, {
      target,
      context: new Map<unknown, unknown>([
        [Symbol.for("@ferrix/svelte:client"), client],
        [DASHBOARD_CONTEXT, ctx],
      ]),
    });
    server.open();
    await answer("_system/settings:get", loaded);

    // Locked by the configuration: an explanation and the effective value, no editor.
    const note = target.querySelector("[data-testid=locked-note]")!;
    expect(note.textContent).toContain("set by the server's configuration");
    expect(note.textContent).toContain("https://app.example.com/");
    expect(target.querySelector("textarea[aria-label='Redirect URLs']")).toBeNull();
    // The stored password is only a mask.
    const pw = target.querySelector<HTMLInputElement>("input[aria-label='SMTP password (stored, hidden)']")!;
    expect(pw.value).toBe(MASK);
    expect(pw.disabled).toBe(true);
    expect(button("Save changes").disabled).toBe(true);

    // Client-side validation, shown next to the field.
    const session = target.querySelector<HTMLInputElement>("#sessionTtlSecs")!;
    type(session, "5");
    button("Save changes").click();
    await settle();
    expect(target.querySelector("[role=alert]")?.textContent).toMatch(/from 60 to/);
    expect(server.drain().filter((m) => m.type === "Action")).toEqual([]);

    // A valid edit: only that field is sent.
    type(session, "7200");
    button("Save changes").click();
    const args = await answer("_system/settings:update", { ...loaded, sessionTtlSecs: 7200 });
    expect(args).toEqual({ sessionTtlSecs: 7200 });
    expect(toast).toHaveBeenCalledWith("Saved sessionTtlSecs; in effect now");
    expect(button("Save changes").disabled).toBe(true);

    // Replacing the password sends the new one with the other SMTP fields; a server rejection shows in the section.
    button("Replace").click();
    flushSync();
    type(target.querySelector<HTMLInputElement>("input[aria-label='SMTP password']")!, "new secret");
    button("Save changes").click();
    const sent = await answer("_system/settings:update", null, { message: "SMTP URL: relative URL without a base", code: "invalid_settings" });
    expect(sent).toEqual({ smtp: { url: "smtps://me@mail.example.com:465", from: "App <a@example.com>", password: "new secret" } });
    const smtpSection = target.querySelector("[aria-labelledby=s-smtp]")!;
    expect(smtpSection.querySelector("[role=alert]")?.textContent).toBe("SMTP URL: relative URL without a base");
    // Discard returns to the stored state (the mask).
    button("Discard").click();
    flushSync();
    expect(target.querySelector("input[aria-label='SMTP password (stored, hidden)']")).not.toBeNull();
  });
});

describe("Backups page", () => {
  let server: FakeServer;
  let client: FerrixClient;
  const toast = vi.fn();
  const ctx: DashboardContext = {
    auth: { getAccessToken: async () => "token" } as unknown as DashboardContext["auth"],
    tables: [],
    tableByNumber: () => undefined,
    toast,
  };
  const replication = {
    target: "s3://bucket/replica",
    everyMs: 1000,
    snapshotEveryMs: 86_400_000,
    keepMs: 604_800_000,
    generation: "20261010T120000.000Z-0000beef",
    currentTs: 120,
    currentTime: Date.now(),
    shippedTs: 117,
    shippedTime: Date.now() - 2000,
    lagCommits: 3,
    lagMs: 2500,
    pendingBytes: 4096,
    maxPendingBytes: 1 << 30,
    journaling: true,
    lastSegment: { name: "x.zst", from: 110, to: 117, entries: 8, bytes: 900, shippedAt: Date.now() - 1000 },
    lastSnapshot: { name: "ferrix-auto-20261010T120000.000Z-ts100.tar.zst", ts: 100, createdAt: Date.now() - 60_000, bytes: 2048 },
    nextSnapshotAt: Date.now() + 3_600_000,
    segmentsShipped: 4,
    snapshotsTaken: 1,
    lastSuccessAt: Date.now() - 1000,
    lastError: null,
    lastErrorAt: null,
    failures: 0,
    generations: [
      {
        id: "20261010T120000.000Z-0000beef",
        createdAt: Date.now() - 60_000,
        current: true,
        snapshots: [],
        segments: 4,
        fromTs: 100,
        toTs: 117,
        fromTime: Date.now() - 60_000,
        toTime: Date.now() - 2000,
      },
    ],
  };

  beforeEach(() => {
    server = new FakeServer();
    client = new FerrixClient("http://localhost:3210", { webSocketConstructor: server.WebSocket, unsavedChangesWarning: false });
    toast.mockClear();
  });
  afterEach(() => client.close());

  it("shows the replication status and stages a point-in-time restore", async () => {
    const calls: { url: string; init?: RequestInit }[] = [];
    const pendingRestore = {
      source: "point in time: ts 110 of generation g",
      stagedAt: 1,
      stagedBy: null,
      backupCreatedAt: 1,
      ts: 110,
      ferrixVersion: "0.1.0",
      includesAux: false,
      blobs: 0,
      bytes: 4096,
      secretIgnored: false,
      error: null,
      pointInTime: { generation: "g", snapshot: "s", snapshotTs: 100, requestedTs: 110, requestedTime: null, ts: 110, time: 5, replayed: 10, segments: 2, reached: true },
    };
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string, init?: RequestInit) => {
        calls.push({ url, init });
        if (url === "/api/admin/restore/point-in-time") {
          return new Response(JSON.stringify({ pendingRestore, restart: { available: false, exitCode: null } }));
        }
        return new Response(JSON.stringify({ target: "/data/backups", backups: [], pendingRestore: null, restart: { available: false, exitCode: null }, replication }));
      }),
    );
    vi.stubGlobal("confirm", () => true);
    mounted = mount(Backups, {
      target,
      context: new Map<unknown, unknown>([
        [Symbol.for("@ferrix/svelte:client"), client],
        [DASHBOARD_CONTEXT, ctx],
      ]),
    });
    server.open();
    await settle();
    const text = (id: string) => target.querySelector(`[data-testid=${id}]`)?.textContent?.replace(/\s+/g, " ").trim() ?? "";
    expect(text("replication-target")).toBe("s3://bucket/replica");
    expect(text("replication-health")).toContain("healthy");
    expect(text("replication-lag")).toContain("3 commits behind");
    expect(text("replication-lag")).toContain("117");
    expect(text("replication-last-segment")).toContain("110–117");
    expect(text("replication-last-snapshot")).toContain("ts100.tar.zst");
    expect(text("replication-generations")).toContain("current");

    // By commit ts: the picker starts at the newest shipped ts.
    const byTs = [...target.querySelectorAll<HTMLInputElement>("[data-testid=pitr] input[type=radio]")].find((r) => r.value === "ts")!;
    byTs.click();
    flushSync();
    const ts = target.querySelector<HTMLInputElement>("input[aria-label='Restore commit ts']")!;
    expect(ts.value).toBe("117");
    type(ts, "110");
    button("Restore to this point").click();
    await settle();
    const staged = calls.find((c) => c.url === "/api/admin/restore/point-in-time")!;
    expect(JSON.parse(staged.init!.body as string)).toEqual({ toTs: 110 });
    expect(text("pending-restore")).toContain("plus 10 replayed commits");
    expect(button("Restore to this point").disabled).toBe(true);
  });
});

describe("Workflows page", () => {
  it("lists workflows, shows an open one's steps, and cancels it", async () => {
    const server = new FakeServer();
    const client = new FerrixClient("http://localhost:3210", { webSocketConstructor: server.WebSocket, unsavedChangesWarning: false });
    const toast = vi.fn();
    const ctx: DashboardContext = {
      auth: { getAccessToken: async () => "token" } as unknown as DashboardContext["auth"],
      tables: [],
      tableByNumber: () => undefined,
      toast,
    };
    vi.stubGlobal("confirm", () => true);
    const now = Date.now();
    const wf = {
      _id: "w1",
      _creationTime: now - 7_200_000,
      name: "onboarding:onboard",
      args: { user: "u1" },
      state: "running",
      result: null,
      error: null,
      steps: 4,
      startedAt: now - 7_200_000,
      completedAt: null,
      origin: "users:sign_up",
      onComplete: { path: "onboarding:done", context: null },
      events: [{ name: "note", value: "early" }],
    };
    const step = (index: number, kind: string, name: string, state: string, extra: Record<string, unknown> = {}) => ({
      _id: `s${index}`,
      _creationTime: now,
      workflow: "w1",
      index,
      kind,
      name,
      args: {},
      state,
      result: null,
      error: null,
      job: null,
      startedAt: now - 7_000_000,
      completedAt: state === "running" ? null : now - 6_000_000,
      ...extra,
    });
    const steps = [
      step(0, "query", "users:email_of", "success", { result: "a@b.co" }),
      step(1, "action", "emails:send", "failed", { error: "SMTP said no", job: "j1" }),
      step(2, "sleep", "sleep", "success", { args: { ms: 3_600_000 } }),
      step(3, "event", "survey", "running", { args: null }),
    ];
    mounted = mount(Workflows, {
      target,
      props: { route: { page: "workflows", rest: [], params: new URLSearchParams("id=w1") } },
      context: new Map<unknown, unknown>([
        [Symbol.for("@ferrix/svelte:client"), client],
        [DASHBOARD_CONTEXT, ctx],
      ]),
    });
    server.open();
    await settle();
    const added = () =>
      server
        .drain()
        .flatMap((m) => (m.type === "ModifyQuerySet" ? m.modifications : []))
        .filter((m) => m.type === "Add");
    const listId = added().find((m) => m.udfPath === "_system/workflow:list")!.queryId;
    server.transition({ querySet: 1, ts: 1 }, [
      { type: "QueryUpdated", queryId: listId, value: { page: [wf], isDone: true, continueCursor: "end" } },
    ]);
    await settle();
    // The open workflow's steps are subscribed once its row shows.
    const stepsAdd = added().find((m) => m.udfPath === "_system/workflow:steps")!;
    expect(stepsAdd.args).toEqual({ id: "w1" });
    server.transition({ querySet: 2 }, [{ type: "QueryUpdated", queryId: stepsAdd.queryId, value: steps }]);
    await settle();
    const text = (id: string) => target.querySelector(`[data-testid=${id}]`)?.textContent?.replace(/\s+/g, " ").trim() ?? "";
    expect(text("workflows")).toContain("onboarding:onboard");
    expect(text("workflows")).toContain("2h 00m"); // running for two hours
    expect(text("workflows")).toContain("note: \"early\"");
    const rows = [...target.querySelectorAll("[data-testid=workflow-steps] tbody tr")].map((r) => r.textContent!.replace(/\s+/g, " ").trim());
    expect(rows).toHaveLength(4);
    expect(rows[0]).toContain("users:email_of");
    expect(rows[0]).toContain("a@b.co");
    expect(rows[1]).toContain("SMTP said no");
    expect(rows[2]).toContain("for 1h 00m");
    expect(rows[3]).toContain("waiting for the event");

    button("Cancel").click();
    await settle();
    const cancel = server.drain().find((m) => m.type === "Mutation")!;
    expect(cancel).toMatchObject({ udfPath: "_system/workflow:cancel_workflow", args: { id: "w1" } });
    if (cancel.type !== "Mutation") throw new Error("unreachable");
    server.push({ type: "MutationResponse", requestId: cancel.requestId, success: true, result: "canceled", ts: 2 });
    server.transition({ ts: 2 });
    await settle();
    expect(toast).toHaveBeenCalledWith("Workflow canceled", "info");
    client.close();
  });
});

describe("TimeChart", () => {
  const points: ChartPoint[] = [
    { t: 0, calls: 3, errors: 1, p50: 1, p95: 4 },
    { t: 60_000, calls: 0, errors: 0, p50: null, p95: null },
    { t: 120_000, calls: 5, errors: 0, p50: 2, p95: 3 },
    { t: 180_000, calls: 2, errors: 0, p50: 1.5, p95: 2 },
  ];

  it("draws latency lines with gaps and a legend", () => {
    mounted = mount(TimeChart, { target, props: { points, bucket: 60_000, variant: "latency", label: "Latency" } });
    flushSync();
    const paths = [...target.querySelectorAll("path.series")].map((p) => p.getAttribute("d") ?? "");
    expect(paths).toHaveLength(2);
    // Two runs per series: the empty bucket breaks the line.
    for (const d of paths) expect(d.match(/M/g)).toHaveLength(2);
    // The first bucket is isolated (its neighbor is empty): drawn as dots too.
    expect(target.querySelectorAll("circle.dot")).toHaveLength(2);
    expect(target.querySelector("figcaption")?.textContent).toMatch(/Latency.*p50.*p95/s);
    expect(target.querySelector("svg")?.getAttribute("aria-label")).toBe("Latency");
  });

  it("stacks errors on calls", () => {
    mounted = mount(TimeChart, { target, props: { points, bucket: 60_000, variant: "volume", label: "Calls" } });
    flushSync();
    expect(target.querySelectorAll("rect.bar.s1")).toHaveLength(3); // buckets with successful calls
    expect(target.querySelectorAll("rect.bar.err")).toHaveLength(1);
    const ticks = [...target.querySelectorAll("text.axis")].map((t) => t.textContent);
    expect(ticks).toContain("0");
    expect(ticks).toContain("6");
  });
});

describe("CommandPalette", () => {
  it("filters as you type and runs the chosen command", () => {
    const ran: string[] = [];
    const commands: Command[] = ["Overview", "Data", "Audit log", "Settings"].map((label) => ({
      id: label,
      label,
      group: "Page",
      run: () => ran.push(label),
    }));
    const onclose = vi.fn();
    mounted = mount(CommandPalette, { target, props: { commands, onclose } });
    flushSync();
    const input = target.querySelector<HTMLInputElement>("input[role=combobox]")!;
    expect(target.querySelectorAll("[role=option]")).toHaveLength(4);
    type(input, "set");
    expect([...target.querySelectorAll("[role=option] .label")].map((l) => l.textContent)).toEqual(["Settings"]);
    input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
    expect(ran).toEqual(["Settings"]);
    expect(onclose).toHaveBeenCalled();

    type(input, "a");
    expect(target.querySelector("[aria-selected=true]")?.textContent).toContain("Audit log"); // a prefix match first
    input.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }));
    flushSync();
    expect(target.querySelector("[aria-selected=true]")?.textContent).toContain("Data");
    input.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    expect(onclose).toHaveBeenCalledTimes(2);
  });
});

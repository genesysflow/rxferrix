import { RxFerrixClient, RxFerrixError } from "@rxferrix/client";
import { FakeServer, flushMicrotasks } from "@rxferrix/client/testing";
import { flushSync, mount, unmount } from "svelte";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { MASK, type SettingsView } from "../src/lib/api.js";
import type { ChartPoint } from "../src/lib/chart.js";
import type { Command } from "../src/lib/commands.js";
import { DASHBOARD_CONTEXT, type DashboardContext } from "../src/lib/context.js";
import CommandPalette from "../src/components/CommandPalette.svelte";
import LineChart from "../src/components/LineChart.svelte";
import TimeChart from "../src/components/TimeChart.svelte";
import Account from "../src/pages/Account.svelte";
import Analytics from "../src/pages/Analytics.svelte";
import Backups from "../src/pages/Backups.svelte";
import Login from "../src/pages/Login.svelte";
import Overview from "../src/pages/Overview.svelte";
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
  let client: RxFerrixClient;
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
    backups: null,
    trustedProxy: null,
    rateLimits: null,
    emailTemplates: null,
    emailTemplateDefaults: {
      appName: null,
      verifyEmail: { subject: "Verify your email address", html: "<p>Please confirm your email address.</p>\n<p>Your code: <strong>{code}</strong></p>" },
      resetPassword: { subject: "Reset your password", html: "<p>Your code: <strong>{code}</strong></p>" },
      signIn: { subject: "Your sign-in link", html: "<p><a href=\"{link}\">Sign in</a></p>\n<p>Your code: <strong>{code}</strong></p>" },
    },
    locked: ["redirectUrls"],
    effective: {
      signups: true,
      accessTokenTtlSecs: 900,
      sessionTtlSecs: 2_592_000,
      redirectUrls: ["https://app.example.com/"],
      oauthProviders: [],
      emailConfigured: true,
      corsOrigins: null,
      backups: { target: "/data/backups", everySecs: null, keep: 7, includeAux: false, includeFiles: false, s3Available: true },
      trustedProxy: { headers: [], useLeftmostIp: false },
      appName: "",
    },
  };

  beforeEach(() => {
    server = new FakeServer();
    client = new RxFerrixClient("http://localhost:3210", { webSocketConstructor: server.WebSocket, unsavedChangesWarning: false });
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
        [Symbol.for("@rxferrix/svelte:client"), client],
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

  it("shows the resolved client IP and edits the trusted proxy headers", async () => {
    const clientIp = { ip: "203.0.113.7", header: null, peer: "203.0.113.7", proxyHeaders: [{ name: "fly-client-ip", value: "198.51.100.9" }] };
    vi.stubGlobal("fetch", vi.fn(async () => new Response(JSON.stringify(clientIp))));
    const auth = { getAccessToken: async () => "admin-token" } as unknown as DashboardContext["auth"];
    mounted = mount(Settings, {
      target,
      context: new Map<unknown, unknown>([
        [Symbol.for("@rxferrix/svelte:client"), client],
        [DASHBOARD_CONTEXT, { ...ctx, auth }],
      ]),
    });
    server.open();
    await answer("_system/settings:get", loaded);

    const resolved = target.querySelector("[data-testid=resolved-ip]")!;
    expect(resolved.textContent).toContain("203.0.113.7");
    expect(resolved.textContent).toContain("fly-client-ip: 198.51.100.9");
    // The header seen on this request is suggested first; a click adds it.
    button("Fly-Client-IP").click();
    flushSync();
    button("CF-Connecting-IP").click();
    flushSync();
    expect(target.querySelector<HTMLInputElement>("#proxy-headers")!.value).toBe("Fly-Client-IP, CF-Connecting-IP");
    button("Save changes").click();
    const args = await answer("_system/settings:update", {
      ...loaded,
      trustedProxy: { headers: ["Fly-Client-IP", "CF-Connecting-IP"], useLeftmostIp: false },
    });
    expect(args).toEqual({ trustedProxy: { headers: ["Fly-Client-IP", "CF-Connecting-IP"], useLeftmostIp: false } });
  });

  it("adds rate limit rules and shows the server's objections in the section", async () => {
    mounted = mount(Settings, {
      target,
      context: new Map<unknown, unknown>([
        [Symbol.for("@rxferrix/svelte:client"), client],
        [DASHBOARD_CONTEXT, ctx],
      ]),
    });
    server.open();
    await answer("_system/settings:get", loaded);
    expect(target.querySelector("[data-testid=rate-limits-on]")).toBeNull();

    // The first rule comes filled in with a sensible default and turns limiting on.
    button("+ Add rate limit rule").click();
    flushSync();
    expect(target.querySelector<HTMLInputElement>("input[aria-label='Enable rate limiting']")!.checked).toBe(true);
    button("+ Add rate limit rule").click();
    flushSync();
    type(target.querySelector<HTMLInputElement>("input[aria-label='Rule 2 label']")!, "*:create");
    type(target.querySelector<HTMLInputElement>("input[aria-label='Rule 2 max requests']")!, "20");
    type(target.querySelector<HTMLInputElement>("input[aria-label='Rule 2 interval in seconds']")!, "5");
    const audience = target.querySelector<HTMLSelectElement>("select[aria-label='Rule 2 targeted users']")!;
    audience.value = "guest";
    audience.dispatchEvent(new Event("change"));
    flushSync();

    button("Save changes").click();
    const rateLimits = {
      enabled: true,
      rules: [
        { label: "/api/", maxRequests: 300, intervalSecs: 10, audience: "all" as const },
        { label: "*:create", maxRequests: 20, intervalSecs: 5, audience: "guest" as const },
      ],
    };
    const args = await answer("_system/settings:update", { ...loaded, rateLimits });
    expect(args).toEqual({ rateLimits });
    expect(target.querySelector("[data-testid=rate-limits-on]")?.textContent).toBe("Enabled");

    target.querySelector<HTMLButtonElement>("button[aria-label='Remove rule 1']")!.click();
    flushSync();
    button("Save changes").click();
    await answer("_system/settings:update", null, { message: "rate limit `*:create`: something", code: "invalid_settings" });
    const section = target.querySelector("[aria-labelledby=s-rate]")!;
    expect(section.querySelector("[role=alert]")?.textContent).toBe("rate limit `*:create`: something");
  });

  it("customizes an email template with placeholders and a preview", async () => {
    mounted = mount(Settings, {
      target,
      context: new Map<unknown, unknown>([
        [Symbol.for("@rxferrix/svelte:client"), client],
        [DASHBOARD_CONTEXT, ctx],
      ]),
    });
    server.open();
    await answer("_system/settings:get", loaded);

    button(/^Sign-in link/).click();
    flushSync();
    expect(target.querySelector("[role=tabpanel]")!.textContent).toContain("Your sign-in link");
    button("Customize").click();
    flushSync();
    const subject = target.querySelector<HTMLInputElement>("#template-subject")!;
    const html = target.querySelector<HTMLTextAreaElement>("#template-html")!;
    expect(subject.value).toBe("Your sign-in link");
    expect(button(/^Sign-in link/).textContent).toContain("custom");

    type(target.querySelector<HTMLInputElement>("#app-name")!, "Acme <Shop>");
    type(subject, "Sign in to {app_name}");
    expect(target.querySelector("[data-testid=template-preview-subject]")!.textContent).toContain("Sign in to Acme <Shop>");
    expect(target.querySelector("iframe")!.getAttribute("sandbox")).toBe("");

    type(html, "<p>Hello</p>");
    button("{email}").click();
    flushSync();
    expect(html.value).toBe("<p>Hello</p>{email}");
    button("Save changes").click();
    flushSync();
    expect(target.textContent).toContain("Include {code} or {link}");

    type(html, "<p>Hello {email}: {code}</p>");
    expect(target.querySelector("iframe")!.getAttribute("srcdoc")).toBe("<p>Hello ana@example.com: 123456</p>");
    button("Save changes").click();
    const templates = { appName: "Acme <Shop>", verifyEmail: null, resetPassword: null, signIn: { subject: "Sign in to {app_name}", html: "<p>Hello {email}: {code}</p>" } };
    const args = await answer("_system/settings:update", { ...loaded, emailTemplates: templates });
    expect(args).toEqual({ emailTemplates: templates });

    // Back to the built-in email: the whole setting is cleared.
    button("Use the built-in email").click();
    type(target.querySelector<HTMLInputElement>("#app-name")!, "");
    button("Save changes").click();
    expect(await answer("_system/settings:update", loaded)).toEqual({ clear: ["emailTemplates"] });
  });
});

describe("Backups page", () => {
  let server: FakeServer;
  let client: RxFerrixClient;
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
    lastSnapshot: { name: "rxferrix-auto-20261010T120000.000Z-ts100.tar.zst", ts: 100, createdAt: Date.now() - 60_000, bytes: 2048 },
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
    client = new RxFerrixClient("http://localhost:3210", { webSocketConstructor: server.WebSocket, unsavedChangesWarning: false });
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
      rxferrixVersion: "0.1.0",
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
        [Symbol.for("@rxferrix/svelte:client"), client],
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
    const client = new RxFerrixClient("http://localhost:3210", { webSocketConstructor: server.WebSocket, unsavedChangesWarning: false });
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
        [Symbol.for("@rxferrix/svelte:client"), client],
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

describe("LineChart", () => {
  const times = [0, 3000, 6000];
  const percent = (n: number) => `${n}%`;

  it("keeps a percentage axis at 0–100% for small values", () => {
    const series = [{ label: "Hit rate", color: "red", values: [0.2, 0.4, null] }];
    mounted = mount(LineChart, { target, props: { times, series, format: percent, min: 100, max: 100, label: "Hit rate" } });
    flushSync();
    const ticks = [...target.querySelectorAll("text.axis")].map((t) => t.textContent);
    expect(ticks).toContain("0%");
    expect(ticks).toContain("100%");
  });

  it("shows the empty message when every value is null", () => {
    const series = [{ label: "Hit rate", color: "red", values: [null, null, null] }];
    mounted = mount(LineChart, { target, props: { times, series, label: "Hit rate", empty: "No cache lookups." } });
    flushSync();
    expect(target.querySelector("svg")).toBeNull();
    expect(target.textContent).toContain("No cache lookups.");
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

describe("Overview page", () => {
  it("shows the analytics copy when the server keeps one", async () => {
    const overview = {
      now: Date.now(), startedAt: Date.now() - 1000, version: "0.1.0", dev: false, ts: 42, commitTime: Date.now(),
      tables: 3, documents: 10, registeredFunctions: 5, sockets: 0, subscriptions: 0, droppedLogEntries: 0,
      files: [], commitsPerSecond: 0, queueDepth: 0, runningJobs: 0, metricsEnabled: false,
    };
    const olap = {
      state: "ready", detail: null, appliedTs: 40, latestTs: 42, appliedTime: Date.now() - 300, lagMs: 300,
      tables: [{ name: "orders", rows: 1200, columns: [] }, { name: "_users", rows: 34, columns: [] }],
      sizeBytes: 3 << 20, rowsApplied: 1234, rebuilds: 1,
    };
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string) =>
        url.startsWith("/api/admin/stats?") || url === "/api/admin/stats"
          ? new Response(JSON.stringify({ since: 0, functions: [], overview, olap }))
          : new Response(JSON.stringify({ points: [] })),
      ),
    );
    const ctx = { auth: { getAccessToken: async () => "token" }, tables: [], tableByNumber: () => undefined, toast: vi.fn() };
    mounted = mount(Overview, { target, context: new Map<unknown, unknown>([[DASHBOARD_CONTEXT, ctx]]) });
    await settle();
    const card = target.querySelector("[data-testid=stat-olap]")?.textContent?.replace(/\s+/g, " ").trim() ?? "";
    expect(card).toContain("Analytics copy");
    expect(card).toContain("ready");
    expect(card).toContain("2 commits behind (300ms)");
    expect(card).toContain("1234 rows");
    expect(card).toContain("3.0 MB");
  });

  it("counts the cached queries apart from the shared ones serving subscriptions", async () => {
    const overview = {
      now: Date.now(), startedAt: Date.now() - 1000, version: "0.1.0", dev: false, ts: 42, commitTime: Date.now(),
      tables: 3, documents: 10, registeredFunctions: 5, sockets: 4, subscriptions: 17, droppedLogEntries: 0,
      files: [], commitsPerSecond: 0, queueDepth: 0, runningJobs: 0, metricsEnabled: false,
      sharedQueries: 25, cachedQueries: 10, queryReruns: 443, subscriptionRounds: 689, cacheHits: 2, cacheMisses: 3,
    };
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string) =>
        url.startsWith("/api/admin/stats?") || url === "/api/admin/stats"
          ? new Response(JSON.stringify({ since: 0, functions: [], overview }))
          : new Response(JSON.stringify({ points: [] })),
      ),
    );
    const ctx = { auth: { getAccessToken: async () => "token" }, tables: [], tableByNumber: () => undefined, toast: vi.fn() };
    mounted = mount(Overview, { target, context: new Map<unknown, unknown>([[DASHBOARD_CONTEXT, ctx]]) });
    await settle();
    const live = target.querySelector("[data-testid=live-stats]")?.textContent?.replace(/\s+/g, " ") ?? "";
    expect(live).toMatch(/15 ?shared queries serving 17 subscriptions · 10 cached/);
    expect(live).toContain("443 reruns in 689 rounds since start");
    expect(live).toMatch(/40% ?hit rate since start/);
  });

  it("scales the failure rate axis to small rates", async () => {
    const overview = {
      now: Date.now(), startedAt: Date.now() - 1000, version: "0.1.0", dev: false, ts: 42, commitTime: Date.now(),
      tables: 3, documents: 10, registeredFunctions: 5, sockets: 0, subscriptions: 0, droppedLogEntries: 0,
      files: [], commitsPerSecond: 0, queueDepth: 0, runningJobs: 0, metricsEnabled: false,
    };
    const point = (t: number, calls: number, errors: number) => ({
      t, calls, errors, userErrors: errors, serverErrors: 0, p50Us: 500, p95Us: 900, maxUs: 1000,
    });
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string) => {
        if (url.startsWith("/api/admin/stats/timeseries?")) {
          const q = new URLSearchParams(url.split("?")[1]);
          const bucket = Number(q.get("bucket"));
          const since = Number(q.get("since"));
          const until = Number(q.get("until"));
          // 0.5% and 2% of calls failed: the axis should end near 2%, not at 100%.
          const points = [point(since, 200, 1), point(since + bucket, 100, 2)];
          return new Response(JSON.stringify({ path: null, bucket, since, until, points }));
        }
        return new Response(JSON.stringify({ since: 0, functions: [], overview }));
      }),
    );
    const ctx = { auth: { getAccessToken: async () => "token" }, tables: [], tableByNumber: () => undefined, toast: vi.fn() };
    mounted = mount(Overview, { target, context: new Map<unknown, unknown>([[DASHBOARD_CONTEXT, ctx]]) });
    await settle();
    const chart = target.querySelector("svg[aria-label='Failure rate']");
    const ticks = [...(chart?.querySelectorAll("text.axis") ?? [])].map((t) => t.textContent);
    expect(ticks).toContain("2%");
    expect(ticks).not.toContain("100%");
  });
});

describe("Analytics page", () => {
  const ctx = { auth: { getAccessToken: async () => "token" }, tables: [], tableByNumber: () => undefined, toast: vi.fn() };
  const status = {
    enabled: true, state: "ready", detail: null, appliedTs: 9, latestTs: 9, appliedTime: 1, lagMs: 0, sizeBytes: 1 << 20, rowsApplied: 30, rebuilds: 1,
    tables: [{ name: "orders", rows: 30, columns: [{ name: "_id", typeName: "VARCHAR" }, { name: "amount", typeName: "DOUBLE" }] }],
  };

  beforeEach(() => localStorage.clear());

  it("says so when analytics are off", async () => {
    vi.stubGlobal("fetch", vi.fn(async () => new Response(JSON.stringify({ enabled: false }))));
    mounted = mount(Analytics, { target, context: new Map<unknown, unknown>([[DASHBOARD_CONTEXT, ctx]]) });
    await settle();
    expect(target.textContent).toContain("Analytics are off");
  });

  it("runs a query and shows the rows, or the error", async () => {
    const bodies: unknown[] = [];
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string, init?: RequestInit) => {
        if (url === "/api/admin/olap") return new Response(JSON.stringify(status));
        const body = JSON.parse(init!.body as string) as { sql: string };
        bodies.push(body);
        if (body.sql.startsWith("DELETE")) {
          return new Response(JSON.stringify({ status: "error", errorMessage: "Analytics queries run exactly one SELECT statement", errorData: { code: "olap_query_failed" } }), { status: 400 });
        }
        return new Response(JSON.stringify({ columns: [{ name: "n", typeName: "BIGINT" }, { name: "tags", typeName: "LIST" }, { name: "note", typeName: "VARCHAR" }], rows: [{ n: 30, tags: ["a"], note: null }], asOfTs: 9 }));
      }),
    );
    mounted = mount(Analytics, { target, context: new Map<unknown, unknown>([[DASHBOARD_CONTEXT, ctx]]) });
    await settle();
    expect(target.querySelector("[data-testid=olap-status]")?.textContent).toContain("ready");
    // A table's "query" link fills the editor.
    button("query").click();
    flushSync();
    const editor = target.querySelector<HTMLTextAreaElement>("textarea[aria-label=SQL]")!;
    expect(editor.value).toBe("SELECT * FROM orders LIMIT 100");
    type(editor, "SELECT count(*) AS n FROM orders");
    button("Run").click();
    await settle();
    expect(bodies.at(-1)).toEqual({ sql: "SELECT count(*) AS n FROM orders", params: [] });
    const results = target.querySelector("[data-testid=olap-results]")!;
    expect(results.querySelector("thead")?.textContent).toContain("BIGINT");
    expect([...results.querySelectorAll("tbody td")].map((td) => td.textContent)).toEqual(["30", '["a"]', "null"]);
    expect(target.querySelector("[data-testid=olap-summary]")?.textContent).toContain("1 rows · as of commit 9");
    expect(localStorage.getItem("rxferrix.analytics.sql")).toBe("SELECT count(*) AS n FROM orders");

    type(editor, "DELETE FROM orders");
    button("Run").click();
    await settle();
    expect(target.querySelector("[data-testid=olap-error]")?.textContent).toContain("exactly one SELECT");
  });
});

describe("Login page", () => {
  it("asks for a code when the administrator has two-factor authentication", async () => {
    const signInAsAdmin = vi.fn(async () => ({ mfaRequired: true as const, mfaToken: "mfa-1", methods: ["totp"] }));
    const verifyMfa = vi.fn(async (_token: string, code: string) => {
      if (code !== "123456") throw new RxFerrixError("The code is not valid", { code: "invalid_mfa_code" });
    });
    const ctx = { auth: { signInAsAdmin, verifyMfa }, tables: [], tableByNumber: () => undefined, toast: vi.fn() };
    mounted = mount(Login, { target, context: new Map<unknown, unknown>([[DASHBOARD_CONTEXT, ctx]]) });
    type(target.querySelector<HTMLInputElement>("input[type=email]")!, "admin@example.com");
    type(target.querySelector<HTMLInputElement>("input[type=password]")!, "pw");
    target.querySelector("form")!.requestSubmit();
    await settle();
    expect(signInAsAdmin).toHaveBeenCalledWith({ email: "admin@example.com", password: "pw" });
    expect(target.textContent).toContain("Two-factor authentication");
    expect(target.querySelector("input[type=password]")).toBeNull();

    const codeInput = target.querySelector<HTMLInputElement>("input[autocomplete=one-time-code]")!;
    type(codeInput, "000000");
    target.querySelector("form")!.requestSubmit();
    await settle();
    expect(verifyMfa).toHaveBeenCalledWith("mfa-1", "000000");
    expect(target.querySelector("[role=alert]")?.textContent).toBe("The code is not valid");

    type(codeInput, "123456");
    target.querySelector("form")!.requestSubmit();
    await settle();
    expect(verifyMfa).toHaveBeenLastCalledWith("mfa-1", "123456");
    expect(target.querySelector("[role=alert]")).toBeNull();

    // "Start over" returns to the password form.
    button("Start over").click();
    flushSync();
    expect(target.querySelector("input[type=password]")).not.toBeNull();
  });
});

describe("Account page", () => {
  it("walks through enabling two-factor authentication and shows the recovery codes once", async () => {
    const server = new FakeServer();
    const client = new RxFerrixClient("http://localhost:3210", { webSocketConstructor: server.WebSocket, unsavedChangesWarning: false });
    const status = { enabled: false, pending: false, recoveryCodesLeft: 0, methods: [] as string[] };
    const auth = {
      getMfaStatus: vi.fn(async () => ({ ...status })),
      setupTotp: vi.fn(async ({ password }: { password?: string }) => {
        if (password !== "pw") throw new RxFerrixError("The password is not correct", { code: "invalid_current_password" });
        return { secret: "JBSWY3DPEHPK3PXP", uri: "otpauth://totp/x", qrSvg: "<svg data-testid='qr'></svg>" };
      }),
      confirmTotp: vi.fn(async (code: string) => {
        if (code !== "123456") throw new RxFerrixError("The code is not valid", { code: "invalid_mfa_code" });
        Object.assign(status, { enabled: true, recoveryCodesLeft: 8, methods: ["totp"] });
        return { recoveryCodes: ["aaaaa-bbbbb", "ccccc-ddddd"] };
      }),
      disableMfa: vi.fn(async () => {
        Object.assign(status, { enabled: false, recoveryCodesLeft: 0, methods: [] });
      }),
      regenerateRecoveryCodes: vi.fn(),
    };
    const toast = vi.fn();
    const ctx = { auth, tables: [], tableByNumber: () => undefined, toast };
    mounted = mount(Account, {
      target,
      context: new Map<unknown, unknown>([
        [Symbol.for("@rxferrix/svelte:client"), client],
        [DASHBOARD_CONTEXT, ctx],
      ]),
    });
    await settle();
    const mfa = () => target.querySelector("[data-testid=mfa]")!.textContent?.replace(/\s+/g, " ") ?? "";
    expect(mfa()).toContain("off");

    button("Turn on…").click();
    await settle();
    const dialog = () => document.querySelector("dialog")!;
    type(dialog().querySelector<HTMLInputElement>("input[type=password]")!, "wrong");
    [...dialog().querySelectorAll("button")].find((b) => b.textContent?.trim() === "Continue")!.click();
    await settle();
    expect(dialog().querySelector("[role=alert]")?.textContent).toBe("The password is not correct");
    type(dialog().querySelector<HTMLInputElement>("input[type=password]")!, "pw");
    [...dialog().querySelectorAll("button")].find((b) => b.textContent?.trim() === "Continue")!.click();
    await settle();
    expect(auth.setupTotp).toHaveBeenLastCalledWith({ password: "pw" });
    expect(dialog().querySelector("[data-testid=qr]")).not.toBeNull();
    expect(dialog().textContent).not.toContain("JBSWY3DPEHPK3PXP");
    [...dialog().querySelectorAll("button")].find((b) => b.textContent?.includes("Show the key"))!.click();
    flushSync();
    expect(dialog().textContent).toContain("JBSWY3DPEHPK3PXP");

    type(dialog().querySelector<HTMLInputElement>("input[autocomplete=one-time-code]")!, "123456");
    [...dialog().querySelectorAll("button")].find((b) => b.textContent?.trim() === "Turn on")!.click();
    await settle();
    expect(auth.confirmTotp).toHaveBeenCalledWith("123456");
    const codes = [...dialog().querySelectorAll("[data-testid=recovery-codes] li")].map((li) => li.textContent);
    expect(codes).toEqual(["aaaaa-bbbbb", "ccccc-ddddd"]);
    expect(toast).toHaveBeenCalledWith("Two-factor authentication is on: your next sign-in needs a code");
    [...dialog().querySelectorAll("button")].find((b) => b.textContent?.trim() === "I saved them")!.click();
    await settle();
    expect(document.querySelector("dialog")).toBeNull();
    expect(mfa()).toContain("on");
    expect(mfa()).toContain("Recovery codes left: 8");

    // Turning off asks for a code.
    button("Turn off…").click();
    await settle();
    type(dialog().querySelector<HTMLInputElement>("input[autocomplete=one-time-code]")!, "aaaaa-bbbbb");
    [...dialog().querySelectorAll("button")].find((b) => b.textContent?.trim() === "Turn off")!.click();
    await settle();
    expect(auth.disableMfa).toHaveBeenCalledWith("aaaaa-bbbbb");
    expect(mfa()).toContain("off");
    client.close();
  });
});

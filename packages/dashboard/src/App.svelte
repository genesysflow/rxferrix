<script lang="ts">
  import { FerrixClient, getAuth } from "@ferrix/client";
  import { setupFerrix, useAuth, useConnectionState, useQuery } from "@ferrix/svelte";
  import { dashboard, type TableSummary } from "./lib/api.js";
  import { setDashboard } from "./lib/context.js";
  import { href, link, router, type Page } from "./lib/router.svelte.js";
  import { theme } from "./lib/theme.svelte.js";
  import { type Command } from "./lib/commands.js";
  import CommandPalette from "./components/CommandPalette.svelte";
  import Audit from "./pages/Audit.svelte";
  import Analytics from "./pages/Analytics.svelte";
  import Backups from "./pages/Backups.svelte";
  import Data from "./pages/Data.svelte";
  import Files from "./pages/Files.svelte";
  import Functions from "./pages/Functions.svelte";
  import Login from "./pages/Login.svelte";
  import Logs from "./pages/Logs.svelte";
  import Migrations from "./pages/Migrations.svelte";
  import Overview from "./pages/Overview.svelte";
  import Scheduler from "./pages/Scheduler.svelte";
  import Workflows from "./pages/Workflows.svelte";
  import Settings from "./pages/Settings.svelte";
  import Setup from "./pages/Setup.svelte";
  import Users from "./pages/Users.svelte";

  // Same origin: the dashboard is served by the ferrix server at /_/.
  const client = new FerrixClient(location.origin, { unsavedChangesWarning: false });
  setupFerrix(client);
  // Its own storage key, so an app served from the same origin keeps its user session separate.
  const ferrixAuth = getAuth(client, { storageKey: `ferrix.dashboard.auth:${location.origin}` });
  const auth = useAuth();
  const connection = useConnectionState();
  const isAdmin = $derived(auth.isAuthenticated && auth.isAdmin);
  const tables = useQuery(dashboard.tables, () => (isAdmin ? {} : "skip"));
  const me = useQuery(dashboard.me, () => (isAdmin ? {} : "skip"));

  let toasts = $state<{ id: number; message: string; kind: "info" | "error" }[]>([]);
  let nextToast = 0;

  setDashboard({
    auth: ferrixAuth,
    get tables() {
      return tables.data;
    },
    tableByNumber(n: number): TableSummary | undefined {
      return tables.data?.find((t) => t.number === n);
    },
    toast(message, kind = "info") {
      const id = nextToast++;
      // At most 4 at a time: the oldest go first.
      toasts = [...toasts.slice(-3), { id, message, kind }];
      setTimeout(() => (toasts = toasts.filter((t) => t.id !== id)), kind === "error" ? 8000 : 3500);
    },
  });

  interface NavItem {
    page: Page;
    label: string;
    icon: string;
    /** Words the command palette also matches. */
    keywords?: string;
  }
  const groups: { label: string | null; items: NavItem[] }[] = [
    { label: null, items: [{ page: "overview", label: "Overview", icon: "◎", keywords: "stats home" }] },
    {
      label: "Build",
      items: [
        { page: "data", label: "Data", icon: "▦", keywords: "tables documents" },
        { page: "functions", label: "Functions", icon: "ƒ", keywords: "runner latency charts" },
        { page: "logs", label: "Logs", icon: "≡", keywords: "executions tail" },
      ],
    },
    {
      label: "Manage",
      items: [
        { page: "users", label: "Users", icon: "◍", keywords: "sessions auth impersonate" },
        { page: "files", label: "Files", icon: "▤", keywords: "storage uploads" },
        { page: "scheduler", label: "Scheduler", icon: "◷", keywords: "crons jobs workpools" },
        { page: "workflows", label: "Workflows", icon: "⇶", keywords: "steps durable events" },
      ],
    },
    {
      label: "Operate",
      items: [
        { page: "migrations", label: "Migrations", icon: "⇄", keywords: "schema" },
        { page: "backups", label: "Backups", icon: "⧉", keywords: "export import restore" },
        { page: "analytics", label: "Analytics", icon: "∑", keywords: "olap sql duckdb parquet csv" },
      ],
    },
    {
      label: "Admin",
      items: [
        { page: "settings", label: "Settings", icon: "✱", keywords: "smtp oauth cors redirect signups tokens" },
        { page: "audit", label: "Audit log", icon: "✎", keywords: "history changes" },
      ],
    },
  ];
  const nav = groups.flatMap((g) => g.items);
  /** Alt+1…9, then Alt+0, open the pages in sidebar order. */
  const shortcutOf = (i: number) => (i < 9 ? `Alt+${i + 1}` : i === 9 ? "Alt+0" : undefined);

  const route = $derived(router.route);
  const live = $derived(connection.current.isWebSocketConnected);
  let palette = $state(false);

  const commands = $derived<Command[]>([
    ...nav.map((item, i) => ({
      id: `page:${item.page}`,
      label: item.label,
      group: "Page",
      keywords: item.keywords,
      shortcut: shortcutOf(i),
      run: () => router.navigate(href(item.page)),
    })),
    ...(tables.data ?? []).map((t) => ({
      id: `table:${t.name}`,
      label: t.name,
      group: t.system ? "System table" : "Table",
      keywords: `data ${t.name}`,
      run: () => router.navigate(href("data", [t.name])),
    })),
    { id: "cmd:theme", label: "Switch theme (light, dark, system)", group: "Command", keywords: "dark mode", run: () => theme.cycle() },
    { id: "cmd:signout", label: "Sign out", group: "Command", keywords: "logout", run: () => void signOut() },
  ]);

  function onkeydown(e: KeyboardEvent) {
    if (!isAdmin) return;
    if ((e.ctrlKey || e.metaKey) && !e.altKey && e.key.toLowerCase() === "k") {
      e.preventDefault();
      palette = !palette;
      return;
    }
    // Alt+1…9 and Alt+0 switch pages (e.code: Alt+digit types other characters on some layouts).
    const digit = /^Digit(\d)$/.exec(e.code)?.[1] ?? (/^\d$/.test(e.key) ? e.key : null);
    if (e.altKey && !e.ctrlKey && !e.metaKey && digit !== null) {
      const item = nav[digit === "0" ? 9 : Number(digit) - 1];
      if (item) {
        e.preventDefault();
        router.navigate(href(item.page));
      }
    }
  }

  async function signOut() {
    await ferrixAuth.signOut();
    router.navigate(href("overview"));
  }
</script>

<svelte:window {onkeydown} />

{#if route.page === "setup"}
  <Setup />
{:else if auth.isLoading}
  <div class="splash muted">Connecting…</div>
{:else if !auth.isAuthenticated}
  <Login />
{:else if !auth.isAdmin}
  <div class="splash">
    <p>This session is not an administrator session.</p>
    <button onclick={signOut}>Sign out</button>
  </div>
{:else}
  <div class="shell">
    <nav class="sidebar" aria-label="Dashboard">
      <a class="brand" href={href("overview")} onclick={link}><span class="logo">F</span> ferrix</a>
      <button class="palette-button" onclick={() => (palette = true)} title="Command palette">
        <span>Go to…</span><kbd>Ctrl K</kbd>
      </button>
      <div class="groups">
        {#each groups as g, gi (gi)}
          {#if g.label}<div class="group-label" aria-hidden="true">{g.label}</div>{/if}
          <ul aria-label={g.label ?? "Home"}>
            {#each g.items as item (item.page)}
              {@const i = nav.indexOf(item)}
              <li>
                <a
                  href={href(item.page)}
                  onclick={link}
                  class:active={route.page === item.page}
                  aria-current={route.page === item.page ? "page" : undefined}
                  title={shortcutOf(i) ? `${item.label} (${shortcutOf(i)})` : item.label}
                >
                  <span class="icon" aria-hidden="true">{item.icon}</span>{item.label}
                </a>
              </li>
            {/each}
          </ul>
        {/each}
      </div>
      <div class="foot">
        <div class="conn" title={live ? "Connected: views update live" : "Reconnecting…"}>
          <span class="dot" class:on={live}></span>{live ? "live" : "offline"}
        </div>
        <div class="who mono" title="Signed in administrator">{me.data?.email ?? ""}</div>
        <div class="row">
          <button class="small" onclick={() => theme.cycle()} title="Theme: light, dark or system">
            {theme.current === "system" ? "◐ system" : theme.current === "dark" ? "☾ dark" : "☀ light"}
          </button>
          <button class="small" onclick={signOut}>Sign out</button>
        </div>
      </div>
    </nav>
    <main>
      {#if route.page === "data"}
        <Data {route} />
      {:else if route.page === "functions"}
        <Functions {route} />
      {:else if route.page === "logs"}
        <Logs />
      {:else if route.page === "scheduler"}
        <Scheduler {route} />
      {:else if route.page === "workflows"}
        <Workflows {route} />
      {:else if route.page === "users"}
        <Users />
      {:else if route.page === "files"}
        <Files {route} />
      {:else if route.page === "migrations"}
        <Migrations />
      {:else if route.page === "backups"}
        <Backups />
      {:else if route.page === "analytics"}
        <Analytics />
      {:else if route.page === "settings"}
        <Settings />
      {:else if route.page === "audit"}
        <Audit {route} />
      {:else}
        <Overview />
      {/if}
    </main>
  </div>
{/if}

{#if palette && isAdmin}
  <CommandPalette {commands} onclose={() => (palette = false)} />
{/if}

<div class="toasts" aria-live="polite">
  {#each toasts as t (t.id)}
    <div class="toast" class:error={t.kind === "error"} role={t.kind === "error" ? "alert" : "status"}>{t.message}</div>
  {/each}
</div>

<style>
  .splash {
    height: 100%;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 8px;
  }
  .shell {
    display: grid;
    grid-template-columns: 196px 1fr;
    height: 100%;
  }
  main {
    min-width: 0;
    height: 100%;
    overflow: hidden;
  }
  .sidebar {
    display: flex;
    flex-direction: column;
    border-right: 1px solid var(--border);
    background: var(--panel);
    padding: 12px 10px;
    gap: 12px;
  }
  .brand {
    display: flex;
    align-items: center;
    gap: 8px;
    font-weight: 700;
    font-size: 15px;
    color: var(--text);
    padding: 4px 6px 8px;
    text-decoration: none;
  }
  .logo {
    display: inline-grid;
    place-items: center;
    width: 24px;
    height: 24px;
    border-radius: 6px;
    background: var(--accent);
    color: white;
    font-size: 13px;
  }
  ul {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 1px;
  }
  .groups {
    display: flex;
    flex-direction: column;
    gap: 2px;
    overflow: auto;
    min-height: 0;
  }
  .group-label {
    font-size: 10.5px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--faint);
    padding: 10px 8px 3px;
  }
  .palette-button {
    justify-content: space-between;
    color: var(--muted);
    background: var(--panel-2);
    border-color: var(--border);
    font-size: 13px;
  }
  ul a {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 5px 8px;
    border-radius: var(--radius);
    color: var(--text);
    text-decoration: none;
  }
  ul a:hover {
    background: var(--panel-2);
  }
  ul a.active {
    background: var(--accent-soft);
    color: var(--accent-text);
    font-weight: 600;
  }
  .icon {
    width: 16px;
    text-align: center;
    color: var(--muted);
  }
  a.active .icon {
    color: inherit;
  }
  .foot {
    margin-top: auto;
    display: flex;
    flex-direction: column;
    gap: 8px;
    font-size: 12.5px;
  }
  .conn {
    display: flex;
    align-items: center;
    gap: 6px;
    color: var(--muted);
  }
  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--faint);
  }
  .dot.on {
    background: var(--ok);
  }
  .who {
    color: var(--muted);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .toasts {
    position: fixed;
    bottom: 16px;
    right: 16px;
    display: flex;
    flex-direction: column;
    gap: 8px;
    z-index: 100;
  }
  .toast {
    background: var(--panel);
    border: 1px solid var(--border);
    box-shadow: var(--shadow);
    border-radius: 8px;
    padding: 8px 14px;
    max-width: 420px;
  }
  .toast.error {
    border-color: var(--err);
    color: var(--err);
  }
</style>

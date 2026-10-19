<script lang="ts">
  import { usePaginatedQuery, useQuery } from "@rxferrix/svelte";
  import { AUDIT_ACTIONS, audit, dashboard, type AuditEntry, type Json } from "../lib/api.js";
  import { dashboardContext } from "../lib/context.js";
  import { diffFields, summarize } from "../lib/diff.js";
  import { formatRelative, formatTime, pretty } from "../lib/format.js";
  import { href, router, type Route } from "../lib/router.svelte.js";
  import Empty from "../components/Empty.svelte";
  import IdLink from "../components/IdLink.svelte";
  import JsonValue from "../components/JsonValue.svelte";

  /**
   * The audit log (`_system/audit:list`): every dashboard write, settings change, impersonation and function run,
   * newest first and live. Filters live in the URL so entries can be linked to.
   */
  let { route }: { route: Route } = $props();
  const ctx = dashboardContext();
  const me = useQuery(dashboard.me, () => ({}));

  const PRESETS = [
    { id: "1h", label: "Last hour", ms: 3_600_000 },
    { id: "24h", label: "Last 24 hours", ms: 86_400_000 },
    { id: "7d", label: "Last 7 days", ms: 7 * 86_400_000 },
    { id: "30d", label: "Last 30 days", ms: 30 * 86_400_000 },
  ];

  const p = $derived(route.params);
  const admin = $derived(p.get("admin") || null);
  const action = $derived(p.get("action") || null);
  const table = $derived(p.get("table") || null);
  const target = $derived(p.get("target") || null);
  const range = $derived(p.get("range") || "");
  const fromText = $derived(p.get("from") || "");
  const toText = $derived(p.get("to") || "");

  /** A `datetime-local` value as ms (local time), or null. */
  function localMs(text: string): number | null {
    if (!text) return null;
    const t = new Date(text).getTime();
    return Number.isFinite(t) ? t : null;
  }

  // Presets are relative to when they were picked (a fixed window keeps the query stable).
  const presetSince = $derived.by(() => {
    const preset = PRESETS.find((x) => x.id === range);
    const at = Number(p.get("at"));
    return preset && Number.isFinite(at) ? at - preset.ms : null;
  });
  const since = $derived(range === "custom" ? localMs(fromText) : presetSince);
  const until = $derived(range === "custom" ? localMs(toText) : null);

  const entries = usePaginatedQuery(
    audit.list,
    () => ({ admin, action, table, target, since, until: until === null ? null : until + 59_999 }),
    { initialNumItems: 50 },
  );

  function setParams(next: Record<string, string | null>) {
    const params: Record<string, string | null> = {};
    for (const [k, v] of p.entries()) params[k] = v;
    router.navigate(href("audit", [], { ...params, ...next }), { replace: true });
  }

  let open = $state<Set<string>>(new Set());
  function toggle(id: string) {
    const next = new Set(open);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    open = next;
  }

  // Admins seen in the loaded entries (there is no admin list function), plus "me".
  const admins = $derived.by(() => {
    const out = new Map<string, string>();
    if (me.data) out.set(me.data._id, `${me.data.email} (you)`);
    for (const e of entries.results) if (e.admin && !out.has(e.admin)) out.set(e.admin, e.adminEmail ?? e.admin);
    if (admin && !out.has(admin)) out.set(admin, admin);
    return [...out.entries()];
  });
  const filtered = $derived(Boolean(admin || action || table || target || range));

  let now = $state(Date.now());
  $effect(() => {
    const t = setInterval(() => (now = Date.now()), 15_000);
    return () => clearInterval(t);
  });

  const actionLabels: Record<string, string> = {
    insert: "insert",
    patch: "patch",
    replace: "replace",
    delete: "delete",
    revoke_session: "revoke session",
    delete_user: "delete user",
    reset_mfa: "reset 2FA",
    mfa_enable: "2FA enabled",
    mfa_disable: "2FA disabled",
    settings_update: "settings",
    env_create: "env: add",
    env_update: "env: change",
    env_delete: "env: remove",
    impersonate: "impersonate",
    run_function: "run function",
  };

  function details(e: AuditEntry): string {
    const d = e.details as Record<string, Json> | null;
    if (!d || typeof d !== "object" || Array.isArray(d)) return "";
    if (e.action === "run_function") return `${d.path ?? ""}${d.asUser ? ` as ${d.asUser}` : ""} · ${d.outcome ?? ""}`;
    if (e.action === "settings_update" && Array.isArray(d.keys)) return (d.keys as string[]).join(", ");
    if (e.action.startsWith("env_") && typeof d.name === "string") return d.name;
    if (e.action === "impersonate") return `${d.purpose ?? ""}${d.expiresAt ? ` · until ${formatTime(d.expiresAt as number)}` : ""}`;
    return "";
  }
</script>

<div class="page">
  <header class="page-header">
    <h1>Audit log</h1>
    <span class="muted small">Every change made from the dashboard, written in the same transaction; kept 90 days.</span>
  </header>
  <div class="filters row" role="search" aria-label="Filter the audit log">
    <select aria-label="Administrator" value={admin ?? ""} onchange={(e) => setParams({ admin: e.currentTarget.value || null })}>
      <option value="">All administrators</option>
      {#each admins as [id, label] (id)}<option value={id}>{label}</option>{/each}
    </select>
    <select aria-label="Action" value={action ?? ""} onchange={(e) => setParams({ action: e.currentTarget.value || null })}>
      <option value="">All actions</option>
      {#each AUDIT_ACTIONS as a (a)}<option value={a}>{actionLabels[a] ?? a}</option>{/each}
    </select>
    <select aria-label="Table" value={table ?? ""} onchange={(e) => setParams({ table: e.currentTarget.value || null })}>
      <option value="">All tables</option>
      {#each ctx.tables ?? [] as t (t.name)}<option value={t.name}>{t.name}</option>{/each}
    </select>
    <input
      class="mono target"
      placeholder="Target document id"
      aria-label="Target id"
      value={target ?? ""}
      onchange={(e) => setParams({ target: e.currentTarget.value.trim() || null })}
    />
    <select
      aria-label="Time range"
      value={range}
      onchange={(e) => {
        const r = e.currentTarget.value;
        setParams({ range: r || null, at: r && r !== "custom" ? String(Date.now()) : null });
      }}
    >
      <option value="">Any time</option>
      {#each PRESETS as x (x.id)}<option value={x.id}>{x.label}</option>{/each}
      <option value="custom">Custom…</option>
    </select>
    {#if range === "custom"}
      <label class="inline"
        >from <input type="datetime-local" aria-label="From" value={fromText} onchange={(e) => setParams({ from: e.currentTarget.value || null })} /></label
      >
      <label class="inline"
        >to <input type="datetime-local" aria-label="To" value={toText} onchange={(e) => setParams({ to: e.currentTarget.value || null })} /></label
      >
    {/if}
    {#if filtered}
      <button class="small ghost" onclick={() => router.navigate(href("audit"), { replace: true })}>Clear filters</button>
    {/if}
  </div>
  <div class="body">
    {#if entries.error}
      <div class="alert" role="alert">{entries.error.message}</div>
    {:else if entries.isLoading && entries.results.length === 0}
      <Empty title="Loading…" />
    {:else if entries.results.length === 0}
      <Empty
        title={filtered ? "No entries match these filters" : "No audit entries yet"}
        hint={filtered
          ? "Change or clear the filters."
          : "Entries appear here when an administrator edits documents, revokes sessions, changes settings, impersonates a user or runs a function."}
      />
    {:else}
      <table class="grid" data-testid="audit">
        <thead>
          <tr><th>When</th><th>Administrator</th><th>Action</th><th>Target</th><th>Changes</th></tr>
        </thead>
        <tbody>
          {#each entries.results as e (e._id)}
            {@const changes = diffFields(e.before, e.after)}
            <tr onclick={() => toggle(e._id)} class:selected={open.has(e._id)} aria-expanded={open.has(e._id)}>
              <td title={formatTime(e.at)}>{formatRelative(e.at, now)}</td>
              <td class="mono">{e.adminEmail ?? e.admin ?? "—"}</td>
              <td><span class="badge act {e.action}">{actionLabels[e.action] ?? e.action}</span></td>
              <td>
                {#if e.targetTable}<span class="muted">{e.targetTable}</span>{/if}
                {#if e.targetId}<span onclick={(ev) => ev.stopPropagation()} role="presentation"><IdLink id={e.targetId} /></span>{/if}
              </td>
              <td class="muted">{details(e) || (e.before !== null || e.after !== null ? summarize(changes) : "")}</td>
            </tr>
            {#if open.has(e._id)}
              <tr class="detail">
                <td colspan="5">
                  {#if changes.length > 0}
                    <table class="diff" data-testid="audit-diff">
                      <thead><tr><th>Field</th><th>Before</th><th>After</th></tr></thead>
                      <tbody>
                        {#each changes as c (c.field)}
                          <tr class={c.kind}>
                            <td class="mono">{c.field}</td>
                            <td class="before">{#if c.before !== undefined}<JsonValue value={c.before} field={c.field} max={200} />{/if}</td>
                            <td class="after">{#if c.after !== undefined}<JsonValue value={c.after} field={c.field} max={200} />{/if}</td>
                          </tr>
                        {/each}
                      </tbody>
                    </table>
                  {/if}
                  {#if e.details !== null}
                    <h3>Details</h3>
                    <pre>{pretty(e.details)}</pre>
                  {/if}
                  <div class="meta faint">
                    {formatTime(e.at)} · entry <span class="mono">{e._id}</span>
                    {#if e.targetId}· <a href={href("audit", [], { target: e.targetId })} onclick={(ev) => {
                      ev.preventDefault();
                      setParams({ target: e.targetId });
                    }}>history of this document</a>{/if}
                  </div>
                </td>
              </tr>
            {/if}
          {/each}
        </tbody>
      </table>
      <div class="more">
        {#if entries.status === "CanLoadMore"}<button onclick={() => entries.loadMore(50)}>Load more</button>
        {:else if entries.status === "LoadingMore"}<span class="faint">Loading…</span>
        {:else}<span class="faint">{entries.results.length} entries · end of the log</span>{/if}
      </div>
    {/if}
  </div>
</div>

<style>
  .small {
    font-size: 12.5px;
  }
  .filters {
    padding: 8px 20px;
    border-bottom: 1px solid var(--border);
    background: var(--panel);
  }
  .target {
    width: 240px;
  }
  .body {
    flex: 1;
    overflow: auto;
  }
  .badge.act.insert {
    color: var(--ok);
    background: var(--ok-soft);
    border-color: transparent;
  }
  .badge.act.delete,
  .badge.act.delete_user,
  .badge.act.revoke_session {
    color: var(--err);
    background: var(--err-soft);
    border-color: transparent;
  }
  .badge.act.patch,
  .badge.act.replace,
  .badge.act.settings_update,
  .badge.act.env_create,
  .badge.act.env_update {
    color: var(--link);
  }
  .badge.act.impersonate {
    color: var(--warn);
    background: var(--warn-soft);
    border-color: transparent;
  }
  tr.detail {
    cursor: default;
    background: var(--panel-2);
  }
  tr.detail > td {
    max-width: none;
    white-space: normal;
    padding: 8px 16px 14px;
  }
  pre {
    background: var(--panel);
    padding: 8px;
    border-radius: var(--radius);
    max-height: 240px;
    overflow: auto;
  }
  .meta {
    font-size: 12px;
    margin-top: 6px;
  }
  .more {
    padding: 12px 20px;
  }
  @media (max-width: 760px) {
    .target {
      width: 100%;
    }
  }
</style>

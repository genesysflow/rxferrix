<script lang="ts">
  import { onMount } from "svelte";
  import type { Json, OlapResult, OlapStatus } from "../lib/api.js";
  import { getOlap, olapExport, olapQuery, olapRebuild, saveBlob } from "../lib/admin.js";
  import { dashboardContext } from "../lib/context.js";
  import { formatBytes, formatSpan } from "../lib/format.js";
  import Empty from "../components/Empty.svelte";

  /** SQL over the analytics copy (`--olap`): the same sandbox as `ctx.olap` (one SELECT, read-only), audited. */
  const ctx = dashboardContext();
  const STORE = "ferrix.analytics.sql";

  let status = $state<OlapStatus | null | undefined>(undefined);
  let loadError = $state<string | null>(null);
  let sql = $state(localStorage.getItem(STORE) ?? "");
  let result = $state<OlapResult | null>(null);
  let error = $state<string | null>(null);
  let running = $state<string | null>(null);
  let took = $state<number | null>(null);
  let open = $state<string | null>(null);

  async function refresh() {
    try {
      status = await getOlap(ctx.auth);
      loadError = null;
    } catch (e) {
      loadError = (e as Error).message;
    }
  }

  onMount(() => {
    void refresh();
    const t = setInterval(refresh, 5000);
    return () => clearInterval(t);
  });

  $effect(() => {
    localStorage.setItem(STORE, sql);
  });

  async function run() {
    if (!sql.trim() || running) return;
    running = "query";
    error = null;
    const started = performance.now();
    try {
      result = await olapQuery(ctx.auth, sql);
      took = performance.now() - started;
    } catch (e) {
      error = (e as Error).message;
      result = null;
    } finally {
      running = null;
    }
  }

  async function download(format: "csv" | "parquet") {
    if (!sql.trim() || running) return;
    running = format;
    error = null;
    try {
      saveBlob(await olapExport(ctx.auth, sql, format));
    } catch (e) {
      error = (e as Error).message;
    } finally {
      running = null;
    }
  }

  async function rebuild() {
    if (!confirm("Rebuild every table of the analytics copy from a snapshot? Queries fail until it is done.")) return;
    try {
      await olapRebuild(ctx.auth);
      ctx.toast("Rebuilding the analytics copy");
      await refresh();
    } catch (e) {
      ctx.toast((e as Error).message, "error");
    }
  }

  function onkeydown(e: KeyboardEvent) {
    if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
      e.preventDefault();
      void run();
    }
  }

  function quoted(name: string): string {
    return /^[a-z_][a-z0-9_]*$/.test(name) ? name : `"${name.replace(/"/g, '""')}"`;
  }

  function insert(text: string) {
    sql = sql.trim() ? `${sql.trimEnd()} ${text}` : text;
  }

  function cell(v: Json | undefined): string {
    if (v === null || v === undefined) return "";
    return typeof v === "object" ? JSON.stringify(v) : String(v);
  }
</script>

<div class="page">
  <header class="page-header">
    <h1>Analytics</h1>
    <span class="muted small">Read-only SQL (DuckDB) over the analytics copy, the same sandbox as <code>ctx.olap</code>. Queries are audited.</span>
    <span class="spacer"></span>
    {#if status}<button onclick={rebuild}>Rebuild copy</button>{/if}
  </header>
  <div class="page-body">
    {#if loadError}
      <div class="alert" role="alert">{loadError}</div>
    {:else if status === undefined}
      <Empty title="Loading…" />
    {:else if status === null}
      <Empty
        title="Analytics are off"
        hint="Start the server with --olap (a binary built with the olap cargo feature) to keep a columnar copy of the data in DuckDB, queried from actions with ctx.olap and here."
      />
    {:else}
      <div class="status" data-testid="olap-status">
        <span class="badge" class:ok={status.state === "ready"} class:bad={status.state === "failed"}>{status.state}</span>
        <span>commit {status.appliedTs}{status.latestTs > status.appliedTs ? ` of ${status.latestTs}` : ""}</span>
        {#if status.lagMs > 0}<span class="muted">{formatSpan(status.lagMs)} behind</span>{/if}
        <span class="muted">· {status.tables.reduce((n, t) => n + t.rows, 0)} rows in {status.tables.length} tables · {formatBytes(status.sizeBytes)}</span>
        {#if status.detail}<span class="faint">· {status.detail}</span>{/if}
      </div>
      <div class="layout">
        <aside class="tables" aria-label="Tables">
          {#each status.tables as t (t.name)}
            <div class="table">
              <button class="link name" onclick={() => (open = open === t.name ? null : t.name)} title="Show columns">
                {open === t.name ? "▾" : "▸"} {t.name}
              </button>
              <span class="faint small">{t.rows}</span>
              <button class="link small" onclick={() => (sql = `SELECT * FROM ${quoted(t.name)} LIMIT 100`)} title="Query it">query</button>
            </div>
            {#if open === t.name}
              <ul class="columns">
                {#each t.columns as c (c.name)}
                  <li><button class="link" onclick={() => insert(quoted(c.name))}>{c.name}</button> <span class="faint">{c.typeName}</span></li>
                {/each}
              </ul>
            {/if}
          {/each}
        </aside>
        <section class="main">
          <textarea
            aria-label="SQL"
            spellcheck="false"
            rows="7"
            placeholder="SELECT customer, count(*) AS n FROM orders GROUP BY customer ORDER BY n DESC"
            bind:value={sql}
            {onkeydown}
          ></textarea>
          <div class="row actions">
            <button class="primary" onclick={run} disabled={!sql.trim() || running !== null}>{running === "query" ? "Running…" : "Run"}</button>
            <span class="faint small">Ctrl+Enter</span>
            <span class="spacer"></span>
            <button onclick={() => download("csv")} disabled={!sql.trim() || running !== null}>{running === "csv" ? "Exporting…" : "Download CSV"}</button>
            <button onclick={() => download("parquet")} disabled={!sql.trim() || running !== null}
              >{running === "parquet" ? "Exporting…" : "Download Parquet"}</button
            >
          </div>
          {#if error}
            <div class="alert" role="alert" data-testid="olap-error">{error}</div>
          {/if}
          {#if result}
            <div class="muted small" data-testid="olap-summary">
              {result.rows.length} rows · as of commit {result.asOfTs}{took !== null ? ` · ${formatSpan(took)}` : ""}
            </div>
            <div class="results">
              <table data-testid="olap-results">
                <thead>
                  <tr>
                    {#each result.columns as c (c.name)}<th>{c.name} <span class="faint">{c.typeName}</span></th>{/each}
                  </tr>
                </thead>
                <tbody>
                  {#each result.rows as row, i (i)}
                    <tr>
                      {#each result.columns as c (c.name)}
                        <td class:null={row[c.name] === null}>{row[c.name] === null ? "null" : cell(row[c.name])}</td>
                      {/each}
                    </tr>
                  {/each}
                </tbody>
              </table>
            </div>
          {/if}
        </section>
      </div>
    {/if}
  </div>
</div>

<style>
  .status {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
    align-items: center;
    margin-bottom: 12px;
  }
  .badge.ok {
    color: var(--ok, #2a7);
  }
  .badge.bad {
    color: var(--danger, #c33);
  }
  .layout {
    display: grid;
    grid-template-columns: 240px 1fr;
    gap: 16px;
    align-items: start;
  }
  .tables {
    border: 1px solid var(--border);
    border-radius: var(--radius);
    padding: 8px;
    max-height: 70vh;
    overflow: auto;
    font-size: 13px;
  }
  .table {
    display: flex;
    gap: 6px;
    align-items: baseline;
  }
  .table .name {
    flex: 1;
    text-align: left;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .columns {
    margin: 2px 0 6px;
    padding-left: 18px;
    font-size: 12px;
  }
  .main {
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  textarea {
    font-family: var(--mono, monospace);
    font-size: 13px;
    width: 100%;
    resize: vertical;
  }
  .actions {
    align-items: center;
    gap: 8px;
  }
  .results {
    overflow: auto;
    max-height: 60vh;
    border: 1px solid var(--border);
    border-radius: var(--radius);
  }
  table {
    border-collapse: collapse;
    font-size: 12px;
    width: max-content;
    min-width: 100%;
  }
  th,
  td {
    padding: 4px 8px;
    border-bottom: 1px solid var(--border);
    text-align: left;
    white-space: nowrap;
    max-width: 480px;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  th {
    position: sticky;
    top: 0;
    background: var(--panel);
  }
  td.null {
    color: var(--faint, #999);
    font-style: italic;
  }
  @media (max-width: 760px) {
    .layout {
      grid-template-columns: 1fr;
    }
    .main {
      order: -1;
    }
    .tables {
      max-height: 40vh;
    }
  }
</style>

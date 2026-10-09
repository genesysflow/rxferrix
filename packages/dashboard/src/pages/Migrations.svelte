<script lang="ts">
  import { FerrixError } from "@ferrix/client";
  import { useAction, useMutation, useQuery } from "@ferrix/svelte";
  import { migrations, type MigrationReport, type MigrationState, type MigrationSummary } from "../lib/api.js";
  import { dashboardContext } from "../lib/context.js";
  import { diffFields } from "../lib/diff.js";
  import { formatRelative, formatTime } from "../lib/format.js";
  import { href, link } from "../lib/router.svelte.js";
  import Empty from "../components/Empty.svelte";
  import IdLink from "../components/IdLink.svelte";
  import JsonValue from "../components/JsonValue.svelte";
  import Modal from "../components/Modal.svelte";

  /** `#[migration]`s: live progress, run / cancel / restart, dry runs with before/after examples. */
  const ctx = dashboardContext();
  const list = useQuery(migrations.list, () => ({}));
  const run = useAction(migrations.run);
  const cancel = useMutation(migrations.cancel);

  let busy = $state<string | null>(null);
  let report = $state<{ title: string; reports: MigrationReport[] } | null>(null);
  let now = $state(Date.now());
  $effect(() => {
    const t = setInterval(() => (now = Date.now()), 5000);
    return () => clearInterval(t);
  });

  const rows = $derived(list.data ?? []);
  const migratingTables = $derived([...new Set(rows.filter((m) => m.tableMigrating).map((m) => m.table))]);
  const notDone = $derived(rows.filter((m) => m.declared && m.state !== "done" && m.state !== "running"));

  const stateLabels: Record<MigrationState, string> = {
    pending: "pending",
    running: "running",
    done: "done",
    failed: "failed",
    canceled: "canceled",
  };

  function progress(m: MigrationSummary): number {
    if (m.state === "done") return 1;
    // Documents inserted during a run come after the cursor, so processed can exceed the count taken earlier.
    return m.documents > 0 ? Math.min(1, m.processed / m.documents) : 0;
  }

  async function start(m: MigrationSummary | null, options: { restart?: boolean } = {}) {
    const name = m?.name ?? null;
    if (options.restart && !confirm(`Run ${name} again from the first document? Documents already in the new shape are left alone by a well-written migration.`)) return;
    busy = name ?? "*";
    try {
      const r = await run({ name, restart: options.restart ?? false });
      if (r.started.length) ctx.toast(`Started ${r.started.join(", ")}`);
      for (const rep of r.reports) ctx.toast(`${rep.name}: ${rep.outcome === "alreadyDone" ? "already done" : rep.outcome === "alreadyRunning" ? "already running" : rep.outcome}`);
      if (!r.started.length && !r.reports.length) ctx.toast("Nothing to run");
    } catch (e) {
      ctx.toast(e instanceof FerrixError ? e.message : String(e), "error");
    } finally {
      busy = null;
    }
  }

  async function dryRun(m: MigrationSummary | null) {
    busy = `dry:${m?.name ?? "*"}`;
    try {
      const r = await run({ name: m?.name ?? null, dryRun: true });
      report = { title: m ? `Dry run of ${m.name}` : "Dry run of every pending migration", reports: r.reports };
    } catch (e) {
      ctx.toast(e instanceof FerrixError ? e.message : String(e), "error");
    } finally {
      busy = null;
    }
  }

  async function stop(m: MigrationSummary) {
    try {
      const outcome = await cancel({ name: m.name });
      ctx.toast(outcome === "canceled" ? `${m.name} stops after its current batch` : `${m.name} is not running`);
    } catch (e) {
      ctx.toast(e instanceof FerrixError ? e.message : String(e), "error");
    }
  }

  const outcomeLabels: Record<string, string> = {
    dryRun: "would complete",
    failed: "would fail",
    completed: "completed",
    canceled: "canceled",
    alreadyDone: "already done",
    alreadyRunning: "already running",
    stopped: "stopped",
  };
</script>

<div class="page">
  <header class="page-header">
    <h1>Migrations</h1>
    <span class="muted small">Batched, resumable and exactly once per run; each batch is one transaction.</span>
    <span class="spacer"></span>
    {#if notDone.length > 0}
      <button onclick={() => dryRun(null)} disabled={busy !== null}>{busy === "dry:*" ? "Running…" : "Dry run all"}</button>
      <button class="primary" onclick={() => start(null)} disabled={busy !== null}>Run {notDone.length} pending</button>
    {/if}
  </header>
  <div class="page-body">
    {#if migratingTables.length > 0}
      <div class="migrating" role="status" data-testid="migrating-banner">
        <strong>Migrating: {migratingTables.join(", ")}.</strong>
        These tables opened with documents that don't match their type. Writes are lenient (a document that didn't match
        may stay invalid) and typed reads of old documents fail until their migrations finish; run them soon.
      </div>
    {/if}
    {#if list.error}
      <div class="alert" role="alert">{list.error.message}</div>
    {:else if list.isLoading}
      <Empty title="Loading…" />
    {:else if rows.length === 0}
      <Empty
        title="No migrations"
        hint="Declare one with #[migration(table = …)] on an async fn that returns a patch for each document. It shows here as pending until it runs (serve --migrate, ferrix migrate, or the Run button)."
      />
    {:else}
      <table class="grid" data-testid="migrations">
        <thead>
          <tr><th>Migration</th><th>Table</th><th>State</th><th class="prog">Progress</th><th>Changed</th><th>Schema check</th><th>Finished</th><th></th></tr>
        </thead>
        <tbody>
          {#each rows as m (m.name)}
            {@const pct = progress(m)}
            <tr class:migrating-row={m.tableMigrating} class:failed={m.state === "failed"}>
              <td>
                <span class="mono name">{m.name}</span>
                {#if !m.declared}<span class="badge" title="Recorded in _migrations but no longer declared in code">not in code</span>{/if}
                {#if m.batchSize}<div class="faint tiny">batches of {m.batchSize}</div>{/if}
              </td>
              <td>
                <a href={href("data", [m.table])} onclick={link}>{m.table}</a>
                {#if m.tableMigrating}<span class="badge mig" title="Lenient writes until its migrations finish">migrating</span>{/if}
              </td>
              <td><span class="badge mstate {m.state}">{stateLabels[m.state]}</span></td>
              <td class="prog">
                <div class="bar" role="progressbar" aria-valuemin="0" aria-valuemax={Math.max(m.documents, m.processed)} aria-valuenow={m.processed} aria-label={`${m.name} progress`}>
                  <div class="fill {m.state}" style:width={`${(pct * 100).toFixed(1)}%`}></div>
                </div>
                <div class="tiny muted">{m.processed} / {m.documents} documents{m.state === "running" ? ` · ${Math.round(pct * 100)}%` : ""}</div>
              </td>
              <td>{m.changed}</td>
              <td>
                {#if m.schemaCheck === null}<span class="faint">—</span>
                {:else if m.schemaCheck === "passed"}<span class="badge success">passed</span>
                {:else}<span class="badge serverError" title={m.schemaCheck}>failed</span>{/if}
              </td>
              <td title={m.finishedAt ? formatTime(m.finishedAt) : m.startedAt ? `started ${formatTime(m.startedAt)}` : ""}>
                {m.finishedAt ? formatRelative(m.finishedAt, now) : m.startedAt ? `started ${formatRelative(m.startedAt, now)}` : "never run"}
              </td>
              <td class="actions">
                {#if m.declared}
                  {#if m.state === "running"}
                    <button class="small danger" onclick={() => stop(m)}>Cancel</button>
                  {:else}
                    <button class="small" onclick={() => dryRun(m)} disabled={busy !== null}>{busy === `dry:${m.name}` ? "Running…" : "Dry run"}</button>
                    {#if m.state === "done"}
                      <button class="small" onclick={() => start(m, { restart: true })} disabled={busy !== null}>Restart</button>
                    {:else}
                      <button class="small primary" onclick={() => start(m)} disabled={busy !== null}>{m.state === "pending" ? "Run" : "Resume"}</button>
                    {/if}
                  {/if}
                {/if}
              </td>
            </tr>
            {#if m.error || (m.schemaCheck && m.schemaCheck !== "passed")}
              <tr class="detail">
                <td colspan="8">
                  {#if m.error}
                    <div class="error-text"><strong>Error:</strong> <span class="pre">{m.error}</span></div>
                    {#if m.failedDocument}
                      <div class="muted">Failing document: <IdLink id={m.failedDocument} /> · <a href={href("data", [m.table], { doc: m.failedDocument })} onclick={link}>open in the data browser</a></div>
                    {/if}
                  {/if}
                  {#if m.schemaCheck && m.schemaCheck !== "passed"}
                    <div class="error-text"><strong>Schema check:</strong> <span class="pre">{m.schemaCheck}</span></div>
                  {/if}
                </td>
              </tr>
            {/if}
          {/each}
        </tbody>
      </table>
    {/if}
  </div>
</div>

{#if report}
  <Modal title={report.title} wide onclose={() => (report = null)}>
    {#if report.reports.length === 0}
      <p class="muted">Nothing to run: every migration is done.</p>
    {/if}
    {#each report.reports as r (r.name)}
      <section class="report" data-testid="dry-run-report">
        <h3 class="rh">
          <span class="mono">{r.name}</span>
          <span class="badge {r.outcome === 'failed' ? 'serverError' : 'success'}">{outcomeLabels[r.outcome] ?? r.outcome}</span>
        </h3>
        <p>
          <strong>{r.processed}</strong> documents processed in {r.batches} batch{r.batches === 1 ? "" : "es"};
          <strong>{r.changed}</strong> would change. Nothing was written.
        </p>
        {#if r.error}
          <div class="alert">{r.error}{#if r.failedDocument}{"\n"}Document: {r.failedDocument}{/if}</div>
        {/if}
        {#if r.examples.length > 0}
          <h4>Examples (up to 5)</h4>
          {#each r.examples as ex (ex.id)}
            {@const changes = diffFields(ex.before, ex.after).filter((c) => c.kind !== "same")}
            <div class="example">
              <div class="muted"><IdLink id={ex.id} />{#if ex.after === null} · <span class="error-text">would be deleted</span>{/if}</div>
              {#if ex.after !== null}
                <table class="diff">
                  <thead><tr><th>Field</th><th>Before</th><th>After</th></tr></thead>
                  <tbody>
                    {#each changes as c (c.field)}
                      <tr class={c.kind}>
                        <td class="mono">{c.field}</td>
                        <td class="before">{#if c.before !== undefined}<JsonValue value={c.before} field={c.field} max={120} />{/if}</td>
                        <td class="after">{#if c.after !== undefined}<JsonValue value={c.after} field={c.field} max={120} />{/if}</td>
                      </tr>
                    {/each}
                  </tbody>
                </table>
              {/if}
            </div>
          {/each}
        {/if}
      </section>
    {/each}
    {#snippet actions()}
      <button onclick={() => (report = null)}>Close</button>
    {/snippet}
  </Modal>
{/if}

<style>
  .small {
    font-size: 12.5px;
  }
  .tiny {
    font-size: 11.5px;
  }
  .page-body {
    padding: 0;
  }
  .migrating {
    margin: 14px 20px 0;
    padding: 10px 12px;
    border-radius: var(--radius);
    background: var(--warn-soft);
    color: var(--warn);
  }
  table.grid tbody tr {
    cursor: default;
  }
  table.grid td {
    vertical-align: middle;
  }
  .name {
    font-weight: 600;
  }
  .prog {
    width: 220px;
  }
  .bar {
    height: 8px;
    border-radius: 4px;
    background: var(--panel-2);
    border: 1px solid var(--border);
    overflow: hidden;
  }
  .fill {
    height: 100%;
    background: var(--series-1);
    border-radius: 4px;
  }
  .fill.done {
    background: var(--ok);
  }
  .fill.failed {
    background: var(--err);
  }
  .fill.canceled {
    background: var(--faint);
  }
  .badge.mstate.running {
    color: var(--link);
  }
  .badge.mstate.done {
    color: var(--ok);
    background: var(--ok-soft);
    border-color: transparent;
  }
  .badge.mstate.failed {
    color: var(--err);
    background: var(--err-soft);
    border-color: transparent;
  }
  .badge.mig {
    margin-left: 6px;
    color: var(--warn);
    background: var(--warn-soft);
    border-color: transparent;
  }
  tr.migrating-row td:first-child {
    box-shadow: inset 3px 0 var(--warn);
  }
  tr.detail td {
    max-width: none;
    white-space: normal;
    background: var(--panel-2);
    padding: 6px 16px 10px;
  }
  .pre {
    white-space: pre-wrap;
    font-family: var(--mono);
    font-size: 12.5px;
  }
  .report + .report {
    border-top: 1px solid var(--border);
    margin-top: 12px;
  }
  .rh {
    display: flex;
    gap: 8px;
    align-items: center;
    text-transform: none;
    letter-spacing: 0;
    font-size: 14px;
    color: var(--text);
  }
  h4 {
    margin: 10px 0 4px;
    font-size: 12.5px;
    color: var(--muted);
  }
  .example {
    margin: 6px 0 10px;
  }
</style>

<script lang="ts">
  import { FerrixError } from "@ferrix/client";
  import { useMutation, useQuery } from "@ferrix/svelte";
  import { onMount } from "svelte";
  import { scheduler, type BackupInfo, type ExportFormat, type ImportMode, type ImportReport } from "../lib/api.js";
  import { createBackup, deleteBackup, downloadBackup, exportTable, importTable, listBackups, saveBlob } from "../lib/admin.js";
  import { dashboardContext } from "../lib/context.js";
  import { formatBytes, formatRelative, formatTime } from "../lib/format.js";
  import { href, link } from "../lib/router.svelte.js";
  import Empty from "../components/Empty.svelte";

  /** Backups (`/api/admin/backups*`), per-table export and JSONL import (`/api/admin/export|import/{table}`). */
  const ctx = dashboardContext();
  const crons = useQuery(scheduler.crons, () => ({}));
  const runCronNow = useMutation(scheduler.runCronNow);
  const schedule = $derived(crons.data?.find((c) => c.name === "ferrix-backup") ?? null);
  const scheduleArgs = $derived.by(() => {
    const a = schedule?.args as { schedule?: { to?: string | null; keep?: number; includeAux?: boolean; includeFiles?: boolean } } | null;
    return a?.schedule ?? null;
  });

  let target = $state<string | null>(null);
  let backups = $state<BackupInfo[] | null>(null);
  let listError = $state<string | null>(null);
  let includeAux = $state(false);
  let includeFiles = $state(true);
  let creating = $state(false);
  let busyName = $state<string | null>(null);
  /** Snapshot ts of the backups created from this page (the listing doesn't know them). */
  let knownTs = $state<Record<string, number>>({});
  let now = $state(Date.now());

  async function refresh() {
    try {
      const r = await listBackups(ctx.auth);
      target = r.target;
      backups = r.backups;
      listError = null;
    } catch (e) {
      listError = e instanceof Error ? e.message : String(e);
    }
  }
  onMount(() => {
    void refresh();
    // Scheduled backups appear without a reload.
    const t = setInterval(() => {
      now = Date.now();
      void refresh();
    }, 15_000);
    return () => clearInterval(t);
  });
  // A scheduled run that finished: list again.
  $effect(() => {
    if (schedule?.lastJobState === "success") void refresh();
  });

  async function create() {
    creating = true;
    try {
      const b = await createBackup(ctx.auth, { includeAux, includeFiles });
      if (b.ts !== null) knownTs = { ...knownTs, [b.name]: b.ts };
      ctx.toast(`Backup ${b.name} stored (${formatBytes(b.bytes)})`);
      await refresh();
    } catch (e) {
      ctx.toast(`Backup failed: ${e instanceof Error ? e.message : String(e)}`, "error");
    } finally {
      creating = false;
    }
  }

  async function download(b: BackupInfo) {
    busyName = b.name;
    try {
      saveBlob(await downloadBackup(ctx.auth, b.name));
    } catch (e) {
      ctx.toast(`Download failed: ${e instanceof Error ? e.message : String(e)}`, "error");
    } finally {
      busyName = null;
    }
  }

  async function remove(b: BackupInfo) {
    if (!confirm(`Delete the backup ${b.name}? This can't be undone.`)) return;
    busyName = b.name;
    try {
      await deleteBackup(ctx.auth, b.name);
      ctx.toast(`Deleted ${b.name}`);
      await refresh();
    } catch (e) {
      ctx.toast(e instanceof Error ? e.message : String(e), "error");
    } finally {
      busyName = null;
    }
  }

  async function runScheduledNow() {
    try {
      await runCronNow({ name: "ferrix-backup" });
      ctx.toast("Scheduled backup started");
    } catch (e) {
      ctx.toast(e instanceof FerrixError ? e.message : String(e), "error");
    }
  }

  // Export
  const tables = $derived(ctx.tables ?? []);
  const appTables = $derived(tables.filter((t) => !t.system));
  let exportName = $state("");
  let exportFormat = $state<ExportFormat>("jsonl");
  let exporting = $state(false);
  $effect(() => {
    if (!exportName && appTables[0]) exportName = appTables[0].name;
  });

  async function runExport() {
    if (!exportName) return;
    exporting = true;
    try {
      const d = await exportTable(ctx.auth, exportName, exportFormat);
      saveBlob(d);
      ctx.toast(`Exported ${exportName} (${formatBytes(d.blob.size)})`);
    } catch (e) {
      ctx.toast(`Export failed: ${e instanceof Error ? e.message : String(e)}`, "error");
    } finally {
      exporting = false;
    }
  }

  // Import
  let importName = $state("");
  let importMode = $state<ImportMode>("insert");
  let importFile = $state<File | null>(null);
  let importing = $state(false);
  let importResult = $state<{ ok: true; report: ImportReport } | { ok: false; error: string } | null>(null);
  $effect(() => {
    if (!importName && appTables[0]) importName = appTables[0].name;
  });
  const importTable_ = $derived(appTables.find((t) => t.name === importName));

  async function runImport() {
    if (!importName || !importFile) return;
    if (importMode === "replace" && !confirm(`Replace mode deletes every document of ${importName} (${importTable_?.documents ?? "?"}) before importing. Continue?`)) return;
    importing = true;
    importResult = null;
    try {
      const report = await importTable(ctx.auth, importName, importMode, importFile);
      importResult = { ok: true, report };
      ctx.toast(`Imported into ${importName}`);
    } catch (e) {
      importResult = { ok: false, error: e instanceof Error ? e.message : String(e) };
    } finally {
      importing = false;
    }
  }

  const modeHelp: Record<ImportMode, string> = {
    insert: "Add documents; fails if an id already exists.",
    upsert: "Add new documents and replace those whose id exists.",
    replace: "Delete every document of the table first, then add these.",
  };
</script>

<div class="page">
  <header class="page-header">
    <h1>Backups and data</h1>
    <span class="spacer"></span>
    <button onclick={refresh}>Refresh</button>
  </header>
  <div class="page-body">
    <section class="card section" aria-labelledby="b-head">
      <div class="head row">
        <h2 id="b-head">Backups</h2>
        <span class="spacer"></span>
        <label class="inline"><input type="checkbox" bind:checked={includeAux} /> include the execution log (aux.db)</label>
        <label class="inline"><input type="checkbox" bind:checked={includeFiles} /> include stored files</label>
        <button class="primary" onclick={create} disabled={creating}>{creating ? "Backing up…" : "Create backup"}</button>
      </div>
      <dl class="facts">
        <dt>Stored in</dt>
        <dd class="mono" data-testid="backup-target">{target ?? "…"}</dd>
        <dt>Schedule</dt>
        <dd data-testid="backup-schedule">
          {#if schedule}
            every <strong>{schedule.schedule.replace(/^every\s+/, "")}</strong>{scheduleArgs?.keep ? `, keeping the last ${scheduleArgs.keep}` : ""}{scheduleArgs?.includeAux ? ", with aux.db" : ""}{scheduleArgs?.includeFiles ? ", with files" : ""}
            · next {schedule.nextRun ? formatRelative(schedule.nextRun, now) : "—"}
            {#if schedule.lastJobState}· last run <span class="badge">{schedule.lastJobState}</span>{/if}
            · <button class="small" onclick={runScheduledNow}>Run now</button>
            · <a href={href("scheduler")} onclick={link}>cron <code>ferrix-backup</code></a>
          {:else}
            <span class="muted">No scheduled backups. Start the server with <code>--backup-every 6h --backup-keep 7</code> (optionally <code>--backup-to s3://bucket/prefix</code>).</span>
          {/if}
        </dd>
      </dl>
      <p class="faint small">
        A backup is a consistent snapshot taken while the server runs (writes are not blocked). It never contains
        <code>auth_secret.key</code>: settings secrets restore only with the same key file. Restore with
        <code>ferrix restore &lt;archive&gt;</code> while the server is stopped.
      </p>
      {#if listError}
        <div class="alert" role="alert">{listError}</div>
      {:else if backups === null}
        <p class="faint">Loading…</p>
      {:else if backups.length === 0}
        <Empty title="No backups yet" hint="Create one now, or schedule them with --backup-every." />
      {:else}
        <table class="grid" data-testid="backups">
          <thead><tr><th>Archive</th><th>Size</th><th>Created</th><th>Kind</th><th>Commit ts</th><th></th></tr></thead>
          <tbody>
            {#each backups as b (b.name)}
              <tr>
                <td class="mono">{b.name}</td>
                <td>{formatBytes(b.bytes)}</td>
                <td title={formatTime(b.createdAt)}>{formatRelative(b.createdAt, now)}</td>
                <td>{#if b.scheduled}<span class="badge" title="Rotated by --backup-keep">scheduled</span>{:else}<span class="badge">manual</span>{/if}</td>
                <td class="mono faint">{b.ts ?? knownTs[b.name] ?? "—"}</td>
                <td class="actions">
                  <button class="small" onclick={() => download(b)} disabled={busyName === b.name}>Download</button>
                  <button class="small danger" onclick={() => remove(b)} disabled={busyName === b.name}>Delete</button>
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      {/if}
    </section>

    <div class="two">
      <section class="card section" aria-labelledby="e-head">
        <h2 id="e-head">Export a table</h2>
        <p class="muted small">The whole table from one consistent snapshot, in id order. Secrets of system tables are masked.</p>
        <div class="row">
          <select aria-label="Table to export" bind:value={exportName}>
            <optgroup label="App tables">
              {#each appTables as t (t.name)}<option value={t.name}>{t.name} ({t.documents})</option>{/each}
            </optgroup>
            <optgroup label="System tables">
              {#each tables.filter((t) => t.system) as t (t.name)}<option value={t.name}>{t.name} ({t.documents})</option>{/each}
            </optgroup>
          </select>
          <select aria-label="Export format" bind:value={exportFormat}>
            <option value="jsonl">JSONL</option>
            <option value="csv">CSV</option>
          </select>
          <button onclick={runExport} disabled={exporting || !exportName}>{exporting ? "Exporting…" : "Export"}</button>
        </div>
      </section>

      <section class="card section" aria-labelledby="i-head">
        <h2 id="i-head">Import into a table</h2>
        <p class="muted small">
          JSONL, one document per line (as exported). Every line is validated against the table before anything is
          written; ids and creation times are kept.
        </p>
        <div class="form">
          <label for="import-table">Table</label>
          <select id="import-table" bind:value={importName}>
            {#each appTables as t (t.name)}<option value={t.name}>{t.name} ({t.documents})</option>{/each}
          </select>
          <label for="import-mode">Mode</label>
          <div>
            <select id="import-mode" bind:value={importMode}>
              <option value="insert">insert</option>
              <option value="upsert">upsert</option>
              <option value="replace">replace</option>
            </select>
            <span class="faint small" class:error-text={importMode === "replace"}>{modeHelp[importMode]}</span>
          </div>
          <label for="import-file">File</label>
          <input
            id="import-file"
            type="file"
            accept=".jsonl,.ndjson,application/x-ndjson,application/json,text/plain"
            onchange={(e) => (importFile = e.currentTarget.files?.[0] ?? null)}
          />
        </div>
        <div class="row">
          <button class="primary" onclick={runImport} disabled={importing || !importFile || !importName}>
            {importing ? "Importing…" : "Import"}
          </button>
          {#if importFile}<span class="faint small">{importFile.name} · {formatBytes(importFile.size)}</span>{/if}
        </div>
        {#if importResult?.ok}
          {@const r = importResult.report}
          <div class="result" role="status" data-testid="import-result">
            {#each r.tables as t (t.table)}
              <div>
                <strong>{t.documents}</strong> document{t.documents === 1 ? "" : "s"} imported into <a href={href("data", [t.table])} onclick={link}>{t.table}</a>
                {#if t.replaced}· {t.replaced} replaced{/if}
                {#if t.deleted}· {t.deleted} deleted first{/if}
              </div>
            {/each}
            {#if r.remappedIds}<div class="muted">Ids were rewritten for this database's table numbers.</div>{/if}
            {#if r.missingFiles}<div class="error-text">{r.missingFiles} referenced files are missing.</div>{/if}
          </div>
        {:else if importResult && !importResult.ok}
          <div class="alert" role="alert" data-testid="import-error">Nothing was imported.{"\n"}{importResult.error}</div>
        {/if}
      </section>
    </div>
  </div>
</div>

<style>
  .small {
    font-size: 12.5px;
  }
  .page-body {
    display: flex;
    flex-direction: column;
    gap: 16px;
  }
  .section {
    padding: 14px 16px;
    min-width: 0;
  }
  .head h2 {
    margin: 0;
  }
  .facts {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: 4px 14px;
    margin: 12px 0 6px;
    font-size: 13px;
    align-items: center;
  }
  .facts dt {
    color: var(--muted);
  }
  .facts dd {
    margin: 0;
    overflow-wrap: anywhere;
  }
  table.grid tbody tr {
    cursor: default;
  }
  .two {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(380px, 1fr));
    gap: 16px;
  }
  .form {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: 8px 12px;
    align-items: center;
    margin: 6px 0 10px;
  }
  .form label {
    color: var(--muted);
    font-size: 13px;
  }
  .form select {
    margin-right: 8px;
  }
  .result {
    margin-top: 10px;
    padding: 8px 12px;
    border-radius: var(--radius);
    background: var(--ok-soft);
    color: var(--text);
  }
  .alert {
    margin-top: 10px;
  }
</style>

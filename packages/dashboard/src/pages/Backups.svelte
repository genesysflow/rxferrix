<script lang="ts">
  import { FerrixError } from "@ferrix/client";
  import { useMutation, useQuery } from "@ferrix/svelte";
  import { onMount } from "svelte";
  import {
    scheduler,
    type BackupInfo,
    type ExportFormat,
    type ImportMode,
    type ImportReport,
    type PendingRestore,
    type RestartInfo,
  } from "../lib/api.js";
  import {
    cancelRestore,
    createBackup,
    deleteBackup,
    downloadBackup,
    exportTable,
    importFormatOf,
    importTable,
    listBackups,
    restartServer,
    saveBlob,
    stageRestore,
    uploadRestore,
    waitForRestart,
  } from "../lib/admin.js";
  import { dashboardContext } from "../lib/context.js";
  import { formatBytes, formatRelative, formatTime } from "../lib/format.js";
  import { href, link } from "../lib/router.svelte.js";
  import Empty from "../components/Empty.svelte";

  /** Backups (`/api/admin/backups*`), per-table export and JSONL/CSV import (`/api/admin/export|import/{table}`). */
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
  // Staged restores (docs/CONTRACT.md §4.4): validated now, applied when the server starts next.
  let pending = $state<PendingRestore | null>(null);
  let restartInfo = $state<RestartInfo>({ available: false, exitCode: null });
  let restoreError = $state<string | null>(null);
  let staging = $state<string | null>(null);
  let restarting = $state<"waiting" | "timeout" | null>(null);

  async function refresh() {
    try {
      const r = await listBackups(ctx.auth);
      target = r.target;
      backups = r.backups;
      pending = r.pendingRestore;
      restartInfo = r.restart;
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

  const restoreWarning =
    "It replaces ALL data (users, sessions, administrators and settings included) the next time the server starts; " +
    "the current database files are kept in pre-restore-<time>/ in the data directory. The backup is checked " +
    "completely first.";

  async function restore(label: string, stage: () => Promise<{ pendingRestore: PendingRestore | null; restart: RestartInfo }>) {
    restoreError = null;
    staging = label;
    try {
      const r = await stage();
      pending = r.pendingRestore;
      restartInfo = r.restart;
      ctx.toast(`${label} checked and staged; it is restored at the next start`);
    } catch (e) {
      restoreError = e instanceof Error ? e.message : String(e);
    } finally {
      staging = null;
    }
  }

  function restoreBackup(b: BackupInfo) {
    if (!confirm(`Restore ${b.name}?\n\n${restoreWarning}`)) return;
    void restore(b.name, () => stageRestore(ctx.auth, b.name));
  }

  function restoreUpload(e: Event & { currentTarget: HTMLInputElement }) {
    const input = e.currentTarget;
    const file = input.files?.[0];
    input.value = "";
    if (!file) return;
    if (!confirm(`Restore ${file.name} (${formatBytes(file.size)}) from this computer?\n\n${restoreWarning}`)) return;
    void restore(file.name, () => uploadRestore(ctx.auth, file, file.name));
  }

  async function cancelPending() {
    if (!pending || !confirm(`Cancel the restore of ${pending.source}? The current data stays.`)) return;
    try {
      await cancelRestore(ctx.auth);
      ctx.toast("Restore canceled");
    } catch (e) {
      ctx.toast(e instanceof Error ? e.message : String(e), "error");
    }
    await refresh();
  }

  async function restartNow() {
    if (!pending) return;
    const ok = confirm(
      `Restart the server now to restore ${pending.source}?\n\nOpen requests finish, then the server exits with code ` +
        `${restartInfo.exitCode} for its supervisor to start it again. Everyone is disconnected for a few seconds, and ` +
        "sessions that aren't in the backup end (you may have to sign in again).",
    );
    if (!ok) return;
    try {
      await restartServer(ctx.auth);
    } catch (e) {
      ctx.toast(e instanceof Error ? e.message : String(e), "error");
      return;
    }
    restarting = "waiting";
    if (await waitForRestart()) location.reload();
    else restarting = "timeout";
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
  let importFormat = $state<ExportFormat>("jsonl");
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
      const report = await importTable(ctx.auth, importName, importMode, importFile, importFormat);
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
        <code>auth_secret.key</code>: settings secrets restore only with the same key file. <strong>Restore</strong>
        checks a backup completely and stages it; it replaces the database when the server starts next (or use
        <code>ferrix restore &lt;archive&gt;</code> while the server is stopped).
      </p>
      {#if pending}
        <div class="pending" class:failed={pending.error} role="status" data-testid="pending-restore">
          <div>
            {#if pending.error}
              <strong>The staged restore of <span class="mono">{pending.source}</span> could not be applied.</strong>
              <div class="error-text" data-testid="pending-restore-error">{pending.error}</div>
              <div>The server started on its current data. Cancel it, then restore again.</div>
            {:else}
              <strong>Restore pending:</strong> <span class="mono">{pending.source}</span> (snapshot ts {pending.ts}, taken
              {formatTime(pending.backupCreatedAt)}{pending.includesAux ? ", with the execution log" : ""}{pending.blobs
                ? `, ${pending.blobs} file${pending.blobs === 1 ? "" : "s"}`
                : ""}, {formatBytes(pending.bytes)}) passed every check. It replaces the current data
              <strong>the next time the server starts</strong>; the current files are kept in
              <code>pre-restore-…/</code>.
              {#if pending.secretIgnored}<div class="faint">The archive's <code>auth_secret.key</code> is not restored (only <code>ferrix restore</code> restores it).</div>{/if}
              {#if restarting === "waiting"}
                <div data-testid="restarting">Restarting… this page reloads when the server is back.</div>
              {:else if restarting === "timeout"}
                <div class="error-text">The server hasn't come back yet. Check its supervisor (systemd, Docker).</div>
              {:else if !restartInfo.available}
                <div class="faint" data-testid="restart-hint">
                  Restart the server to apply it. (Started with <code>--restart-exit-code 75</code> under systemd or Docker, it
                  can restart itself from here.)
                </div>
              {/if}
            {/if}
          </div>
          <div class="pending-actions">
            {#if !pending.error && restartInfo.available && !restarting}
              <button class="primary" onclick={restartNow} data-testid="restart-now">Restart now</button>
            {/if}
            <button class="danger" onclick={cancelPending} disabled={restarting === "waiting"} data-testid="cancel-restore">Cancel restore</button>
          </div>
        </div>
      {/if}
      {#if staging}
        <div class="faint" role="status" data-testid="restore-checking">Checking {staging}… (extracting, integrity check, schema, indexes)</div>
      {/if}
      {#if restoreError}
        <div class="alert" role="alert" data-testid="restore-error">Not restored: {restoreError}</div>
      {/if}
      <div class="row upload">
        <label class="inline faint small" for="restore-file">Restore an archive from this computer:</label>
        <input id="restore-file" type="file" accept=".zst,.tar.zst,application/zstd" onchange={restoreUpload} disabled={!!pending || !!staging} />
      </div>
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
                  <button
                    class="small"
                    onclick={() => restoreBackup(b)}
                    disabled={busyName === b.name || !!pending || !!staging}
                    title={pending ? "A restore is pending" : "Check this backup and restore it at the next start"}>Restore</button>
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
          JSONL (one document per line) or CSV with a header row, as exported; CSV cells are typed with the table's
          fields. Every document is validated against the table before anything is written; ids and creation times
          are kept.
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
            accept=".jsonl,.ndjson,.csv,application/x-ndjson,application/json,text/csv,text/plain"
            onchange={(e) => {
              importFile = e.currentTarget.files?.[0] ?? null;
              if (importFile) importFormat = importFormatOf(importFile.name);
            }}
          />
          <label for="import-format">Format</label>
          <select id="import-format" bind:value={importFormat}>
            <option value="jsonl">JSONL</option>
            <option value="csv">CSV</option>
          </select>
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
  .pending {
    display: flex;
    gap: 12px;
    align-items: flex-start;
    justify-content: space-between;
    margin: 10px 0;
    padding: 10px 12px;
    border-radius: var(--radius);
    background: var(--warn-soft);
    border: 1px solid var(--border);
    font-size: 13px;
  }
  .pending.failed {
    background: var(--err-soft);
  }
  .pending-actions {
    display: flex;
    gap: 8px;
    flex-shrink: 0;
  }
  .upload {
    margin: 8px 0;
    gap: 8px;
  }
</style>

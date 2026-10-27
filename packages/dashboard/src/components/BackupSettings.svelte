<script lang="ts">
  import { untrack } from "svelte";
  import { RxFerrixError } from "@rxferrix/client";
  import { useAction } from "@rxferrix/svelte";
  import { settings, type SettingsView } from "../lib/api.js";
  import { formError, toForm, toInput, type BackupForm } from "../lib/backupSettings.js";
  import { dashboardContext } from "../lib/context.js";
  import Modal from "./Modal.svelte";

  /**
   * Scheduled backups and their destination (`_system/settings:update { backups }`): applied at once, no restart.
   * Parts fixed by the command line (`--backup-every`, `--backup-to`) are read-only.
   */
  let { view, onclose, onsaved }: { view: SettingsView; onclose: () => void; onsaved: (view: SettingsView) => void } =
    $props();
  const ctx = dashboardContext();
  const update = useAction(settings.update);
  const test = useAction(settings.testBackupDestination);

  // Seeded once from the view the dialog opened with; edits are local until saved.
  let form = $state<BackupForm>(untrack(() => toForm(view.backups, view.effective.backups)));
  const scheduleLocked = $derived(view.locked.includes("backups.schedule"));
  const destinationLocked = $derived(view.locked.includes("backups.destination"));
  const s3Available = $derived(view.effective.backups.s3Available);
  const problem = $derived(formError(form));
  let busy = $state<"save" | "test" | "reset" | null>(null);
  let tested = $state<{ ok: boolean; message: string } | null>(null);
  // A result is about the destination it tested: editing it clears the result.
  $effect(() => {
    void [form.kind, form.path, form.bucket, form.prefix, form.region, form.endpoint, form.accessKeyId, form.secret];
    tested = null;
  });

  function message(e: unknown): string {
    return e instanceof RxFerrixError ? e.message : String(e);
  }

  async function runTest() {
    busy = "test";
    tested = null;
    try {
      const at = await test({ backups: toInput(form) });
      tested = { ok: true, message: `Wrote, read back and deleted a test file in ${at}` };
    } catch (e) {
      tested = { ok: false, message: message(e) };
    } finally {
      busy = null;
    }
  }

  async function save() {
    busy = "save";
    try {
      onsaved(await update({ backups: toInput(form) }));
      ctx.toast("Backup settings saved");
      onclose();
    } catch (e) {
      ctx.toast(message(e), "error");
    } finally {
      busy = null;
    }
  }

  async function reset() {
    if (!confirm("Remove the backup settings? Scheduled backups stop, and backups go to the data directory again.")) return;
    busy = "reset";
    try {
      onsaved(await update({ clear: ["backups"] }));
      ctx.toast("Backup settings removed");
      onclose();
    } catch (e) {
      ctx.toast(message(e), "error");
    } finally {
      busy = null;
    }
  }
</script>

<Modal title="Backup settings" wide {onclose}>
  <div class="form" data-testid="backup-settings">
    <fieldset disabled={scheduleLocked}>
      <legend>Schedule</legend>
      {#if scheduleLocked}<p class="locked">🔒 Set on the command line (<code>--backup-every</code>); it wins over these settings.</p>{/if}
      <label class="inline"><input type="checkbox" bind:checked={form.scheduled} /> Back up automatically</label>
      {#if form.scheduled}
        <div class="row">
          <label class="inline">
            every
            <input class="num" type="number" min="1" aria-label="Interval" bind:value={form.every} />
            <select aria-label="Interval unit" bind:value={form.unit}>
              <option value="minutes">minutes</option>
              <option value="hours">hours</option>
              <option value="days">days</option>
            </select>
          </label>
          <label class="inline">
            keeping the last
            <input class="num" type="number" min="1" max="1000" aria-label="Backups to keep" bind:value={form.keep} />
          </label>
        </div>
        <div class="row">
          <label class="inline"><input type="checkbox" bind:checked={form.includeFiles} /> include stored files</label>
          <label class="inline"><input type="checkbox" bind:checked={form.includeAux} /> include the execution log and job history (aux.db)</label>
        </div>
        <p class="hint">Only scheduled backups are rotated; backups you create by hand are kept.</p>
      {/if}
    </fieldset>

    <fieldset disabled={destinationLocked}>
      <legend>Destination</legend>
      {#if destinationLocked}<p class="locked">🔒 Set on the command line (<code>--backup-to</code>); it wins over these settings.</p>{/if}
      <div class="kinds" role="radiogroup" aria-label="Destination">
        <label class="inline"><input type="radio" value="default" bind:group={form.kind} /> The data directory (<code>backups/</code>)</label>
        <label class="inline"><input type="radio" value="local" bind:group={form.kind} /> Another directory on the server</label>
        <label class="inline" title={s3Available ? "" : "This server was built without S3 support"}>
          <input type="radio" value="s3" bind:group={form.kind} disabled={!s3Available} /> An S3-compatible bucket (AWS, R2, MinIO, …)
        </label>
      </div>
      {#if form.kind === "local"}
        <label class="field"><span>Directory</span><input class="mono" placeholder="/var/backups/myapp" aria-label="Directory" bind:value={form.path} /></label>
      {:else if form.kind === "s3"}
        <div class="grid">
          <label class="field"><span>Bucket</span><input class="mono" placeholder="my-backups" aria-label="Bucket" bind:value={form.bucket} /></label>
          <label class="field"><span>Prefix</span><input class="mono" placeholder="prod/db" aria-label="Prefix" bind:value={form.prefix} /></label>
          <label class="field"><span>Region</span><input class="mono" placeholder="us-east-1" aria-label="Region" bind:value={form.region} /></label>
          <label class="field"><span>Endpoint</span><input class="mono" placeholder="(AWS) or https://<account>.r2.cloudflarestorage.com" aria-label="Endpoint" bind:value={form.endpoint} /></label>
          <label class="field"><span>Access key id</span><input class="mono" autocomplete="off" aria-label="Access key id" bind:value={form.accessKeyId} /></label>
          <label class="field">
            <span>Secret access key</span>
            <input
              class="mono"
              type="password"
              autocomplete="new-password"
              aria-label="Secret access key"
              placeholder={form.secretStored ? "•••• stored (type to replace)" : ""}
              disabled={form.accessKeyId.trim() === ""}
              bind:value={form.secret}
            />
          </label>
        </div>
        <p class="hint">Leave the access key empty to use the server's <code>AWS_*</code> environment. The secret is encrypted at rest and never shown again.</p>
      {/if}
      <div class="row">
        <button onclick={runTest} disabled={busy !== null || problem !== null}>{busy === "test" ? "Testing…" : "Test destination"}</button>
        {#if tested}<span class={tested.ok ? "ok" : "error-text"} role="status" data-testid="backup-test-result">{tested.ok ? "✓ " : "✗ "}{tested.message}</span>{/if}
      </div>
    </fieldset>
    {#if problem}<p class="error-text">{problem}</p>{/if}
  </div>
  {#snippet actions()}
    {#if view.backups}<button class="danger" onclick={reset} disabled={busy !== null}>Remove settings</button>{/if}
    <span class="spacer"></span>
    <button onclick={onclose}>Cancel</button>
    <button class="primary" onclick={save} disabled={busy !== null || problem !== null || (scheduleLocked && destinationLocked)}>
      {busy === "save" ? "Saving…" : "Save"}
    </button>
  {/snippet}
</Modal>

<style>
  .form {
    display: flex;
    flex-direction: column;
    gap: 14px;
  }
  fieldset {
    border: 1px solid var(--border);
    border-radius: var(--radius);
    padding: 10px 14px 12px;
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  fieldset:disabled {
    opacity: 0.75;
  }
  legend {
    font-weight: 600;
    padding: 0 4px;
  }
  .kinds {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .grid {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 8px 12px;
  }
  .field {
    display: flex;
    flex-direction: column;
    gap: 3px;
    font-size: 12.5px;
    color: var(--muted);
  }
  .field input {
    color: var(--text);
  }
  input.num {
    width: 80px;
  }
  .hint,
  .locked {
    margin: 0;
    font-size: 12.5px;
    color: var(--muted);
  }
  .ok {
    color: var(--ok);
    font-size: 13px;
  }
  .spacer {
    flex: 1;
  }
  @media (max-width: 760px) {
    .grid {
      grid-template-columns: 1fr;
    }
  }
</style>

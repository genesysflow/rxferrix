<script lang="ts">
  import { untrack } from "svelte";
  import { RxFerrixError } from "@rxferrix/client";
  import { useMutation, useQuery } from "@rxferrix/svelte";
  import { ENV_NAME, env, type EnvVarView } from "../lib/api.js";
  import { dashboardContext } from "../lib/context.js";
  import { parseDotenv } from "../lib/dotenv.js";
  import { flash } from "../lib/flash.js";
  import { formatRelative, formatTime } from "../lib/format.js";
  import Empty from "../components/Empty.svelte";
  import Modal from "../components/Modal.svelte";

  /**
   * Environment variables (`_system/env:*`): add, edit, reveal, copy, remove, and paste a `.env`. The list is live, and
   * every change reaches the functions that read the variable at once (queries rerun).
   */
  const ctx = dashboardContext();
  const list = useQuery(env.list, () => ({}));
  const setVar = useMutation(env.set);
  const removeVar = useMutation(env.remove);

  const vars = $derived(list.data ?? []);
  let filter = $state("");
  const shown = $derived(vars.filter((v) => v.name.toLowerCase().includes(filter.trim().toLowerCase())));

  // Adding.
  let newName = $state("");
  let newValue = $state("");
  const nameError = $derived(
    newName === "" ? null : ENV_NAME.test(newName) ? null : "Letters, digits and _, not starting with a digit (max 64)",
  );
  const exists = $derived(vars.some((v) => v.name === newName));

  // Editing and revealing, by name.
  let editing = $state<string | null>(null);
  let draft = $state("");
  let revealed = $state<Set<string>>(new Set());
  let revealAll = $state(false);
  let busy = $state<string | null>(null);
  let pasting = $state(false);
  let pasted = $state("");
  const parsed = $derived(parseDotenv(pasted));

  // Changes made anywhere (another admin, the command line) flash, like rows in the data grid.
  let seen: Map<string, number> | null = null;
  let recent = $state.raw(new Map<string, { kind: "inserted" | "changed"; at: number }>());
  $effect(() => {
    const data = list.data;
    if (!data) return;
    const now = performance.now();
    const next = new Map(data.map((v) => [v.name, v.updatedAt] as const));
    if (seen) {
      const changes = new Map(untrack(() => recent));
      for (const [name, at] of next) {
        if (!seen.has(name)) changes.set(name, { kind: "inserted", at: now });
        else if (seen.get(name) !== at) changes.set(name, { kind: "changed", at: now });
      }
      recent = changes;
    }
    seen = next;
  });

  function errorText(e: unknown): string {
    return e instanceof RxFerrixError ? e.message : String(e);
  }

  async function add(e: SubmitEvent) {
    e.preventDefault();
    if (!newName || nameError) return;
    const replacing = exists;
    if (replacing && !confirm(`${newName} is already set. Replace its value?`)) return;
    busy = newName;
    try {
      await setVar({ name: newName, value: newValue });
      ctx.toast(`${replacing ? "Updated" : "Added"} ${newName}`);
      newName = "";
      newValue = "";
    } catch (err) {
      ctx.toast(errorText(err), "error");
    } finally {
      busy = null;
    }
  }

  function edit(v: EnvVarView) {
    editing = v.name;
    draft = v.value;
  }

  async function save(name: string) {
    busy = name;
    try {
      await setVar({ name, value: draft });
      ctx.toast(`Updated ${name}`);
      editing = null;
    } catch (err) {
      ctx.toast(errorText(err), "error");
    } finally {
      busy = null;
    }
  }

  async function remove(name: string) {
    if (!confirm(`Remove ${name}? Functions that read it get nothing from then on.`)) return;
    busy = name;
    try {
      await removeVar({ name });
      ctx.toast(`Removed ${name}`);
    } catch (err) {
      ctx.toast(errorText(err), "error");
    } finally {
      busy = null;
    }
  }

  async function importPasted() {
    busy = "*";
    let done = 0;
    try {
      for (const [name, value] of parsed.entries) {
        await setVar({ name, value });
        done++;
      }
      ctx.toast(`Set ${done} variable${done === 1 ? "" : "s"}`);
      pasting = false;
      pasted = "";
    } catch (err) {
      ctx.toast(`Set ${done}, then: ${errorText(err)}`, "error");
    } finally {
      busy = null;
    }
  }

  function toggleReveal(name: string) {
    const next = new Set(revealed);
    if (next.has(name)) next.delete(name);
    else next.add(name);
    revealed = next;
  }

  async function copy(v: EnvVarView) {
    try {
      await navigator.clipboard.writeText(v.value);
      ctx.toast(`Copied the value of ${v.name}`);
    } catch {
      ctx.toast("The browser refused to copy", "error");
    }
  }

  function onEditKey(e: KeyboardEvent, name: string) {
    if (e.key === "Escape") editing = null;
    if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) void save(name);
  }
</script>

<div class="page">
  <header class="page-header">
    <h1>Environment</h1>
    <span class="muted small">
      Read with <code>ctx.env.get("NAME")</code>; queries that read a variable rerun as soon as it changes. Encrypted at
      rest.
    </span>
    <span class="spacer"></span>
    <button onclick={() => (pasting = true)}>Paste .env…</button>
  </header>

  <div class="page-body">
    <form class="card add" onsubmit={add} data-testid="env-add">
      <div class="add-fields">
        <label>
          <span>Name</span>
          <input
            class="mono"
            placeholder="OPENAI_API_KEY"
            aria-label="Name"
            aria-invalid={nameError ? "true" : undefined}
            autocomplete="off"
            spellcheck="false"
            bind:value={newName}
          />
        </label>
        <label class="grow">
          <span>Value</span>
          <input class="mono" aria-label="Value" autocomplete="off" spellcheck="false" bind:value={newValue} />
        </label>
        <button class="primary" type="submit" disabled={!newName || !!nameError || busy !== null}>
          {exists ? "Replace" : "Add"}
        </button>
      </div>
      {#if nameError}<div class="field-error">{nameError}</div>{/if}
    </form>

    {#if list.data === undefined}
      <Empty title="Loading…" />
    {:else if vars.length === 0}
      <Empty
        title="No variables yet"
        hint="Add one above (or paste a .env). From the command line, before the server starts: env set NAME."
      />
    {:else}
      <div class="tools row">
        <input class="filter" placeholder="Filter by name" aria-label="Filter variables" bind:value={filter} />
        <label class="inline"><input type="checkbox" bind:checked={revealAll} /> show values</label>
        <span class="spacer"></span>
        <span class="faint small">{vars.length} variable{vars.length === 1 ? "" : "s"}</span>
      </div>
      <div class="card table-wrap">
        <table class="grid" data-testid="env-vars">
          <thead><tr><th>Name</th><th>Value</th><th>Updated</th><th></th></tr></thead>
          <tbody>
            {#each shown as v (v.name)}
              {@const change = recent.get(v.name)}
              {@const show = revealAll || revealed.has(v.name)}
              <tr data-env={v.name} use:flash={change?.kind === "inserted" ? change : undefined}>
                <td class="mono name">{v.name}</td>
                <td class="value" use:flash={change?.kind === "changed" ? change : undefined}>
                  {#if editing === v.name}
                    <div class="edit">
                      <textarea
                        class="mono"
                        aria-label={`Value of ${v.name}`}
                        rows={Math.min(6, Math.max(1, draft.split("\n").length))}
                        bind:value={draft}
                        onkeydown={(e) => onEditKey(e, v.name)}
                      ></textarea>
                      <button class="primary small" onclick={() => save(v.name)} disabled={busy !== null}>Save</button>
                      <button class="small" onclick={() => (editing = null)}>Cancel</button>
                    </div>
                  {:else}
                    <button class="ghost reveal" onclick={() => toggleReveal(v.name)} title={show ? "Hide" : "Show"} aria-label={`${show ? "Hide" : "Show"} ${v.name}`}>
                      {show ? "◉" : "○"}
                    </button>
                    {#if show}
                      <span class="mono shown" data-testid="env-value">{v.value === "" ? "(empty)" : v.value}</span>
                    {:else}
                      <span class="mono masked">{"•".repeat(Math.min(24, Math.max(8, v.value.length)))}</span>
                    {/if}
                  {/if}
                </td>
                <td class="faint" title={formatTime(v.updatedAt)}>{formatRelative(v.updatedAt)}{v.updatedBy ? "" : " · CLI"}</td>
                <td class="actions">
                  {#if editing !== v.name}
                    <button class="small" onclick={() => copy(v)} aria-label={`Copy ${v.name}`}>Copy</button>
                    <button class="small" onclick={() => edit(v)} aria-label={`Edit ${v.name}`}>Edit</button>
                    <button class="small danger" onclick={() => remove(v.name)} disabled={busy !== null} aria-label={`Remove ${v.name}`}>Remove</button>
                  {/if}
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    {/if}
  </div>
</div>

{#if pasting}
  <Modal title="Paste a .env" wide onclose={() => (pasting = false)}>
    <p class="muted small">
      <code>NAME=value</code> per line; quotes, <code>export</code> and <code>#</code> comments are understood. Existing
      variables are overwritten, others are left alone.
    </p>
    <textarea class="mono paste" rows="10" aria-label=".env contents" placeholder={"API_URL=https://api.example.com\nAPI_KEY=…"} bind:value={pasted}></textarea>
    {#if parsed.errors.length > 0}
      <ul class="errors">{#each parsed.errors as e (e)}<li>{e}</li>{/each}</ul>
    {/if}
    {#if parsed.entries.length > 0}
      <p class="small">
        Will set: {#each parsed.entries as [name], i (name)}<span class="mono">{name}</span>{#if vars.some((v) => v.name === name)}<span class="faint"> (replace)</span>{/if}{i < parsed.entries.length - 1 ? ", " : ""}{/each}
      </p>
    {/if}
    {#snippet actions()}
      <button onclick={() => (pasting = false)}>Cancel</button>
      <button class="primary" onclick={importPasted} disabled={parsed.entries.length === 0 || busy !== null}>
        Set {parsed.entries.length} variable{parsed.entries.length === 1 ? "" : "s"}
      </button>
    {/snippet}
  </Modal>
{/if}

<style>
  .small {
    font-size: 12.5px;
  }
  .page-body {
    display: flex;
    flex-direction: column;
    gap: 12px;
    max-width: 1000px;
  }
  .add {
    padding: 12px 14px;
  }
  .add-fields {
    display: flex;
    align-items: flex-end;
    gap: 10px;
    flex-wrap: wrap;
  }
  .add-fields label {
    display: flex;
    flex-direction: column;
    gap: 4px;
    font-size: 12.5px;
    color: var(--muted);
  }
  .add-fields label.grow {
    flex: 1;
    min-width: 200px;
  }
  .add-fields input[aria-label="Name"] {
    width: 240px;
  }
  [aria-invalid="true"] {
    border-color: var(--err);
  }
  .field-error {
    color: var(--err);
    font-size: 12.5px;
    margin-top: 6px;
  }
  .filter {
    width: 240px;
  }
  .table-wrap {
    padding: 0;
    overflow: auto;
  }
  table.grid td {
    vertical-align: middle;
  }
  .name {
    font-weight: 600;
  }
  .value {
    max-width: 460px;
  }
  .value > * {
    vertical-align: middle;
  }
  .reveal {
    padding: 0 6px;
    color: var(--muted);
  }
  .masked {
    color: var(--faint);
    letter-spacing: 1px;
  }
  .shown {
    white-space: pre-wrap;
    word-break: break-all;
  }
  .edit {
    display: flex;
    gap: 6px;
    align-items: flex-start;
  }
  .edit textarea {
    flex: 1;
    min-width: 200px;
    resize: vertical;
  }
  .actions {
    white-space: nowrap;
    text-align: right;
  }
  .actions button + button {
    margin-left: 4px;
  }
  .paste {
    width: 100%;
    resize: vertical;
  }
  .errors {
    color: var(--err);
    font-size: 12.5px;
    margin: 6px 0;
    padding-left: 18px;
  }
  @media (max-width: 760px) {
    .add-fields input[aria-label="Name"],
    .filter {
      width: 100%;
    }
    .add-fields label {
      width: 100%;
    }
    .value {
      max-width: 220px;
    }
  }
</style>

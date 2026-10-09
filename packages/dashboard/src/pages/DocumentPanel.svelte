<script lang="ts">
  import { FerrixError } from "@ferrix/client";
  import { untrack } from "svelte";
  import { useMutation, useQuery } from "@ferrix/svelte";
  import { dashboard, type Json, type JsonObject, type TableSummary } from "../lib/api.js";
  import { dashboardContext } from "../lib/context.js";
  import { parseJson, pretty } from "../lib/format.js";
  import { href, router } from "../lib/router.svelte.js";
  import JsonEditor from "../components/JsonEditor.svelte";
  import JsonValue from "../components/JsonValue.svelte";

  /** The side panel: one document (live) with a JSON editor, or a new document when `id` is null. */
  let {
    table,
    id,
    template,
    onclose,
  }: { table: TableSummary; id: string | null; template: JsonObject | null; onclose: () => void } = $props();
  const ctx = dashboardContext();
  const doc = useQuery(dashboard.document, () => (id ? { id } : "skip"));
  const replaceDocument = useMutation(dashboard.replaceDocument);
  const insertDocument = useMutation(dashboard.insertDocument);
  const deleteDocuments = useMutation(dashboard.deleteDocuments);

  const editable = $derived(table.access !== "readOnly");
  const current = $derived(doc.data?.document);
  // The panel is keyed by id (see Data.svelte), so the initial props are all it needs.
  let text = $state(untrack(() => (id ? "" : pretty(template ?? {}))));
  /** The server text the editor started from; when the document changes underneath, `text` follows unless edited. */
  let base = $state<string | null>(null);
  let busy = $state(false);
  let error = $state<string | null>(null);
  const dirty = $derived(base !== null && text !== base);
  const changedOnServer = $derived(current !== undefined && base !== null && pretty(current) !== base);

  $effect(() => {
    if (current === undefined) return;
    const fresh = pretty(current);
    if (base === null || (!dirty && fresh !== base)) {
      base = fresh;
      text = fresh;
    }
  });

  function reload() {
    if (current === undefined) return;
    base = pretty(current);
    text = base;
    error = null;
  }

  async function save() {
    const parsed = parseJson(text);
    if (!parsed.ok || typeof parsed.value !== "object" || parsed.value === null || Array.isArray(parsed.value)) {
      error = parsed.ok ? "A document must be a JSON object" : `Invalid JSON: ${parsed.error}`;
      return;
    }
    const value = parsed.value as JsonObject;
    busy = true;
    error = null;
    try {
      if (id) {
        await replaceDocument({ id, document: value });
        base = null; // take the server's version (which now matches)
        ctx.toast("Saved");
      } else {
        const newId = await insertDocument({ table: table.name, document: value });
        ctx.toast("Inserted");
        router.navigate(href("data", [table.name], { doc: newId }));
      }
    } catch (e) {
      error = e instanceof FerrixError ? e.message : String(e);
    } finally {
      busy = false;
    }
  }

  async function remove() {
    if (!id || !confirm(`Delete document ${id}?`)) return;
    try {
      await deleteDocuments({ ids: [id] });
      ctx.toast("Deleted");
      onclose();
    } catch (e) {
      error = e instanceof FerrixError ? e.message : String(e);
    }
  }

  function onkeydown(e: KeyboardEvent) {
    if (e.key === "Escape" && !(e.target instanceof HTMLTextAreaElement)) onclose();
  }

  const fields = $derived(current ? (Object.entries(current) as [string, Json][]) : []);
</script>

<svelte:window {onkeydown} />

<aside class="panel" aria-label={id ? "Document" : "New document"}>
  <header>
    <div class="title">
      <strong>{id ? "Document" : `New ${table.name} document`}</strong>
      {#if id}<span class="mono faint">{id}</span>{/if}
    </div>
    <button class="ghost" onclick={onclose} title="Close (Esc)" aria-label="Close">✕</button>
  </header>

  <div class="content">
    {#if id && doc.isLoading}
      <p class="muted">Loading…</p>
    {:else if id && doc.data === null}
      <div class="alert info" role="status">This document doesn't exist (it may have been deleted).</div>
    {:else}
      {#if current}
        <dl class="fields">
          {#each fields as [k, v] (k)}
            <dt class="mono">{k}</dt>
            <dd>
              <JsonValue value={v} field={k} max={200} />
              {#if k === "_creationTime" && typeof v === "number"}<span class="faint mono"> · {v}</span>{/if}
            </dd>
          {/each}
        </dl>
      {/if}

      {#if editable}
        <h3>{id ? "Edit JSON" : "JSON"}</h3>
        {#if table.access === "limited"}
          <p class="faint note">Only <code>name</code>, <code>image</code> and <code>emailVerified</code> can be changed.</p>
        {/if}
        {#if changedOnServer && dirty}
          <div class="alert info row">
            Changed on the server since you started editing.
            <button class="small" onclick={reload}>Load latest</button>
          </div>
        {/if}
        <JsonEditor label="Document JSON" bind:value={text} rows={16} onsubmit={save} />
        {#if error}<div class="alert" role="alert">{error}</div>{/if}
        <div class="row actions">
          <button class="primary" onclick={save} disabled={busy || (id !== null && !dirty)}>
            {id ? "Save" : "Insert"} <kbd>Ctrl ↵</kbd>
          </button>
          {#if id && dirty}<button onclick={reload}>Discard</button>{/if}
          <span class="spacer"></span>
          {#if id && table.access === "readWrite"}<button class="danger" onclick={remove}>Delete</button>{/if}
        </div>
      {:else if current}
        <h3>JSON</h3>
        <pre class="json">{pretty(current)}</pre>
        <p class="faint note">System table: read-only here.</p>
      {/if}
    {/if}
  </div>
</aside>

<style>
  .panel {
    width: min(520px, 45vw);
    flex: none;
    border-left: 1px solid var(--border);
    background: var(--panel);
    display: flex;
    flex-direction: column;
    box-shadow: var(--shadow);
  }
  header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 12px 16px;
    border-bottom: 1px solid var(--border);
  }
  .title {
    display: flex;
    flex-direction: column;
    min-width: 0;
  }
  .title .mono {
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .content {
    flex: 1;
    overflow: auto;
    padding: 12px 16px 20px;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .fields {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: 4px 12px;
    margin: 0;
  }
  dt {
    color: var(--muted);
  }
  dd {
    margin: 0;
    min-width: 0;
    overflow-wrap: anywhere;
  }
  .note {
    margin: 0;
    font-size: 12.5px;
  }
  .json {
    background: var(--panel-2);
    padding: 10px;
    border-radius: var(--radius);
  }
  .actions {
    margin-top: 4px;
  }
</style>

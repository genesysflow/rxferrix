<script lang="ts">
  import { FerrixError } from "@ferrix/client";
  import { useMutation, usePaginatedQuery } from "@ferrix/svelte";
  import { dashboard, type FilterOp, type IndexFilter, type JsonObject, type TableSummary } from "../lib/api.js";
  import { dashboardContext } from "../lib/context.js";
  import { columnsOf, parseFilterValue } from "../lib/format.js";
  import { href, router } from "../lib/router.svelte.js";
  import Empty from "../components/Empty.svelte";
  import JsonValue from "../components/JsonValue.svelte";

  let {
    table,
    selectedId,
    oninsert,
  }: { table: TableSummary; selectedId: string | null; oninsert: (template: JsonObject) => void } = $props();
  const ctx = dashboardContext();
  const deleteDocuments = useMutation(dashboard.deleteDocuments);

  const usableIndexes = $derived(table.indexes.filter((i) => i.usable));
  // Inputs (edited freely) vs. the applied query (changed by Apply / Enter, index and order switches).
  let indexName = $state("by_creation_time");
  let order = $state<"asc" | "desc">("desc");
  let inputs = $state<Record<string, { op: FilterOp; value: string }>>({});
  let applied = $state<IndexFilter[]>([]);
  let showSchema = $state(false);
  let selected = $state<Set<string>>(new Set());

  const index = $derived(usableIndexes.find((i) => i.name === indexName) ?? usableIndexes[0]);

  const docs = usePaginatedQuery(
    dashboard.documents,
    () => ({ table: table.name, index: index?.name ?? null, filters: applied, order }),
    { initialNumItems: 50 },
  );
  const columns = $derived(columnsOf(docs.results));
  const writable = $derived(table.access === "readWrite");

  function apply() {
    // Index rules: eq on a prefix of the fields, then at most one range bound. Fields are used in order up to the
    // first empty one.
    const filters: IndexFilter[] = [];
    for (const field of index?.fields ?? []) {
      const input = inputs[field];
      if (!input || input.value.trim() === "") break;
      filters.push({ field, op: input.op, value: parseFilterValue(input.value) });
      if (input.op !== "eq") break;
    }
    applied = filters;
  }

  function setIndex(name: string) {
    indexName = name;
    inputs = {};
    applied = [];
  }

  function clearFilters() {
    inputs = {};
    applied = [];
  }

  function open(id: string) {
    router.navigate(href("data", [table.name], { doc: id }));
  }

  function toggle(id: string, on: boolean) {
    const next = new Set(selected);
    if (on) next.add(id);
    else next.delete(id);
    selected = next;
  }

  async function deleteSelected() {
    const ids = [...selected];
    if (!confirm(`Delete ${ids.length} document${ids.length === 1 ? "" : "s"} from ${table.name}?`)) return;
    try {
      const n = await deleteDocuments({ ids });
      selected = new Set();
      ctx.toast(`Deleted ${n} document${n === 1 ? "" : "s"}`);
    } catch (e) {
      ctx.toast(e instanceof FerrixError ? e.message : String(e), "error");
    }
  }

  /** A starting point for a new document: the user fields of the first document, or `{}`. */
  function template(): JsonObject {
    const first = docs.results[0];
    if (!first) return {};
    const { _id: _a, _creationTime: _b, ...rest } = first;
    return rest;
  }

  function onkeydown(e: KeyboardEvent) {
    if (e.key === "Enter") {
      e.preventDefault();
      apply();
    }
  }
</script>

<div class="page">
  <header class="page-header">
    <h1>{table.name}</h1>
    <span class="muted">{table.documents} document{table.documents === 1 ? "" : "s"}</span>
    {#if table.access === "readOnly"}<span class="badge" title="System tables can't be edited here">read-only</span>{/if}
    {#if table.access === "limited"}<span class="badge" title="Only name, image and emailVerified are editable">limited edits</span>{/if}
    {#if !table.defined}<span class="badge" title="No longer defined in code">not in code</span>{/if}
    <span class="spacer"></span>
    <button class="ghost" onclick={() => (showSchema = !showSchema)} aria-expanded={showSchema}>Schema</button>
    {#if selected.size > 0 && writable}
      <button class="danger" onclick={deleteSelected}>Delete {selected.size}</button>
    {/if}
    {#if writable}
      <button class="primary" onclick={() => oninsert(template())}>+ Insert</button>
    {/if}
  </header>

  {#if showSchema}
    <div class="schema">
      <div>
        <h3>Type</h3>
        <pre>{table.ts ?? "(not defined in code)"}</pre>
      </div>
      <div>
        <h3>Indexes</h3>
        <ul>
          {#each table.indexes as i (i.name)}
            <li><code>{i.name}</code> <span class="muted">[{i.fields.join(", ")}]</span>{#if !i.usable}<span class="faint"> · not browsable</span>{/if}</li>
          {/each}
        </ul>
        {#if table.searchIndexes.length > 0}
          <h3>Search and vector indexes</h3>
          <ul>
            {#each table.searchIndexes as s (s.name)}
              <li>
                <code>{s.name}</code>
                <span class="muted"
                  >{s.kind === "vector" ? `vector ${s.field} (${s.dimensions} dims)` : `text ${s.field}`}{#if s.filterFields.length > 0}
                    · filter [{s.filterFields.join(", ")}]{/if}</span
                >
              </li>
            {/each}
          </ul>
        {/if}
        {#if table.secretFields.length > 0}
          <p class="faint">Masked: {table.secretFields.join(", ")}</p>
        {/if}
      </div>
    </div>
  {/if}

  <div class="filters row">
    <label class="inline"
      >Index
      <select value={index?.name} onchange={(e) => setIndex(e.currentTarget.value)} aria-label="Index">
        {#each usableIndexes as i (i.name)}<option value={i.name}>{i.name}</option>{/each}
      </select>
    </label>
    {#each index?.fields ?? [] as field (field)}
      {@const input = inputs[field] ?? { op: "eq" as FilterOp, value: "" }}
      <span class="field">
        <span class="mono fname">{field}</span>
        <select
          aria-label={`${field} operator`}
          value={input.op}
          onchange={(e) => (inputs = { ...inputs, [field]: { ...input, op: e.currentTarget.value as FilterOp } })}
        >
          <option value="eq">=</option><option value="gt">&gt;</option><option value="gte">≥</option>
          <option value="lt">&lt;</option><option value="lte">≤</option>
        </select>
        <input
          class="mono"
          aria-label={`Filter ${field}`}
          placeholder="any"
          value={input.value}
          oninput={(e) => (inputs = { ...inputs, [field]: { ...input, value: e.currentTarget.value } })}
          {onkeydown}
        />
      </span>
    {/each}
    <button class="small" onclick={apply}>Apply</button>
    {#if applied.length > 0}<button class="small ghost" onclick={clearFilters}>Clear</button>{/if}
    <span class="spacer"></span>
    <button class="small" onclick={() => (order = order === "asc" ? "desc" : "asc")} title="Order along the index">
      {order === "desc" ? "↓ newest first" : "↑ oldest first"}
    </button>
  </div>

  <div class="body">
    {#if docs.error}
      <div class="alert" role="alert">{docs.error.message}</div>
    {:else if docs.isLoading && docs.results.length === 0}
      <Empty title="Loading…" />
    {:else if docs.results.length === 0}
      <Empty
        title={applied.length > 0 ? "No documents match these filters" : "No documents yet"}
        hint={applied.length > 0 ? "Change or clear the filters." : writable ? "Insert one here, or write from your app." : ""}
      >
        {#if writable && applied.length === 0}<button onclick={() => oninsert({})}>+ Insert document</button>{/if}
      </Empty>
    {:else}
      <table class="grid" data-testid="document-grid">
        <thead>
          <tr>
            {#if writable}<th class="check"></th>{/if}
            {#each columns as c (c)}<th>{c}</th>{/each}
          </tr>
        </thead>
        <tbody>
          {#each docs.results as doc (doc._id)}
            {@const id = doc._id as string}
            <tr class:selected={id === selectedId} onclick={() => open(id)}>
              {#if writable}
                <td class="check" onclick={(e) => e.stopPropagation()}>
                  <input
                    type="checkbox"
                    aria-label={`Select ${id}`}
                    checked={selected.has(id)}
                    onchange={(e) => toggle(id, e.currentTarget.checked)}
                  />
                </td>
              {/if}
              {#each columns as c (c)}
                <td><JsonValue value={doc[c]} field={c} /></td>
              {/each}
            </tr>
          {/each}
        </tbody>
      </table>
      <div class="more">
        {#if docs.status === "CanLoadMore"}
          <button onclick={() => docs.loadMore(50)}>Load more</button>
        {:else if docs.status === "LoadingMore"}
          <span class="faint">Loading…</span>
        {:else}
          <span class="faint">{docs.results.length} shown · end of results</span>
        {/if}
      </div>
    {/if}
  </div>
</div>

<style>
  .filters {
    padding: 8px 20px;
    border-bottom: 1px solid var(--border);
    background: var(--panel);
  }
  .field {
    display: inline-flex;
    align-items: center;
    gap: 4px;
  }
  .fname {
    color: var(--muted);
  }
  .field input {
    width: 150px;
  }
  .body {
    flex: 1;
    overflow: auto;
  }
  .check {
    width: 28px;
    padding-right: 0 !important;
  }
  .more {
    padding: 12px 20px;
  }
  .schema {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 16px;
    padding: 4px 20px 12px;
    border-bottom: 1px solid var(--border);
    background: var(--panel);
  }
  .schema ul {
    margin: 0;
    padding-left: 18px;
  }
  .schema pre {
    background: var(--panel-2);
    padding: 8px;
    border-radius: var(--radius);
  }
</style>

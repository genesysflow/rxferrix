<script lang="ts">
  import { untrack } from "svelte";
  import { FerrixError } from "@ferrix/client";
  import { useMutation, usePaginatedQuery } from "@ferrix/svelte";
  import {
    dashboard,
    type ExportFormat,
    type FilterOp,
    type IndexFilter,
    type Json,
    type JsonObject,
    type TableSummary,
  } from "../lib/api.js";
  import { exportTable, saveBlob } from "../lib/admin.js";
  import { dashboardContext } from "../lib/context.js";
  import { diffSnapshots, snapshot, withGhosts, type Removed, type Snapshot } from "../lib/changes.js";
  import { columnsOf, documentMatches, parseFilterValue } from "../lib/format.js";
  import { href, link, router } from "../lib/router.svelte.js";
  import { columnWidths, nearEnd, rowWindow } from "../lib/virtual.js";
  import Empty from "../components/Empty.svelte";
  import JsonValue from "../components/JsonValue.svelte";

  /**
   * The documents of one table: a virtualized, live grid (only the rows in view are rendered; further pages load as
   * you scroll), browsed along an index with filters, or searched through a search index; a quick filter over the
   * loaded rows; export.
   */
  let {
    table,
    selectedId,
    oninsert,
    migrating = false,
  }: { table: TableSummary; selectedId: string | null; oninsert: (template: JsonObject) => void; migrating?: boolean } =
    $props();
  const ctx = dashboardContext();
  const deleteDocuments = useMutation(dashboard.deleteDocuments);

  const ROW = 32;
  const HEADER = 32;
  const PAGE = 200;

  const usableIndexes = $derived(table.indexes.filter((i) => i.usable));
  const searchIndexes = $derived(table.searchIndexes.filter((i) => i.kind === "search"));
  let mode = $state<"browse" | "search">("browse");

  // Browse: inputs (edited freely) vs. the applied query (changed by Apply / Enter, index and order switches).
  let indexName = $state("by_creation_time");
  let order = $state<"asc" | "desc">("desc");
  let inputs = $state<Record<string, { op: FilterOp; value: string }>>({});
  let applied = $state<IndexFilter[]>([]);
  const index = $derived(usableIndexes.find((i) => i.name === indexName) ?? usableIndexes[0]);

  // Search: the text is debounced; filter fields are equality filters.
  let searchIndexName = $state("");
  let searchInput = $state("");
  let searchText = $state("");
  let searchFilters = $state<Record<string, string>>({});
  const searchIndex = $derived(searchIndexes.find((i) => i.name === searchIndexName) ?? searchIndexes[0]);
  let searchTimer: ReturnType<typeof setTimeout> | undefined;
  function onSearchInput(value: string) {
    searchInput = value;
    clearTimeout(searchTimer);
    searchTimer = setTimeout(() => (searchText = value.trim()), 200);
  }

  let quick = $state("");
  let showSchema = $state(false);
  let selected = $state<Set<string>>(new Set());
  let exporting = $state<ExportFormat | null>(null);

  const browse = usePaginatedQuery(
    dashboard.documents,
    () => (mode === "browse" ? { table: table.name, index: index?.name ?? null, filters: applied, order } : "skip"),
    { initialNumItems: PAGE },
  );
  const search = usePaginatedQuery(
    dashboard.searchDocuments,
    () => {
      if (mode !== "search" || !searchIndex || searchText === "") return "skip";
      const filters: { field: string; value: Json }[] = [];
      for (const field of searchIndex.filterFields) {
        const v = searchFilters[field]?.trim();
        if (v) filters.push({ field, value: parseFilterValue(v) });
      }
      return { table: table.name, index: searchIndex.name, text: searchText, filters };
    },
    { initialNumItems: PAGE },
  );
  const docs = $derived(mode === "search" ? search : browse);
  const searching = $derived(mode === "search" && searchText !== "");

  const needle = $derived(quick.trim().toLowerCase());
  const rows = $derived(needle ? docs.results.filter((d) => documentMatches(d, needle)) : docs.results);
  const columns = $derived(columnsOf(docs.results));
  const writable = $derived(table.access === "readWrite");
  const widths = $derived(columnWidths(columns, docs.results.slice(0, 300)));
  const totalWidth = $derived(widths.reduce((a, b) => a + b, 0) + (writable ? 32 : 0));

  // Live changes, animated: rows that appear slide in, changed cells flash, removed rows fade out (as "ghost" rows
  // kept for a moment where they were). Diffed per document id, so rows scrolled into view mid-animation pick it up
  // where it is, and scrolling itself animates nothing.
  const FLASH_MS = 1800;
  const GHOST_MS = 700;
  /** Diffing stops past this many loaded rows (it serializes every row on every update). */
  const DIFF_LIMIT = 5000;
  let recent = $state.raw(new Map<string, { kind: "inserted" | "changed"; fields: string[]; at: number }>());
  let ghosts = $state.raw<(Removed & { at: number })[]>([]);
  let last: { key: string; snap: Snapshot; settled: boolean } | null = null;
  $effect(() => {
    const results = docs.results;
    const status = docs.status;
    const key = queryKey;
    untrack(() => {
      if (results.length > DIFF_LIMIT) {
        last = null;
        return;
      }
      const snap = snapshot(results, status !== "Exhausted");
      const prev = last;
      // A first page (or a new query) is not a change.
      last = { key, snap, settled: status !== "LoadingFirstPage" };
      if (!prev || prev.key !== key || !prev.settled) {
        if (prev?.key !== key) {
          recent = new Map();
          ghosts = [];
        }
        return;
      }
      const changes = diffSnapshots(prev.snap, snap);
      const now = performance.now();
      if (changes.inserted.length > 0 || changes.changed.size > 0) {
        const next = new Map([...recent].filter(([, r]) => now - r.at < FLASH_MS));
        for (const id of changes.inserted) next.set(id, { kind: "inserted", fields: [], at: now });
        for (const [id, fields] of changes.changed) next.set(id, { kind: "changed", fields, at: now });
        recent = next;
      }
      if (changes.removed.length > 0) {
        ghosts = [...ghosts.filter((g) => now - g.at < GHOST_MS), ...changes.removed.map((r) => ({ ...r, at: now }))];
        setTimeout(() => (ghosts = ghosts.filter((g) => performance.now() - g.at < GHOST_MS)), GHOST_MS + 20);
      }
    });
  });
  const display = $derived(withGhosts(rows, needle ? ghosts.filter((g) => documentMatches(g.doc, needle)) : ghosts));

  const reducedMotion = typeof matchMedia === "function" && matchMedia("(prefers-reduced-motion: reduce)").matches;
  type Flash = { kind: "inserted" | "changed" | "removed"; at: number } | undefined;
  /** A theme color (`--ok`, ...) at `alpha`. */
  function tint(node: HTMLElement, name: string, alpha: number): string {
    const hex = getComputedStyle(node).getPropertyValue(name).trim();
    const m = /^#([0-9a-f]{2})([0-9a-f]{2})([0-9a-f]{2})$/i.exec(hex);
    return m ? `rgb(${parseInt(m[1]!, 16)} ${parseInt(m[2]!, 16)} ${parseInt(m[3]!, 16)} / ${alpha})` : hex;
  }
  function keyframes(node: HTMLElement, kind: "inserted" | "changed" | "removed"): Keyframe[] {
    if (kind === "inserted") {
      const c = tint(node, "--ok", 0.22);
      return reducedMotion
        ? [{ backgroundColor: c }, { backgroundColor: c, offset: 0.4 }]
        : [
            { backgroundColor: c, opacity: 0, transform: "translateY(-10px)" },
            { backgroundColor: c, opacity: 1, transform: "none", offset: 0.12 },
            { backgroundColor: c, offset: 0.4 },
          ];
    }
    if (kind === "changed") {
      const c = tint(node, "--warn", 0.3);
      return [{ backgroundColor: c }, { backgroundColor: c, offset: 0.35 }];
    }
    const c = tint(node, "--err", 0.16);
    return [
      { backgroundColor: c, opacity: 1 },
      { backgroundColor: c, opacity: 0 },
    ];
  }
  /** Plays a row's or cell's animation for a change (once per change; partway through if it started earlier). */
  function flash(node: HTMLElement, change: Flash) {
    let played = 0;
    const play = (c: Flash) => {
      if (!c || c.at === played || typeof node.animate !== "function") return;
      played = c.at;
      const duration = c.kind === "removed" ? GHOST_MS : FLASH_MS;
      const age = performance.now() - c.at;
      if (age >= duration) return;
      const timing: KeyframeAnimationOptions = { duration, delay: -age, easing: "ease-out", fill: c.kind === "removed" ? "forwards" : "none" };
      node.animate(keyframes(node, c.kind), timing);
      // A removed row then closes up: its cells shrink to nothing, so the rows below slide up instead of jumping.
      if (c.kind === "removed" && !reducedMotion) {
        for (const cell of node.children) {
          (cell as HTMLElement).animate(
            [
              { height: `${ROW}px`, lineHeight: `${ROW - 1}px`, offset: 0.55 },
              { height: "0px", lineHeight: "0px", fontSize: "0px", borderBottomWidth: "0px" },
            ],
            { ...timing, easing: "ease-in-out" },
          );
        }
      }
    };
    play(change);
    return { update: play };
  }

  // Virtualization: the window of rows to render, from the scroll position.
  let scroller: HTMLDivElement | undefined = $state();
  let scrollTop = $state(0);
  let viewport = $state(600);
  const win = $derived(
    rowWindow({ scrollTop: Math.max(0, scrollTop - HEADER), viewportHeight: viewport - HEADER, rowHeight: ROW, count: display.length }),
  );
  // Load the next page when the window nears the end of what is loaded (not while quick-filtering: a filter that
  // matches nothing would otherwise page through the whole table).
  $effect(() => {
    if (docs.status === "CanLoadMore" && !needle && nearEnd(win, display.length, 60)) docs.loadMore(PAGE);
  });
  // A new query starts at the top. Keyed by names, not the index objects: the table summary (and so every derived
  // object) is re-sent on every write, and a primitive derived only notifies when its value changes.
  const queryKey = $derived(JSON.stringify([mode, applied, order, index?.name, searchIndex?.name, searchText]));
  $effect(() => {
    void queryKey;
    if (scroller) scroller.scrollTop = 0;
  });

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

  async function exportAs(format: ExportFormat) {
    exporting = format;
    try {
      saveBlob(await exportTable(ctx.auth, table.name, format));
      ctx.toast(`Exported ${table.name} as ${format.toUpperCase()}`);
    } catch (e) {
      ctx.toast(`Export failed: ${e instanceof Error ? e.message : String(e)}`, "error");
    } finally {
      exporting = null;
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

  const tokenizerHelp: Record<string, string> = {
    unicode61: "whole words",
    porter: "English stems: “run” finds “running”",
    trigram: "substrings of 3+ characters",
    snowball: "stems: inflected forms match each other",
  };
</script>

<div class="page">
  <header class="page-header">
    <h1>{table.name}</h1>
    <span class="muted">{table.documents} document{table.documents === 1 ? "" : "s"}</span>
    {#if table.access === "readOnly"}<span class="badge" title="System tables can't be edited here">read-only</span>{/if}
    {#if table.access === "limited"}<span class="badge" title="Only name, image and emailVerified are editable">limited edits</span>{/if}
    {#if !table.defined}<span class="badge" title="No longer defined in code">not in code</span>{/if}
    {#if migrating}
      <a class="badge migrating" href={href("migrations")} onclick={link} title="Documents may not match the type until its pending migrations finish; writes are lenient"
        >migrating</a
      >
    {/if}
    <span class="spacer"></span>
    <button class="ghost" onclick={() => (showSchema = !showSchema)} aria-expanded={showSchema}>Schema</button>
    <div class="export" role="group" aria-label="Export">
      <button class="small" onclick={() => exportAs("jsonl")} disabled={exporting !== null} title="Download every document as JSON lines">
        {exporting === "jsonl" ? "Exporting…" : "⇩ JSONL"}
      </button>
      <button class="small" onclick={() => exportAs("csv")} disabled={exporting !== null} title="Download every document as CSV">
        {exporting === "csv" ? "Exporting…" : "⇩ CSV"}
      </button>
    </div>
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
          <ul data-testid="search-indexes">
            {#each table.searchIndexes as s (s.name)}
              <li>
                <code>{s.name}</code>
                {#if s.kind === "vector"}
                  <span class="badge">vector</span>
                  <span class="muted"
                    >{s.field} · {s.dimensions} dims · {s.method ?? "exact"}{s.method === "hnsw" ? " (approximate)" : ""}{s.hnsw
                      ? ` · m ${s.hnsw.m}, ef_construction ${s.hnsw.efConstruction}, ef_search ${s.hnsw.efSearch}`
                      : ""}</span
                  >
                {:else}
                  <span class="badge">text</span>
                  <span class="muted"
                    >{s.field} · tokenizer {s.tokenizer ?? "unicode61"}{s.language ? ` (${s.language})` : ""}{s.removeDiacritics === false
                      ? " · diacritics kept"
                      : ""}</span
                  >
                {/if}
                {#if s.filterFields.length > 0}<span class="muted"> · filter [{s.filterFields.join(", ")}]</span>{/if}
              </li>
            {/each}
          </ul>
        {/if}
        {#if table.aggregates && table.aggregates.length > 0}
          <h3>Aggregates</h3>
          <ul data-testid="aggregates">
            {#each table.aggregates as a (a.name)}
              <li>
                <code>{a.name}</code>
                <span class="muted"
                  >count{a.sumFields.length > 0 ? `, sum of ${a.sumFields.join(", ")}` : ""}{a.groupFields.length > 0
                    ? ` per ${a.groupFields.join(", ")}`
                    : " (whole table)"}</span
                >
              </li>
            {/each}
          </ul>
        {/if}
        {#if table.secretFields.length > 0}
          <p class="faint">Masked: {table.secretFields.join(", ")}</p>
        {/if}
        {#if table.columns && table.columns.length > 0}
          <h3>Analytics columns <span class="faint">(SQL in <code>ctx.olap</code>, with <code>--olap</code>)</span></h3>
          <ul class="columns" data-testid="analytics-columns">
            {#each table.columns as c (c.name)}
              <li><code>{c.name}</code> <span class="muted">{c.sqlType}</span></li>
            {/each}
          </ul>
        {:else if table.analytics === false}
          <p class="faint" data-testid="analytics-columns">Not in the analytics copy: it holds secrets or admin-only data.</p>
        {/if}
      </div>
    </div>
  {/if}

  <div class="filters row">
    {#if searchIndexes.length > 0}
      <div class="modes" role="group" aria-label="Mode">
        <button class="small" class:active={mode === "browse"} aria-pressed={mode === "browse"} onclick={() => (mode = "browse")}>Browse</button>
        <button class="small" class:active={mode === "search"} aria-pressed={mode === "search"} onclick={() => (mode = "search")}>Search</button>
      </div>
    {/if}
    {#if mode === "browse"}
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
      <button class="small" onclick={() => (order = order === "asc" ? "desc" : "asc")} title="Order along the index">
        {order === "desc" ? "↓ newest first" : "↑ oldest first"}
      </button>
    {:else if searchIndex}
      {#if searchIndexes.length > 1}
        <select aria-label="Search index" value={searchIndex.name} onchange={(e) => (searchIndexName = e.currentTarget.value)}>
          {#each searchIndexes as s (s.name)}<option value={s.name}>{s.name}</option>{/each}
        </select>
      {/if}
      <input
        class="search-box"
        type="search"
        placeholder={`Search ${searchIndex.field}…`}
        aria-label="Search documents"
        value={searchInput}
        oninput={(e) => onSearchInput(e.currentTarget.value)}
      />
      {#each searchIndex.filterFields as field (field)}
        <span class="field">
          <span class="mono fname">{field} =</span>
          <input
            class="mono"
            aria-label={`Search filter ${field}`}
            placeholder="any"
            value={searchFilters[field] ?? ""}
            oninput={(e) => (searchFilters = { ...searchFilters, [field]: e.currentTarget.value })}
          />
        </span>
      {/each}
      <span class="faint small" data-testid="search-info"
        ><code>{searchIndex.name}</code> · {searchIndex.tokenizer ?? "unicode61"} ({tokenizerHelp[searchIndex.tokenizer ?? "unicode61"]}){searchIndex
          .filterFields.length
          ? ` · filters: ${searchIndex.filterFields.join(", ")}`
          : ""} · most relevant first</span
      >
    {/if}
    <span class="spacer"></span>
    <input
      class="quick"
      type="search"
      placeholder="Filter loaded rows"
      aria-label="Filter loaded rows"
      title="Matches any value of the rows loaded so far (client-side; not an index search)"
      bind:value={quick}
    />
  </div>

  <div class="body" bind:this={scroller} bind:clientHeight={viewport} onscroll={(e) => (scrollTop = e.currentTarget.scrollTop)}>
    {#if docs.error}
      <div class="alert" role="alert">{docs.error.message}</div>
    {:else if mode === "search" && !searching}
      <Empty
        title={`Search ${table.name}`}
        hint={searchIndex
          ? `Type words to find in ${searchIndex.field}: every word must match (the last one as a prefix), most relevant first. Results stay live.`
          : ""}
      />
    {:else if docs.isLoading && docs.results.length === 0}
      <Empty title="Loading…" />
    {:else if docs.results.length === 0 && ghosts.length === 0}
      {#if searching}
        <Empty title="No documents match this search" hint="Try fewer or shorter words." />
      {:else}
        <Empty
          title={applied.length > 0 ? "No documents match these filters" : "No documents yet"}
          hint={applied.length > 0 ? "Change or clear the filters." : writable ? "Insert one here, or write from your app." : ""}
        >
          {#if writable && applied.length === 0}<button onclick={() => oninsert({})}>+ Insert document</button>{/if}
        </Empty>
      {/if}
    {:else}
      <table class="grid vgrid" data-testid="document-grid" style:width={`${totalWidth}px`} aria-rowcount={display.length + 1}>
        <colgroup>
          {#if writable}<col style:width="32px" />{/if}
          {#each widths as w, i (columns[i])}<col style:width={`${w}px`} />{/each}
        </colgroup>
        <thead>
          <tr>
            {#if writable}<th class="check"></th>{/if}
            {#each columns as c (c)}<th title={c}>{c}</th>{/each}
          </tr>
        </thead>
        <tbody>
          {#if win.padTop > 0}
            <tr class="spacer" aria-hidden="true"><td colspan={columns.length + 1} style:height={`${win.padTop}px`}></td></tr>
          {/if}
          {#each display.slice(win.start, win.end) as { doc, ghost }, i (ghost ? `ghost:${doc._id}` : doc._id)}
            {@const id = doc._id as string}
            {@const change = ghost ? undefined : recent.get(id)}
            {#if ghost}
              {@const gone = ghosts.find((g) => g.id === id)}
              <tr class="ghost" aria-hidden="true" data-testid="removed-row" use:flash={gone && { kind: "removed", at: gone.at }}>
                {#if writable}<td class="check"></td>{/if}
                {#each columns as c (c)}
                  <td><JsonValue value={doc[c]} field={c} /></td>
                {/each}
              </tr>
            {:else}
              <tr
                class:selected={id === selectedId}
                onclick={() => open(id)}
                aria-rowindex={win.start + i + 2}
                data-row={win.start + i}
                use:flash={change?.kind === "inserted" ? change : undefined}
              >
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
                  <td use:flash={change?.kind === "changed" && change.fields.includes(c) ? change : undefined}>
                    <JsonValue value={doc[c]} field={c} />
                  </td>
                {/each}
              </tr>
            {/if}
          {/each}
          {#if win.padBottom > 0}
            <tr class="spacer" aria-hidden="true"><td colspan={columns.length + 1} style:height={`${win.padBottom}px`}></td></tr>
          {/if}
        </tbody>
      </table>
      <div class="more" data-testid="grid-status">
        {#if needle}
          <span class="faint">{rows.length} of {docs.results.length} loaded rows match “{quick.trim()}”</span>
          {#if docs.status === "CanLoadMore"}<button class="small" onclick={() => docs.loadMore(PAGE)}>Load {PAGE} more</button>{/if}
        {:else if docs.status === "CanLoadMore"}
          <span class="faint">{docs.results.length} loaded · scroll for more</span>
          <button class="small" onclick={() => docs.loadMore(PAGE)}>Load more</button>
        {:else if docs.status === "LoadingMore"}
          <span class="faint">{docs.results.length} loaded · loading…</span>
        {:else}
          <span class="faint">{docs.results.length} shown · end of results</span>
        {/if}
      </div>
    {/if}
  </div>
</div>

<style>
  .badge.migrating {
    color: var(--warn);
    background: var(--warn-soft);
    border-color: transparent;
    text-decoration: none;
  }
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
  .modes {
    display: inline-flex;
  }
  .modes button {
    border-radius: 0;
  }
  .modes button:first-child {
    border-radius: var(--radius) 0 0 var(--radius);
  }
  .modes button:last-child {
    border-radius: 0 var(--radius) var(--radius) 0;
    border-left: none;
  }
  .modes button.active {
    background: var(--accent-soft);
    color: var(--accent-text);
    font-weight: 600;
  }
  .search-box {
    width: 260px;
  }
  .quick {
    width: 200px;
  }
  .small {
    font-size: 12.5px;
  }
  .export {
    display: inline-flex;
    gap: 4px;
  }
  .body {
    flex: 1;
    overflow: auto;
    min-height: 0;
  }
  table.vgrid {
    table-layout: fixed;
    min-width: 100%;
  }
  .vgrid th {
    height: 32px;
    padding-top: 0;
    padding-bottom: 0;
    line-height: 31px;
  }
  .vgrid tbody td {
    height: 32px;
    padding-top: 0;
    padding-bottom: 0;
    line-height: 31px;
    vertical-align: middle;
    max-width: none;
  }
  .vgrid tr.ghost {
    cursor: default;
    pointer-events: none;
    text-decoration: line-through;
    text-decoration-color: var(--err);
  }
  .vgrid tr.spacer td {
    padding: 0;
    border: none;
    line-height: 0;
  }
  .vgrid tr.spacer {
    cursor: default;
  }
  .vgrid tr.spacer:hover {
    background: none;
  }
  .check {
    padding-right: 0 !important;
  }
  .check input {
    vertical-align: middle;
  }
  .more {
    display: flex;
    gap: 12px;
    align-items: center;
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
  .schema li {
    margin: 2px 0;
  }
  .schema ul.columns {
    columns: 2;
    font-size: 12px;
  }
  .schema pre {
    background: var(--panel-2);
    padding: 8px;
    border-radius: var(--radius);
  }
</style>

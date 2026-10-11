<script lang="ts">
  import { FerrixError } from "@ferrix/client";
  import { useAction, usePaginatedQuery, useQuery } from "@ferrix/svelte";
  import { dashboard, type FunctionSummary, type Json, type RunResult } from "../lib/api.js";
  import { parseJson, pretty } from "../lib/format.js";
  import { href, link, router, type Route } from "../lib/router.svelte.js";
  import Empty from "../components/Empty.svelte";
  import FunctionActivity from "../components/FunctionActivity.svelte";
  import JsonEditor from "../components/JsonEditor.svelte";

  let { route }: { route: Route } = $props();
  const functions = useQuery(dashboard.functions);
  const users = usePaginatedQuery(dashboard.users, () => ({}), { initialNumItems: 100 });
  const runFunction = useAction(dashboard.runFunction);

  let search = $state("");
  let showSystem = $state(false);
  let searchInput: HTMLInputElement | undefined = $state();

  const selectedPath = $derived(route.params.get("fn"));
  const selected = $derived(functions.data?.find((f) => f.path === selectedPath));
  const visible = $derived(
    (functions.data ?? []).filter(
      (f) => (showSystem || !f.system || f.path === selectedPath) && f.path.toLowerCase().includes(search.toLowerCase()),
    ),
  );
  const groups = $derived.by(() => {
    const out = new Map<string, FunctionSummary[]>();
    for (const f of visible) {
      const module = f.path.includes(":") ? f.path.slice(0, f.path.lastIndexOf(":")) : "(root)";
      out.set(module, [...(out.get(module) ?? []), f]);
    }
    return [...out.entries()];
  });

  let args = $state("{}");
  let asUser = $state("");
  let running = $state(false);
  let result = $state<RunResult | null>(null);
  let error = $state<string | null>(null);

  // A different function starts with fresh args and no result.
  $effect(() => {
    void selectedPath;
    args = "{}";
    result = null;
    error = null;
  });

  async function run() {
    if (!selected) return;
    const parsed = parseJson(args.trim() === "" ? "{}" : args);
    if (!parsed.ok) {
      error = `Invalid JSON: ${parsed.error}`;
      return;
    }
    running = true;
    error = null;
    try {
      result = await runFunction({ path: selected.path, args: parsed.value as Json, asUser: asUser || null });
    } catch (e) {
      result = null;
      error = e instanceof FerrixError ? e.message : String(e);
    } finally {
      running = false;
    }
  }

  function onkeydown(e: KeyboardEvent) {
    if (e.key === "/" && !(e.target instanceof HTMLInputElement || e.target instanceof HTMLTextAreaElement)) {
      e.preventDefault();
      searchInput?.focus();
    }
  }
</script>

<svelte:window {onkeydown} />

<div class="layout" class:has-selection={!!selected}>
  <aside class="list" aria-label="Functions">
    <div class="tools">
      <input bind:this={searchInput} bind:value={search} placeholder="Search functions ( / )" aria-label="Search functions" />
      <label class="inline"><input type="checkbox" bind:checked={showSystem} /> show system</label>
    </div>
    {#if functions.isLoading}
      <p class="faint pad">Loading…</p>
    {:else if visible.length === 0}
      <Empty title="No functions" hint={search ? "Nothing matches the search." : "Define #[query], #[mutation] or #[action] functions."} />
    {:else}
      {#each groups as [module, fns] (module)}
        <h3>{module}</h3>
        <ul>
          {#each fns as f (f.path)}
            <li>
              <a href={href("functions", [], { fn: f.path })} onclick={link} class:active={f.path === selectedPath}>
                <span class="badge {f.kind}">{f.kind[0]!.toUpperCase()}</span>
                <span class="fname mono">{f.path.slice(f.path.lastIndexOf(":") + 1)}</span>
                {#if f.visibility === "internal"}<span class="faint small">internal</span>{/if}
              </a>
            </li>
          {/each}
        </ul>
      {/each}
    {/if}
  </aside>

  <section class="page">
    {#if !selected}
      <Empty title="Select a function" hint="Pick a function to see its signature and run it with JSON args, as yourself or as a user." />
    {:else}
      <header class="page-header">
        <a class="back" href={href("functions")} onclick={link}>← Functions</a>
        <h1 class="mono">{selected.path}</h1>
        <span class="badge {selected.kind}">{selected.kind}</span>
        <span class="badge">{selected.visibility}</span>
        {#if selected.system}<span class="badge">system</span>{/if}
      </header>
      <div class="page-body">
        <div class="sig">
          <div>
            <h3>Args</h3>
            <pre class="ts">{selected.argsTs}</pre>
          </div>
          <div>
            <h3>Returns</h3>
            <pre class="ts">{selected.outputTs}</pre>
          </div>
        </div>

        <h3>Run</h3>
        <JsonEditor label="Arguments JSON" bind:value={args} rows={6} onsubmit={run} />
        <div class="row runbar">
          <label class="inline"
            >Act as
            <select bind:value={asUser} aria-label="Act as user">
              <option value="">Administrator (you)</option>
              {#each users.results as u (u._id)}
                <option value={u._id}>{u.email ?? u.name ?? u._id}</option>
              {/each}
            </select>
          </label>
          <span class="spacer"></span>
          <button class="primary" onclick={run} disabled={running}>{running ? "Running…" : "Run"} <kbd>Ctrl ↵</kbd></button>
        </div>
        {#if error}<div class="alert" role="alert">{error}</div>{/if}

        {#if result}
          <div class="result card" data-testid="run-result">
            <div class="row">
              <span class="badge {result.success ? 'success' : 'serverError'}">{result.success ? "success" : "error"}</span>
              <span class="muted">{result.durationMs.toFixed(1)} ms</span>
              {#if result.ts !== null}<span class="muted">ts {result.ts}</span>{/if}
              {#if result.identity}<span class="muted mono">as {result.identity}</span>{/if}
            </div>
            {#if result.error}
              <h3>Error{result.error.code ? ` (${result.error.code})` : ""}</h3>
              <pre class="err">{result.error.message}</pre>
              {#if result.error.data !== null}<pre>{pretty(result.error.data)}</pre>{/if}
            {:else}
              <h3>Result</h3>
              <pre class="value">{pretty(result.value)}</pre>
            {/if}
            <h3>Logs</h3>
            {#if result.logLines.length === 0}
              <p class="faint">No log lines.</p>
            {:else}
              <pre class="logs">{result.logLines.join("\n")}</pre>
            {/if}
            <a href={href("logs")} onclick={link} class="small">Open in logs →</a>
          </div>
        {/if}

        <FunctionActivity path={selected.path} />
      </div>
    {/if}
  </section>
</div>

<style>
  .layout {
    display: flex;
    height: 100%;
  }
  .list {
    width: 280px;
    flex: none;
    border-right: 1px solid var(--border);
    background: var(--panel);
    overflow: auto;
    padding: 10px 8px;
  }
  .tools {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 0 4px;
  }
  .pad {
    padding: 0 8px;
  }
  h3 {
    margin: 14px 8px 4px;
  }
  .page-body h3 {
    margin: 16px 0 6px;
  }
  ul {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  li a {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 4px 8px;
    border-radius: var(--radius);
    color: var(--text);
    text-decoration: none;
  }
  li a:hover {
    background: var(--panel-2);
  }
  li a.active {
    background: var(--accent-soft);
  }
  .fname {
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .small {
    font-size: 12px;
  }
  .page {
    flex: 1;
    min-width: 0;
  }
  .sig {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 16px;
  }
  pre.ts,
  .result pre {
    background: var(--panel-2);
    padding: 8px 10px;
    border-radius: var(--radius);
  }
  .runbar {
    margin: 8px 0;
  }
  .result {
    padding: 12px 14px;
    margin-top: 8px;
  }
  pre.err {
    color: var(--err);
  }
  .back {
    display: none;
  }
  /* Phones: the list, or the selected function with a link back to the list. */
  @media (max-width: 760px) {
    .list {
      width: 100%;
      border-right: 0;
    }
    .layout.has-selection .list,
    .layout:not(.has-selection) .page {
      display: none;
    }
    .back {
      display: block;
      flex-basis: 100%;
      font-size: 13px;
    }
    h1.mono {
      font-size: 15px;
      word-break: break-all;
    }
    .sig {
      grid-template-columns: 1fr;
    }
  }
</style>

<script lang="ts">
  import { useQuery } from "@rxferrix/svelte";
  import { migrations, type JsonObject } from "../lib/api.js";
  import { dashboardContext } from "../lib/context.js";
  import { href, link, router, type Route } from "../lib/router.svelte.js";
  import Empty from "../components/Empty.svelte";
  import DocumentGrid from "./DocumentGrid.svelte";
  import DocumentPanel from "./DocumentPanel.svelte";

  let { route }: { route: Route } = $props();
  const ctx = dashboardContext();

  const tables = $derived(ctx.tables ?? []);
  const appTables = $derived(tables.filter((t) => !t.system));
  const systemTables = $derived(tables.filter((t) => t.system));
  const tableName = $derived(route.rest[0]);
  const table = $derived(tables.find((t) => t.name === tableName));
  const docId = $derived(route.params.get("doc"));
  let inserting = $state<JsonObject | null>(null);
  let filter = $state("");
  // Tables opened in lenient "migrating" mode (documents that don't match their type, migrations pending).
  const migrationList = useQuery(migrations.list, () => ({}));
  const migrating = $derived(new Set((migrationList.data ?? []).filter((m) => m.tableMigrating).map((m) => m.table)));

  // /_/data opens the first app table (or the first table).
  $effect(() => {
    if (!tableName && tables.length > 0) {
      router.navigate(href("data", [(appTables[0] ?? tables[0])!.name]), { replace: true });
    }
  });
  // Switching tables closes the insert panel.
  $effect(() => {
    void tableName;
    inserting = null;
  });

  function matches(name: string) {
    return !filter || name.toLowerCase().includes(filter.toLowerCase());
  }

  function closePanel() {
    inserting = null;
    if (docId && tableName) router.navigate(href("data", [tableName]));
  }
</script>

<div class="layout">
  <aside class="tables" aria-label="Tables">
    <input class="search" placeholder="Filter tables" aria-label="Filter tables" bind:value={filter} />
    {#if ctx.tables === undefined}
      <p class="faint pad">Loading…</p>
    {:else}
      <h3>Tables</h3>
      {#if appTables.length === 0}<p class="faint pad">No app tables.</p>{/if}
      <ul>
        {#each appTables.filter((t) => matches(t.name)) as t (t.name)}
          <li>
            <a href={href("data", [t.name])} onclick={link} class:active={t.name === tableName}>
              <span class="name">{t.name}</span>
              {#if migrating.has(t.name)}<span class="mig" title="Migrating: documents may not match the type until its migrations finish">⇄</span>{/if}
              <span class="count">{t.documents}</span>
            </a>
          </li>
        {/each}
      </ul>
      <h3>System</h3>
      <ul>
        {#each systemTables.filter((t) => matches(t.name)) as t (t.name)}
          <li>
            <a href={href("data", [t.name])} onclick={link} class:active={t.name === tableName}>
              <span class="name system">{t.name}</span><span class="count">{t.documents}</span>
            </a>
          </li>
        {/each}
      </ul>
    {/if}
  </aside>

  <section class="main">
    {#if ctx.tables !== undefined}
      <select
        class="table-picker"
        aria-label="Table"
        value={tableName ?? ""}
        onchange={(e) => router.navigate(href("data", [e.currentTarget.value]))}
      >
        <optgroup label="Tables">
          {#each appTables as t (t.name)}<option value={t.name}>{t.name} ({t.documents})</option>{/each}
        </optgroup>
        <optgroup label="System">
          {#each systemTables as t (t.name)}<option value={t.name}>{t.name} ({t.documents})</option>{/each}
        </optgroup>
      </select>
    {/if}
    {#if table}
      {#key table.name}
        <DocumentGrid
          {table}
          migrating={migrating.has(table.name)}
          selectedId={docId}
          oninsert={(template) => {
            inserting = template;
            if (docId) router.navigate(href("data", [table.name]));
          }}
        />
      {/key}
    {:else if ctx.tables !== undefined}
      <Empty
        title={tableName ? `Table “${tableName}” does not exist` : "No tables"}
        hint="Tables appear here once a #[table] is defined in your backend."
      />
    {/if}
  </section>

  {#if table && (docId || inserting)}
    {#key docId ?? "insert"}
      <DocumentPanel {table} id={docId} template={inserting} onclose={closePanel} />
    {/key}
  {/if}
</div>

<style>
  .layout {
    display: flex;
    height: 100%;
    min-width: 0;
  }
  .tables {
    width: 220px;
    flex: none;
    border-right: 1px solid var(--border);
    background: var(--panel);
    overflow: auto;
    padding: 10px 8px;
  }
  .search {
    width: 100%;
    margin-bottom: 4px;
  }
  .pad {
    padding: 0 8px;
    font-size: 13px;
  }
  h3 {
    margin: 14px 8px 4px;
  }
  ul {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  li a {
    display: flex;
    justify-content: space-between;
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
    color: var(--accent-text);
    font-weight: 600;
  }
  .name {
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .name.system {
    font-family: var(--mono);
    font-size: 12.5px;
  }
  .mig {
    color: var(--warn);
    margin-left: auto;
  }
  .count {
    color: var(--faint);
    font-variant-numeric: tabular-nums;
    font-size: 12.5px;
  }
  .main {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
  }
  .table-picker {
    display: none;
  }
  @media (max-width: 760px) {
    .tables {
      display: none;
    }
    .main {
      display: flex;
      flex-direction: column;
    }
    .table-picker {
      display: block;
      margin: 10px 12px 0;
    }
  }
</style>

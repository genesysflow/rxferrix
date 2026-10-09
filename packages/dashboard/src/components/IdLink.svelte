<script lang="ts">
  import { dashboardContext } from "../lib/context.js";
  import { idTableNumber } from "../lib/format.js";
  import { href, link } from "../lib/router.svelte.js";

  let { id }: { id: string } = $props();
  const ctx = dashboardContext();
  const table = $derived.by(() => {
    const n = idTableNumber(id);
    return n === null ? undefined : ctx.tableByNumber(n);
  });
</script>

{#if table}
  <a
    class="id mono"
    href={href("data", [table.name], { doc: id })}
    title={`Open ${table.name} ${id}`}
    onclick={(e) => {
      e.stopPropagation();
      link(e);
    }}>{id}</a
  >
{:else}
  <span class="mono">{id}</span>
{/if}

<style>
  .id {
    color: var(--link);
  }
</style>

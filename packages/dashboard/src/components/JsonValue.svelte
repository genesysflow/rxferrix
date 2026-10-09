<script lang="ts">
  import type { Json } from "../lib/api.js";
  import { dashboardContext } from "../lib/context.js";
  import { compactJson, formatTime, idTableNumber } from "../lib/format.js";
  import IdLink from "./IdLink.svelte";

  /** One value, rendered compactly: ids link to their document, strings plain, the rest as one-line JSON. */
  let { value, field = "", max = 80 }: { value: Json | undefined; field?: string; max?: number } = $props();
  const ctx = dashboardContext();
  const isId = $derived.by(() => {
    const n = idTableNumber(value);
    return n !== null && ctx.tableByNumber(n) !== undefined;
  });
</script>

{#if value === undefined}
  <span class="faint">—</span>
{:else if isId}
  <IdLink id={value as string} />
{:else if value === null}
  <span class="null">null</span>
{:else if typeof value === "string"}
  <span class="string" title={value.length > max ? value : undefined}>{compactJson(value, max)}</span>
{:else if typeof value === "number" && field === "_creationTime"}
  <span class="number" title={String(value)}>{formatTime(value)}</span>
{:else if typeof value === "number"}
  <span class="number" title={/At$|Time$/.test(field) ? formatTime(value) : undefined}
    >{value}</span
  >
{:else if typeof value === "boolean"}
  <span class="bool">{value}</span>
{:else}
  <span class="mono other" title={JSON.stringify(value)}>{compactJson(value, max)}</span>
{/if}

<style>
  .null,
  .bool {
    color: var(--accent-text);
    font-family: var(--mono);
    font-size: 12.5px;
  }
  .number {
    color: var(--link);
    font-family: var(--mono);
    font-size: 12.5px;
  }
  .other {
    color: var(--muted);
  }
</style>

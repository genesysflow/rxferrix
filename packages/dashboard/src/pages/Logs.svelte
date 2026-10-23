<script lang="ts">
  import type { LogFilter } from "../lib/api.js";
  import { background } from "../lib/background.svelte.js";
  import { href, link, router, type Route } from "../lib/router.svelte.js";
  import BackgroundToggle from "../components/BackgroundToggle.svelte";
  import LogTail from "../components/LogTail.svelte";

  /** `?fn=<path>`: only that function (the function view links here). */
  let { route }: { route: Route } = $props();
  const fn = $derived(route.params.get("fn"));

  let pathPrefix = $state("");
  let kind = $state<LogFilter["kind"]>("");
  let outcome = $state<LogFilter["outcome"]>("");
  let includeSystem = $state(false);
  let paused = $state(false);
  let status = $state<"connecting" | "live" | "error">("connecting");
  let statusError = $state<string | null>(null);
  let lagged = $state(0);
  let pathInput: HTMLInputElement | undefined = $state();

  const filter = $derived<LogFilter>({
    path: fn ?? undefined,
    pathPrefix: fn ? undefined : pathPrefix.trim() || undefined,
    kind,
    outcome,
    excludeSystem: !includeSystem && !fn,
    excludeBackground: background.hidden,
  });

  function onkeydown(e: KeyboardEvent) {
    if (e.key === "/" && !(e.target instanceof HTMLInputElement || e.target instanceof HTMLTextAreaElement)) {
      e.preventDefault();
      pathInput?.focus();
    }
  }
</script>

<svelte:window {onkeydown} />

<div class="page">
  <header class="page-header">
    <h1>Logs</h1>
    <span class="state" class:live={status === "live" && !paused} title={statusError ?? ""}>
      <span class="dot"></span>{paused ? "paused" : status === "live" ? "live" : status === "error" ? "reconnecting" : "connecting"}
    </span>
    {#if lagged > 0}<span class="badge userError" title="The tail fell behind; refresh to see everything">{lagged} skipped</span>{/if}
    <span class="spacer"></span>
    <button onclick={() => (paused = !paused)}>{paused ? "▶ Resume" : "❚❚ Pause"}</button>
  </header>
  <div class="filters row">
    {#if fn}
      <span class="fn-chip" data-testid="log-function">
        <span class="mono">{fn}</span>
        <a href={href("functions", [], { fn })} onclick={link} title="Open the function">ƒ</a>
        <button class="ghost small" aria-label="Show all functions" onclick={() => router.navigate(href("logs"))}>×</button>
      </span>
    {:else}
      <input bind:this={pathInput} class="mono" placeholder="Path prefix, e.g. todos:  ( / )" aria-label="Path prefix" bind:value={pathPrefix} />
    {/if}
    <select bind:value={kind} aria-label="Kind">
      <option value="">All kinds</option><option value="query">query</option><option value="mutation">mutation</option><option value="action">action</option>
    </select>
    <select bind:value={outcome} aria-label="Outcome">
      <option value="">All outcomes</option><option value="success">success</option><option value="userError">user error</option><option value="serverError">server error</option>
    </select>
    {#if !fn}<label class="inline"><input type="checkbox" bind:checked={includeSystem} /> include system functions</label>{/if}
    <BackgroundToggle />
  </div>

  <div class="body">
    <LogTail {filter} {paused} bind:status bind:statusError bind:lagged />
  </div>
</div>

<style>
  .filters {
    padding: 8px 20px;
    border-bottom: 1px solid var(--border);
    background: var(--panel);
  }
  .filters input.mono {
    width: 280px;
  }
  .body {
    flex: 1;
    overflow: auto;
  }
  .fn-chip {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    padding: 2px 4px 2px 10px;
    border: 1px solid var(--border-strong);
    border-radius: 999px;
    background: var(--accent-soft);
    color: var(--accent-text);
  }
  .fn-chip button {
    border: 0;
    padding: 0 6px;
    background: none;
  }
  .state {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    color: var(--muted);
    font-size: 12.5px;
  }
  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--faint);
  }
  .state.live .dot {
    background: var(--ok);
  }
  @media (max-width: 760px) {
    .filters {
      padding: 8px 12px;
    }
    .filters input.mono {
      width: 100%;
    }
  }
</style>

<script lang="ts">
  import type { Snippet } from "svelte";

  /**
   * A titled dashboard card: a header (title, optional actions, an optional `?` with help text), the body, and an
   * optional footer (a legend or a note). With `href` the whole card is a link.
   */
  let {
    title,
    help,
    href,
    onclick,
    warn = false,
    testid,
    actions,
    footer,
    children,
  }: {
    title: string;
    help?: string;
    href?: string;
    onclick?: (e: MouseEvent) => void;
    warn?: boolean;
    testid?: string;
    actions?: Snippet;
    footer?: Snippet;
    children: Snippet;
  } = $props();
</script>

{#snippet inner()}
  <header>
    <span class="title">{title}</span>
    <span class="spacer"></span>
    {@render actions?.()}
    {#if help}
      <span class="help" title={help} aria-label={help} role="img">?</span>
    {/if}
  </header>
  <div class="body">{@render children()}</div>
  {#if footer}<footer>{@render footer()}</footer>{/if}
{/snippet}

{#if href}
  <a class="card panel link" class:warn {href} {onclick} data-testid={testid}>{@render inner()}</a>
{:else}
  <section class="card panel" class:warn data-testid={testid}>{@render inner()}</section>
{/if}

<style>
  .panel {
    display: flex;
    flex-direction: column;
    min-width: 0;
    padding: 10px 14px 12px;
    color: inherit;
  }
  .panel.warn {
    border-color: var(--warn);
  }
  .link {
    text-decoration: none;
  }
  .link:hover {
    border-color: var(--border-strong);
    text-decoration: none;
  }
  header {
    display: flex;
    align-items: center;
    gap: 8px;
    min-height: 22px;
    margin-bottom: 8px;
  }
  .title {
    font-size: 13px;
    font-weight: 600;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .help {
    display: inline-grid;
    place-items: center;
    flex: none;
    width: 15px;
    height: 15px;
    border: 1px solid var(--faint);
    border-radius: 50%;
    color: var(--faint);
    font-size: 10px;
    font-weight: 600;
    line-height: 1;
    cursor: help;
  }
  .body {
    flex: 1;
    min-width: 0;
  }
  footer {
    margin-top: 8px;
    font-size: 12px;
    color: var(--muted);
  }
</style>

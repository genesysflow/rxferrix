<script lang="ts">
  import type { Snippet } from "svelte";

  let {
    title,
    hint = "",
    compact = false,
    children,
  }: { title: string; hint?: string; /** Inside a card: less padding. */ compact?: boolean; children?: Snippet } = $props();
</script>

<div class="empty" class:compact role="status">
  <div class="title">{title}</div>
  {#if hint}<div class="hint">{hint}</div>{/if}
  {#if children}<div class="actions">{@render children()}</div>{/if}
</div>

<style>
  .empty {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 6px;
    padding: 48px 16px;
    text-align: center;
    color: var(--muted);
  }
  .empty.compact {
    padding: 24px 16px;
  }
  .title {
    font-weight: 600;
    color: var(--text);
  }
  .hint {
    max-width: 440px;
    font-size: 13px;
    line-height: 1.5;
  }
  .actions {
    margin-top: 8px;
  }
</style>

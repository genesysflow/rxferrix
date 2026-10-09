<script lang="ts">
  import type { Snippet } from "svelte";

  let { title, hint = "", children }: { title: string; hint?: string; children?: Snippet } = $props();
</script>

<div class="empty" role="status">
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
  .title {
    font-weight: 600;
    color: var(--text);
  }
  .hint {
    max-width: 440px;
    font-size: 13px;
  }
  .actions {
    margin-top: 8px;
  }
</style>

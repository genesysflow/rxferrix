<script lang="ts">
  import type { Snippet } from "svelte";

  /** A modal dialog (native `<dialog>`: focus trapping, Escape closes). Mounted = open; `onclose` unmounts it. */
  let {
    title,
    onclose,
    wide = false,
    children,
    actions,
  }: { title: string; onclose: () => void; wide?: boolean; children: Snippet; actions?: Snippet } = $props();

  let dialog: HTMLDialogElement | undefined = $state();
  $effect(() => {
    if (dialog && !dialog.open) dialog.showModal?.();
  });
</script>

<dialog
  bind:this={dialog}
  class:wide
  aria-label={title}
  onclose={onclose}
  onclick={(e) => {
    // A click on the backdrop (the dialog element itself, outside its content) closes it.
    if (e.target === e.currentTarget) onclose();
  }}
>
  <div class="content">
    <header>
      <h2>{title}</h2>
      <button class="small ghost" onclick={onclose} aria-label="Close">✕</button>
    </header>
    <div class="body">{@render children()}</div>
    {#if actions}<footer class="row">{@render actions()}</footer>{/if}
  </div>
</dialog>

<style>
  dialog {
    padding: 0;
    border: 1px solid var(--border-strong);
    border-radius: 10px;
    background: var(--panel);
    color: var(--text);
    box-shadow: var(--shadow);
    width: min(520px, calc(100vw - 32px));
    max-height: calc(100vh - 64px);
  }
  dialog.wide {
    width: min(960px, calc(100vw - 32px));
  }
  dialog::backdrop {
    background: rgb(0 0 0 / 0.35);
  }
  .content {
    display: flex;
    flex-direction: column;
    max-height: calc(100vh - 66px);
  }
  header {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 12px 16px;
    border-bottom: 1px solid var(--border);
  }
  h2 {
    flex: 1;
    margin: 0;
    font-size: 15px;
  }
  .body {
    padding: 14px 16px;
    overflow: auto;
  }
  footer {
    justify-content: flex-end;
    padding: 10px 16px;
    border-top: 1px solid var(--border);
  }
</style>

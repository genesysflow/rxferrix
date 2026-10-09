<script lang="ts">
  import { parseJson } from "../lib/format.js";

  /**
   * A JSON textarea that validates as you type. Ctrl/Cmd+Enter calls `onsubmit` (only when the JSON is valid).
   */
  let {
    value = $bindable(""),
    label,
    rows = 12,
    readonly = false,
    onsubmit,
  }: { value?: string; label: string; rows?: number; readonly?: boolean; onsubmit?: () => void } = $props();

  const parsed = $derived(value.trim() === "" ? null : parseJson(value));
  const error = $derived(parsed && !parsed.ok ? parsed.error : null);

  function onkeydown(e: KeyboardEvent) {
    if ((e.ctrlKey || e.metaKey) && e.key === "Enter") {
      e.preventDefault();
      if (!error && value.trim() !== "") onsubmit?.();
    } else if (e.key === "Tab" && !e.shiftKey && !readonly) {
      // Indent instead of leaving the editor (Esc then Tab still moves focus).
      e.preventDefault();
      const t = e.currentTarget as HTMLTextAreaElement;
      const { selectionStart: s, selectionEnd: end } = t;
      value = `${value.slice(0, s)}  ${value.slice(end)}`;
      queueMicrotask(() => t.setSelectionRange(s + 2, s + 2));
    } else if (e.key === "Escape") {
      (e.currentTarget as HTMLTextAreaElement).blur();
    }
  }
</script>

<div class="editor">
  <textarea
    aria-label={label}
    aria-invalid={error ? "true" : "false"}
    spellcheck="false"
    autocomplete="off"
    {rows}
    {readonly}
    bind:value
    {onkeydown}
  ></textarea>
  {#if error}
    <div class="status error-text" role="alert">Invalid JSON: {error}</div>
  {/if}
</div>

<style>
  .editor {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  textarea {
    width: 100%;
    line-height: 1.5;
    tab-size: 2;
  }
  textarea[aria-invalid="true"] {
    border-color: var(--err);
  }
  .status {
    font-size: 12.5px;
  }
</style>

<script lang="ts">
  import type { Json, JsonObject } from "../lib/api.js";
  import type { TsField } from "../lib/tsType.js";
  import FormField from "./FormField.svelte";

  /**
   * A document as a form: one field per field of the table's type (in its order, with its doc comments), then the
   * fields the type doesn't list. `_id` and `_creationTime` are kept as they are. Without a type (a table not
   * defined in code), the widgets follow the values.
   */
  let {
    value,
    fields,
    locked = () => false,
    onchange,
    onsubmit,
  }: {
    value: JsonObject;
    /** The fields of the table's type, or `null` when it has none. */
    fields: TsField[] | null;
    /** Fields that can't be edited (a user's credentials, masked secrets). */
    locked?: (field: string) => boolean;
    onchange: (value: JsonObject) => void;
    /** Ctrl/Cmd+Enter. */
    onsubmit?: () => void;
  } = $props();

  const SYSTEM = new Set(["_id", "_creationTime"]);
  const extras = $derived(Object.keys(value).filter((k) => !SYSTEM.has(k) && !(fields ?? []).some((f) => f.name === k)));

  function set(key: string, v: Json | undefined) {
    const next: JsonObject = { ...value };
    if (v === undefined) delete next[key];
    else next[key] = v;
    onchange(next);
  }

  function onkeydown(e: KeyboardEvent) {
    if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
      e.preventDefault();
      onsubmit?.();
    }
  }
</script>

<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
<div class="form" role="group" aria-label="Document fields" {onkeydown}>
  {#each fields ?? [] as f (f.name)}
    <FormField
      name={f.name}
      path={f.name}
      type={f.type}
      value={value[f.name]}
      optional={f.optional}
      doc={f.doc}
      disabled={locked(f.name)}
      onchange={(v) => set(f.name, v)}
    />
  {/each}
  {#each extras as k (k)}
    <FormField name={k} path={k} type={null} value={value[k]} extra={fields !== null} disabled={locked(k)} onchange={(v) => set(k, v)} />
  {/each}
  {#if (fields ?? []).length === 0 && extras.length === 0}
    <p class="faint empty">No fields.</p>
  {/if}
</div>

<style>
  .form {
    display: flex;
    flex-direction: column;
    gap: 12px;
  }
  .empty {
    margin: 0;
    font-size: 13px;
  }
</style>

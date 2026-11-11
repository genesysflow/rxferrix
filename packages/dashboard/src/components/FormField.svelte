<script lang="ts">
  import type { Json, JsonObject } from "../lib/api.js";
  import { parseJson } from "../lib/format.js";
  import { defaultValue, fieldKind, kindOfValue, typeText, type TsType } from "../lib/tsType.js";
  import FormField from "./FormField.svelte";

  /**
   * One field of the document form: a widget for its type (text, number, checkbox, select, nested fields, or JSON
   * for the rest), with a null toggle for nullable fields. `onchange(undefined)` removes the field (optional ones).
   */
  let {
    name,
    path,
    type,
    value,
    optional = false,
    doc,
    disabled = false,
    extra = false,
    onchange,
  }: {
    name: string;
    /** Unique within the form: the element ids. */
    path: string;
    /** The field's type; `null` when unknown (the widget then follows the value). */
    type: TsType | null;
    value: Json | undefined;
    optional?: boolean;
    doc?: string;
    disabled?: boolean;
    /** A field the type doesn't list. */
    extra?: boolean;
    onchange: (value: Json | undefined) => void;
  } = $props();

  const id = $derived(`field-${path}`);
  const shape = $derived(type ? fieldKind(type) : { kind: kindOfValue(value), nullable: value === null });
  const kind = $derived(shape.kind);
  const isNull = $derived(value === null);
  const missing = $derived(value === undefined);
  const hint = $derived(type ? typeText(type) : extra ? "not in the type" : "");
  /** A value of the wrong shape for its type (old data): edited as JSON so nothing is lost. */
  const mismatch = $derived(
    !isNull &&
      !missing &&
      ((kind.kind === "string" || kind.kind === "id") && typeof value !== "string"
        ? true
        : kind.kind === "number" && typeof value !== "number"
          ? true
          : kind.kind === "boolean" && typeof value !== "boolean"
            ? true
            : kind.kind === "object" && (typeof value !== "object" || value === null || Array.isArray(value))),
  );
  const widget = $derived(mismatch ? "json" : kind.kind);

  // Numbers and JSON keep the text being typed while the field has focus: "-", "1e" or half-written JSON aren't
  // values yet, and "1." stays as typed (its value, 1, is already set).
  let draft = $state("");
  let invalid = $state(false);
  let focused = $state(false);
  $effect(() => {
    const text = widget === "number" ? (typeof value === "number" ? String(value) : "") : JSON.stringify(value ?? null, null, 2);
    if (!focused) {
      draft = text;
      invalid = false;
    }
  });

  function blank(): Json {
    if (type) {
      const parts = type.kind === "union" ? type.of.filter((p) => p.kind !== "null") : [type];
      if (parts[0]) return defaultValue(parts[0]);
    }
    return "";
  }

  function onNumber(text: string) {
    draft = text;
    const n = Number(text);
    invalid = text.trim() === "" || !Number.isFinite(n);
    if (!invalid) onchange(n);
  }

  function onJson(text: string) {
    draft = text;
    const parsed = parseJson(text);
    invalid = !parsed.ok;
    if (parsed.ok) onchange(parsed.value);
  }

  function setChild(key: string, v: Json | undefined) {
    const next: JsonObject = { ...(value as JsonObject) };
    if (v === undefined) delete next[key];
    else next[key] = v;
    onchange(next);
  }

  const childFields = $derived(kind.kind === "object" ? kind.fields : []);
  /** Nested fields are shown for an object value only (not while the field is null or missing). */
  const isObject = $derived(typeof value === "object" && value !== null && !Array.isArray(value));
  const childExtras = $derived(
    widget === "object" && isObject ? Object.keys(value as JsonObject).filter((k) => !childFields.some((f) => f.name === k)) : [],
  );
  const rows = $derived(Math.min(8, Math.max(2, draft.split("\n").length)));
</script>

<div class="field" class:nested={widget === "object" && isObject}>
  <div class="label">
    {#if widget === "boolean" && !isNull && !missing}
      <span class="name mono">{name}</span>
    {:else}
      <label class="name mono" for={id}>{name}</label>
    {/if}
    {#if hint}<span class="hint" title={hint}>{hint}</span>{/if}
  </div>
  <div class="control">
    <div class="line">
      {#if missing}
        <span class="placeholder">{optional ? "not set" : "missing"}</span>
        {#if !disabled}<button class="small" type="button" onclick={() => onchange(blank())}>Add</button>{/if}
      {:else if isNull}
        <input {id} class="null-box" value="" placeholder="no value" disabled />
      {:else if widget === "string"}
        {#if typeof value === "string" && value.includes("\n")}
          <textarea {id} rows={Math.min(8, value.split("\n").length + 1)} {disabled} value={value as string} oninput={(e) => onchange(e.currentTarget.value)}
          ></textarea>
        {:else}
          <input {id} type="text" {disabled} value={value as string} oninput={(e) => onchange(e.currentTarget.value)} />
        {/if}
      {:else if widget === "id"}
        <input
          {id}
          class="mono"
          type="text"
          spellcheck="false"
          placeholder={kind.kind === "id" ? `Id<"${kind.table}">` : ""}
          {disabled}
          value={value as string}
          oninput={(e) => onchange(e.currentTarget.value)}
        />
      {:else if widget === "number"}
        <input
          {id}
          class="mono num"
          type="text"
          inputmode="decimal"
          {disabled}
          value={draft}
          aria-invalid={invalid}
          onfocus={() => (focused = true)}
          onblur={() => (focused = false)}
          oninput={(e) => onNumber(e.currentTarget.value)}
        />
      {:else if widget === "boolean"}
        <label class="check">
          <input {id} type="checkbox" {disabled} checked={value === true} onchange={(e) => onchange(e.currentTarget.checked)} aria-label={name} />
          <span class="mono">{value ? "true" : "false"}</span>
        </label>
      {:else if widget === "enum" && kind.kind === "enum"}
        {@const options = kind.options}
        <select {id} {disabled} value={String(options.findIndex((o) => o === value))} onchange={(e) => onchange(options[Number(e.currentTarget.value)]!)}>
          {#if !options.includes(value as string | number | boolean)}<option value="-1" disabled>{JSON.stringify(value)}</option>{/if}
          {#each options as o, i (i)}<option value={String(i)}>{typeof o === "string" ? o : JSON.stringify(o)}</option>{/each}
        </select>
      {:else if widget === "object"}
        <span class="placeholder">{childFields.length + childExtras.length} field{childFields.length + childExtras.length === 1 ? "" : "s"}</span>
      {:else}
        <textarea
          {id}
          class="mono json"
          {rows}
          spellcheck="false"
          {disabled}
          value={draft}
          aria-invalid={invalid}
          onfocus={() => (focused = true)}
          onblur={() => (focused = false)}
          oninput={(e) => onJson(e.currentTarget.value)}
        ></textarea>
      {/if}
      {#if !disabled && !missing && (shape.nullable || isNull)}
        <button
          class="small toggle"
          type="button"
          aria-pressed={isNull}
          title={isNull ? `Give ${name} a value` : `Set ${name} to null`}
          aria-label={isNull ? `Give ${name} a value` : `Set ${name} to null`}
          onclick={() => onchange(isNull ? blank() : null)}>null</button
        >
      {/if}
      {#if !disabled && !missing && (optional || extra)}
        <button class="small ghost remove" type="button" title={`Remove ${name}`} aria-label={`Remove ${name}`} onclick={() => onchange(undefined)}>✕</button>
      {/if}
    </div>
    {#if invalid}<div class="error-text msg">{widget === "number" ? "Not a number" : "Invalid JSON"}</div>{/if}
    {#if mismatch}<div class="faint msg">Stored value doesn't match the type; edit it as JSON.</div>{/if}
    {#if doc}<div class="doc">{doc}</div>{/if}
  </div>
  {#if widget === "object" && isObject}
    <div class="children">
      {#each childFields as f (f.name)}
        <FormField
          name={f.name}
          path={`${path}.${f.name}`}
          type={f.type}
          value={(value as JsonObject)[f.name]}
          optional={f.optional}
          doc={f.doc}
          {disabled}
          onchange={(v) => setChild(f.name, v)}
        />
      {/each}
      {#each childExtras as k (k)}
        <FormField name={k} path={`${path}.${k}`} type={null} value={(value as JsonObject)[k]} extra {disabled} onchange={(v) => setChild(k, v)} />
      {/each}
    </div>
  {/if}
</div>

<style>
  .field {
    display: grid;
    grid-template-columns: minmax(96px, 30%) 1fr;
    gap: 4px 12px;
    align-items: start;
  }
  .label {
    display: flex;
    flex-direction: column;
    min-width: 0;
    padding-top: 6px;
  }
  .name {
    font-size: 12.5px;
    color: var(--text);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .hint {
    font-family: var(--mono);
    font-variant-ligatures: none;
    font-size: 11px;
    color: var(--faint);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .control {
    display: flex;
    flex-direction: column;
    gap: 4px;
    min-width: 0;
  }
  .line {
    display: flex;
    align-items: center;
    gap: 6px;
    min-height: 32px;
  }
  .line > input:not([type="checkbox"]),
  .line > textarea,
  .line > select {
    flex: 1;
    min-width: 0;
    width: 100%;
  }
  .line > select {
    flex: 0 1 auto;
  }
  .num {
    max-width: 200px;
  }
  textarea {
    font-family: var(--sans);
    font-size: 14px;
    line-height: 1.45;
  }
  textarea.json {
    font-family: var(--mono);
    font-variant-ligatures: none;
    font-size: 12.5px;
  }
  .null-box::placeholder {
    font-style: italic;
  }
  [aria-invalid="true"] {
    border-color: var(--err);
  }
  .check {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    color: var(--muted);
    font-size: 12.5px;
  }
  .placeholder {
    color: var(--faint);
    font-size: 13px;
    font-style: italic;
  }
  .toggle {
    flex: none;
    font-family: var(--mono);
    font-variant-ligatures: none;
    font-size: 11.5px;
    color: var(--muted);
  }
  .toggle[aria-pressed="true"] {
    background: var(--accent-soft);
    border-color: transparent;
    color: var(--accent-text);
  }
  .remove {
    flex: none;
    width: 28px;
    padding: 0;
    justify-content: center;
    color: var(--muted);
  }
  .doc {
    font-size: 12px;
    color: var(--muted);
    line-height: 1.45;
  }
  .msg {
    font-size: 12px;
  }
  /* A nested object: its fields indented under it, behind a guide line. */
  .children {
    grid-column: 1 / -1;
    display: flex;
    flex-direction: column;
    gap: 10px;
    margin: 2px 0 4px 6px;
    padding-left: 12px;
    border-left: 2px solid var(--border);
  }
  /* Phones: the name above its widget. */
  @media (max-width: 760px) {
    .field {
      grid-template-columns: 1fr;
    }
    .label {
      flex-direction: row;
      align-items: baseline;
      gap: 8px;
      padding-top: 0;
    }
  }
</style>

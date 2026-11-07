import { createRawSnippet, flushSync, mount, unmount } from "svelte";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Json, TableSummary } from "../src/lib/api.js";
import { DASHBOARD_CONTEXT, type DashboardContext } from "../src/lib/context.js";
import DocumentForm from "../src/components/DocumentForm.svelte";
import JsonEditor from "../src/components/JsonEditor.svelte";
import { parseTsType, type TsField } from "../src/lib/tsType.js";
import JsonValue from "../src/components/JsonValue.svelte";
import Modal from "../src/components/Modal.svelte";

let target: HTMLElement;
let mounted: ReturnType<typeof mount> | null = null;

beforeEach(() => {
  target = document.createElement("div");
  document.body.appendChild(target);
});

afterEach(() => {
  if (mounted) unmount(mounted);
  mounted = null;
  target.remove();
});

function type(textarea: HTMLTextAreaElement, value: string) {
  textarea.value = value;
  textarea.dispatchEvent(new Event("input", { bubbles: true }));
  flushSync();
}

function ctrlEnter(el: HTMLElement) {
  el.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", ctrlKey: true, bubbles: true }));
  flushSync();
}

describe("JsonEditor", () => {
  it("validates as you type and submits only valid JSON with Ctrl+Enter", () => {
    const onsubmit = vi.fn();
    mounted = mount(JsonEditor, { target, props: { label: "Doc", value: "{}", onsubmit } });
    flushSync();
    const textarea = target.querySelector("textarea")!;
    expect(textarea.getAttribute("aria-label")).toBe("Doc");
    expect(target.querySelector("[role=alert]")).toBeNull();

    type(textarea, '{ "a": ');
    expect(target.querySelector("[role=alert]")?.textContent).toMatch(/^Invalid JSON/);
    expect(textarea.getAttribute("aria-invalid")).toBe("true");
    ctrlEnter(textarea);
    expect(onsubmit).not.toHaveBeenCalled();

    type(textarea, '{ "a": 1 }');
    expect(target.querySelector("[role=alert]")).toBeNull();
    ctrlEnter(textarea);
    expect(onsubmit).toHaveBeenCalledTimes(1);
  });
});

describe("JsonValue", () => {
  const todos: TableSummary = {
    name: "todos",
    number: 8,
    system: false,
    defined: true,
    documents: 1,
    indexes: [],
    searchIndexes: [],
    ts: null,
    secretFields: [],
    access: "readWrite",
  };
  const files: TableSummary = { ...todos, name: "_storage", number: 9, system: true, access: "readOnly" };
  const ctx: DashboardContext = {
    auth: {} as DashboardContext["auth"],
    tables: [todos, files],
    tableByNumber: (n) => (n === 8 ? todos : n === 9 ? files : undefined),
    toast: () => {},
  };
  const render = (value: Json | undefined, field = "x") => {
    if (mounted) unmount(mounted);
    mounted = mount(JsonValue, { target, props: { value, field }, context: new Map([[DASHBOARD_CONTEXT, ctx]]) });
    flushSync();
    return target;
  };

  it("links ids of known tables to their document", () => {
    const a = render("0000022dbsg2b0typca0183asm").querySelector("a")!;
    expect(a.getAttribute("href")).toBe("/_/data/todos?doc=0000022dbsg2b0typca0183asm");
    // An id-shaped string of an unknown table stays text.
    expect(render("00000k2dbsg2b0typca0183asm").querySelector("a")).toBeNull();
  });

  it("links stored file ids to the Files page", () => {
    const a = render("0000028dbsg2b0typca0183asm").querySelector("a")!;
    expect(a.getAttribute("href")).toBe("/_/files?file=0000028dbsg2b0typca0183asm");
  });

  it("renders scalars, objects and creation times compactly", () => {
    expect(render(null).textContent).toBe("null");
    expect(render({ a: [1] }).textContent).toBe('{"a":[1]}');
    expect(render(undefined).textContent).toBe("—");
    expect(render(1791554695097.8223, "_creationTime").textContent).toMatch(/^\d{4}-\d\d-\d\d \d\d:\d\d:\d\d$/);
  });
});

describe("Modal", () => {
  const body = createRawSnippet(() => ({ render: () => "<p class='inside'>Recovery codes</p>" }));
  function open(dismissible = true) {
    const target = document.createElement("div");
    document.body.append(target);
    const onclose = vi.fn();
    const component = mount(Modal, { target, props: { title: "Codes", onclose, dismissible, children: body } });
    flushSync();
    const dialog = target.querySelector("dialog")!;
    const press = (down: Element, up: Element) => {
      down.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
      up.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    };
    return { dialog, inside: target.querySelector(".inside")!, onclose, press, close: () => unmount(component) };
  }

  it("closes on a click on the backdrop, not on a drag that only ends there", () => {
    const m = open();
    m.press(m.inside, m.dialog); // selecting text inside, released over the backdrop
    expect(m.onclose).not.toHaveBeenCalled();
    m.press(m.dialog, m.dialog);
    expect(m.onclose).toHaveBeenCalledTimes(1);
    m.close();
  });

  it("ignores the backdrop and Escape when not dismissible", () => {
    const m = open(false);
    m.press(m.dialog, m.dialog);
    const cancel = new Event("cancel", { cancelable: true });
    m.dialog.dispatchEvent(cancel);
    expect(cancel.defaultPrevented).toBe(true);
    expect(m.onclose).not.toHaveBeenCalled();
    m.close();
  });
});


describe("DocumentForm", () => {
  const todo = parseTsType(
    `{ text: string, done: boolean, image: Id<"_storage"> | null, completedAt: number | null, priority: "low" | "high", meta: { tags: Array<string> }, }`,
  );
  const fields = (todo as { fields: TsField[] }).fields;
  const doc = { _id: "x1", _creationTime: 1, text: "milk", done: false, image: null, completedAt: 5, priority: "low", meta: { tags: ["a"] }, old: 1 };

  function render(value: Record<string, Json> = doc, locked = (_: string) => false) {
    const changes: Record<string, Json>[] = [];
    const onsubmit = vi.fn();
    mounted = mount(DocumentForm, { target, props: { value, fields, locked, onchange: (v: Record<string, Json>) => changes.push(v), onsubmit } });
    flushSync();
    const field = (id: string) => target.querySelector<HTMLInputElement>(`#field-${CSS.escape(id)}`)!;
    return { changes, onsubmit, field };
  }

  it("edits each field with a widget for its type, keeping _id and _creationTime", () => {
    const { changes, field } = render();
    field("text").value = "oat milk";
    field("text").dispatchEvent(new Event("input", { bubbles: true }));
    expect(changes.at(-1)).toEqual({ ...doc, text: "oat milk" });

    field("done").click();
    expect(changes.at(-1)!.done).toBe(true);

    const priority = target.querySelector<HTMLSelectElement>("#field-priority")!;
    priority.value = "1";
    priority.dispatchEvent(new Event("change", { bubbles: true }));
    expect(changes.at(-1)!.priority).toBe("high");

    // A nested object's fields, the array in it as JSON.
    type(target.querySelector<HTMLTextAreaElement>(`#field-${CSS.escape("meta.tags")}`)!, '["a", "b"]');
    expect(changes.at(-1)!.meta).toEqual({ tags: ["a", "b"] });
    expect(changes.at(-1)!._id).toBe("x1");
  });

  it("keeps a half-typed number as text, and toggles null", () => {
    const { changes, field } = render();
    const n = field("completedAt");
    n.value = "-";
    n.dispatchEvent(new Event("input", { bubbles: true }));
    flushSync();
    expect(changes).toHaveLength(0);
    expect(target.textContent).toContain("Not a number");
    n.value = "-12.5";
    n.dispatchEvent(new Event("input", { bubbles: true }));
    expect(changes.at(-1)!.completedAt).toBe(-12.5);

    target.querySelector<HTMLButtonElement>('[aria-label="Set completedAt to null"]')!.click();
    expect(changes.at(-1)!.completedAt).toBeNull();
    target.querySelector<HTMLButtonElement>('[aria-label="Give image a value"]')!.click();
    expect(changes.at(-1)!.image).toBe("");
  });

  it("lists fields the type doesn't have, removable, and submits with Ctrl+Enter", () => {
    const { changes, field, onsubmit } = render();
    expect(target.textContent).toContain("not in the type");
    target.querySelector<HTMLButtonElement>('[aria-label="Remove old"]')!.click();
    expect("old" in changes.at(-1)!).toBe(false);
    ctrlEnter(field("text"));
    expect(onsubmit).toHaveBeenCalledOnce();
  });

  it("shows a null or missing nested object without its fields", () => {
    const { changes } = render({ ...doc, meta: null });
    expect(target.querySelector(`#field-${CSS.escape("meta.tags")}`)).toBeNull();
    target.querySelector<HTMLButtonElement>('[aria-label="Give meta a value"]')!.click();
    expect(changes.at(-1)!.meta).toEqual({ tags: [] });
    unmount(mounted!);
    mounted = null;
    const { meta: _, ...without } = doc;
    render(without);
    expect(target.textContent).toContain("missing");
  });

  it("disables locked fields", () => {
    const { field } = render(doc, (f) => f !== "text");
    expect(field("text").disabled).toBe(false);
    expect(field("done").disabled).toBe(true);
    expect(target.querySelector('[aria-label="Give image a value"]')).toBeNull();
  });
});

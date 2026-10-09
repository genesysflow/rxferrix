import { flushSync, mount, unmount } from "svelte";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Json, TableSummary } from "../src/lib/api.js";
import { DASHBOARD_CONTEXT, type DashboardContext } from "../src/lib/context.js";
import JsonEditor from "../src/components/JsonEditor.svelte";
import JsonValue from "../src/components/JsonValue.svelte";

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
    ts: null,
    secretFields: [],
    access: "readWrite",
  };
  const ctx: DashboardContext = {
    auth: {} as DashboardContext["auth"],
    tables: [todos],
    tableByNumber: (n) => (n === 8 ? todos : undefined),
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

  it("renders scalars, objects and creation times compactly", () => {
    expect(render(null).textContent).toBe("null");
    expect(render({ a: [1] }).textContent).toBe('{"a":[1]}');
    expect(render(undefined).textContent).toBe("—");
    expect(render(1791554695097.8223, "_creationTime").textContent).toMatch(/^\d{4}-\d\d-\d\d \d\d:\d\d:\d\d$/);
  });
});

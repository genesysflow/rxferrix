import { flushSync, mount, unmount } from "svelte";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { RxFerrixClient } from "@rxferrix/client";
import { FakeServer, flushMicrotasks } from "@rxferrix/client/testing";
import { useAction, useMutation, useQuery } from "../src/lib/index.js";
import { api } from "./api.js";
import QueryView from "./QueryView.svelte";
import Root from "./Root.svelte";
import TodoList from "./TodoList.svelte";

let server: FakeServer;
let client: RxFerrixClient;
let target: HTMLElement;
let mounted: ReturnType<typeof mount> | null = null;

beforeEach(async () => {
  server = new FakeServer();
  client = new RxFerrixClient("http://localhost:3210", {
    webSocketConstructor: server.WebSocket,
    unsavedChangesWarning: false,
  });
  server.open();
  await flushMicrotasks();
  server.drain();
  target = document.createElement("div");
  document.body.appendChild(target);
});

afterEach(() => {
  if (mounted) unmount(mounted);
  mounted = null;
  target.remove();
  client.close();
});

/** Lets Svelte's microtask-scheduled work and the client's batched flush run, then flushes DOM updates. */
async function settle() {
  await flushMicrotasks();
  flushSync();
  await flushMicrotasks();
  flushSync();
}

function text() {
  return target.textContent?.replace(/\s+/g, " ").trim();
}

describe("useQuery", () => {
  it("shows loading, then the value, then updates", async () => {
    const props = $state({ id: "a" as string | null });
    mounted = mount(Root, { target, props: { client, child: QueryView, props } });
    flushSync();
    expect(text()).toBe("loading");
    await settle();
    expect(server.drain()).toEqual([
      {
        type: "ModifyQuerySet",
        baseVersion: 0,
        newVersion: 1,
        modifications: [{ type: "Add", queryId: 0, udfPath: "todos:get", args: { id: "a" } }],
      },
    ]);

    server.transition({ querySet: 1, ts: 1 }, [{ type: "QueryUpdated", queryId: 0, value: { _id: "a", text: "A" } }]);
    flushSync();
    expect(text()).toBe("todo: A");

    server.transition({ ts: 2 }, [{ type: "QueryUpdated", queryId: 0, value: { _id: "a", text: "A2" } }]);
    flushSync();
    expect(text()).toBe("todo: A2");

    server.transition({ ts: 3 }, [
      { type: "QueryFailed", queryId: 0, errorMessage: "gone", errorData: { code: "NOT_FOUND", data: null } },
    ]);
    flushSync();
    expect(text()).toBe("error: gone (NOT_FOUND)");
  });

  it("switches the subscription when the args thunk changes, and skips", async () => {
    const props = $state({ id: "a" as string | null });
    mounted = mount(Root, { target, props: { client, child: QueryView, props } });
    await settle();
    server.drain();
    server.transition({ querySet: 1, ts: 1 }, [{ type: "QueryUpdated", queryId: 0, value: { _id: "a", text: "A" } }]);
    flushSync();
    expect(text()).toBe("todo: A");

    props.id = "b";
    await settle();
    expect(text()).toBe("loading");
    expect(server.drain()).toEqual([
      {
        type: "ModifyQuerySet",
        baseVersion: 1,
        newVersion: 2,
        modifications: [
          { type: "Add", queryId: 1, udfPath: "todos:get", args: { id: "b" } },
          { type: "Remove", queryId: 0 },
        ],
      },
    ]);
    server.transition({ querySet: 2, ts: 1 }, [
      { type: "QueryRemoved", queryId: 0 },
      { type: "QueryUpdated", queryId: 1, value: { _id: "b", text: "B" } },
    ]);
    flushSync();
    expect(text()).toBe("todo: B");

    // Same args again (a new object with equal content): no resubscription.
    props.id = "b";
    await settle();
    expect(server.drain()).toEqual([]);

    props.id = null;
    await settle();
    expect(text()).toBe("skipped");
    expect(server.drain()).toEqual([
      { type: "ModifyQuerySet", baseVersion: 2, newVersion: 3, modifications: [{ type: "Remove", queryId: 1 }] },
    ]);
  });

  it("unsubscribes when the component is destroyed", async () => {
    const props = $state({ id: "a" as string | null });
    mounted = mount(Root, { target, props: { client, child: QueryView, props } });
    await settle();
    server.drain();
    unmount(mounted);
    mounted = null;
    await settle();
    expect(server.drain()).toEqual([
      { type: "ModifyQuerySet", baseVersion: 1, newVersion: 2, modifications: [{ type: "Remove", queryId: 0 }] },
    ]);
  });

  it("two components watching the same query share one subscription", async () => {
    const props = $state({ id: "a" as string | null });
    mounted = mount(Root, { target, props: { client, child: QueryView, props } });
    const target2 = document.createElement("div");
    const second = mount(Root, { target: target2, props: { client, child: QueryView, props } });
    await settle();
    expect(server.sentOfType("ModifyQuerySet")).toHaveLength(1);
    server.drain();
    unmount(second);
    await settle();
    expect(server.drain()).toEqual([]);
  });

  it("works outside components with an explicit client inside $effect.root", async () => {
    let id = $state("a");
    const values: unknown[] = [];
    const stop = $effect.root(() => {
      const q = useQuery(api.todos.get, () => ({ id }), { client });
      $effect(() => {
        values.push(q.data);
      });
    });
    await settle();
    expect(server.sentOfType("ModifyQuerySet")).toHaveLength(1);
    server.transition({ querySet: 1, ts: 1 }, [{ type: "QueryUpdated", queryId: 0, value: { _id: "a", text: "A" } }]);
    flushSync();
    expect(values).toEqual([undefined, { _id: "a", text: "A" }]);
    id = "b";
    await settle();
    stop();
    await settle();
    const last = server.sentOfType("ModifyQuerySet").at(-1);
    expect(last?.modifications).toEqual([{ type: "Remove", queryId: 1 }]);
  });
});

describe("useMutation / useAction", () => {
  it("applies an optimistic update and settles with server data", async () => {
    let result: Promise<unknown> | null = null;
    mounted = mount(Root, {
      target,
      props: { client, child: TodoList, props: { onResult: (p: Promise<unknown>) => (result = p) } },
    });
    await settle();
    server.drain();
    server.transition({ querySet: 1, ts: 1 }, [{ type: "QueryUpdated", queryId: 0, value: [{ _id: "1", text: "old" }] }]);
    flushSync();
    expect([...target.querySelectorAll("li")].map((li) => li.textContent)).toEqual(["old"]);

    target.querySelector("button")!.click();
    flushSync();
    expect([...target.querySelectorAll("li")].map((li) => li.textContent)).toEqual(["old", "new"]);
    expect(server.sentOfType("Mutation")).toEqual([
      { type: "Mutation", requestId: 0, udfPath: "todos:add", args: { text: "new" } },
    ]);

    server.push({ type: "MutationResponse", requestId: 0, success: true, result: "2", ts: 2 });
    server.transition({ ts: 2 }, [
      {
        type: "QueryUpdated",
        queryId: 0,
        value: [
          { _id: "1", text: "old" },
          { _id: "2", text: "new" },
        ],
      },
    ]);
    flushSync();
    expect([...target.querySelectorAll("li")].map((li) => li.textContent)).toEqual(["old", "new"]);
    await expect(result).resolves.toBe("2");
  });

  it("rolls back on failure", async () => {
    let result: Promise<unknown> | null = null;
    mounted = mount(Root, {
      target,
      props: { client, child: TodoList, props: { onResult: (p: Promise<unknown>) => (result = p) } },
    });
    await settle();
    server.transition({ querySet: 1, ts: 1 }, [{ type: "QueryUpdated", queryId: 0, value: [] }]);
    target.querySelector("button")!.click();
    flushSync();
    expect(target.querySelectorAll("li")).toHaveLength(1);
    server.push({ type: "MutationResponse", requestId: 0, success: false, errorMessage: "no", errorData: { code: "X" } });
    flushSync();
    expect(target.querySelectorAll("li")).toHaveLength(0);
    await expect(result).resolves.toMatchObject({ code: "X", message: "no" });
  });

  it("runs actions with an explicit client", async () => {
    const run = useAction(api.ai.run, { client });
    const add = useMutation(api.todos.add, { client });
    expect(typeof add.withOptimisticUpdate).toBe("function");
    const p = run({ prompt: "hi" });
    expect(server.sentOfType("Action")).toEqual([
      { type: "Action", requestId: 0, udfPath: "ai:run", args: { prompt: "hi" } },
    ]);
    server.push({ type: "ActionResponse", requestId: 0, success: true, result: "ok" });
    await expect(p).resolves.toBe("ok");
  });
});

describe("useConnectionState", () => {
  it("tracks the socket state reactively", async () => {
    const { useConnectionState } = await import("../src/lib/index.js");
    const seen: boolean[] = [];
    const stop = $effect.root(() => {
      const state = useConnectionState({ client });
      $effect(() => {
        seen.push(state.current.isWebSocketConnected);
      });
    });
    flushSync();
    server.close();
    flushSync();
    stop();
    expect(seen).toEqual([true, false]);
  });
});

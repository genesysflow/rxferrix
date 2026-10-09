import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  CONNECTION_LOST,
  FerrixClient,
  FerrixError,
  makeFunctionReference,
  type ConnectionState,
  type Id,
} from "../src/index.js";
import type { ClientMessage } from "../src/protocol.js";
import { FakeServer, flushMicrotasks } from "../src/testing.js";

type Todo = { _id: Id<"todos">; text: string };

const api = {
  todos: {
    list: makeFunctionReference<"query", { done?: boolean | null }, Todo[]>("query", "todos:list"),
    get: makeFunctionReference<"query", { id: string }, Todo | null>("query", "todos:get"),
    count: makeFunctionReference<"query", Record<string, never>, number>("query", "todos:count"),
    add: makeFunctionReference<"mutation", { text: string }, Id<"todos">>("mutation", "todos:add"),
  },
  ai: {
    summarize: makeFunctionReference<"action", { text: string }, string>("action", "ai:summarize"),
  },
};

let server: FakeServer;
let client: FerrixClient;

function newClient(): FerrixClient {
  return new FerrixClient("http://localhost:3210", {
    webSocketConstructor: server.WebSocket,
    unsavedChangesWarning: false,
  });
}

/** Opens the socket and returns the messages of the handshake (Connect, ...). */
async function openAndDrain(): Promise<ClientMessage[]> {
  server.open();
  await flushMicrotasks();
  return server.drain();
}

beforeEach(() => {
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "setInterval", "clearInterval", "Date"] });
  server = new FakeServer();
  client = newClient();
});

afterEach(() => {
  client.close();
  vi.useRealTimers();
  vi.restoreAllMocks();
});

describe("connection", () => {
  it("derives the sync URL from the http base URL", () => {
    expect(server.socket.url).toBe("ws://localhost:3210/api/sync");
    const c = new FerrixClient("https://example.com/base/", {
      webSocketConstructor: server.WebSocket,
      unsavedChangesWarning: false,
    });
    expect(server.socket.url).toBe("wss://example.com/base/api/sync");
    c.close();
  });

  it("sends Connect first and then one ModifyQuerySet for queries watched before the socket opened", async () => {
    const watch = client.watchQuery(api.todos.list, { done: false });
    watch.onUpdate(() => {});
    await flushMicrotasks(); // socket not open yet: nothing can be sent
    expect(server.sent).toEqual([]);

    const sent = await openAndDrain();
    expect(sent).toEqual([
      {
        type: "Connect",
        sessionId: client.sessionId,
        connectionCount: 0,
        lastCloseReason: null,
        clientVersion: "0.1.0",
      },
      {
        type: "ModifyQuerySet",
        baseVersion: 0,
        newVersion: 1,
        modifications: [{ type: "Add", queryId: 0, udfPath: "todos:list", args: { done: false } }],
      },
    ]);
    expect(client.connectionState().isWebSocketConnected).toBe(true);
  });

  it("sends ModifyQuerySet for queries watched after the socket opened", async () => {
    await openAndDrain();
    client.watchQuery(api.todos.count).onUpdate(() => {});
    expect(server.sent).toEqual([]); // batched to the end of the tick
    await flushMicrotasks();
    expect(server.drain()).toEqual([
      {
        type: "ModifyQuerySet",
        baseVersion: 0,
        newVersion: 1,
        modifications: [{ type: "Add", queryId: 0, udfPath: "todos:count", args: {} }],
      },
    ]);
  });

  it("answers Ping with Pong", async () => {
    await openAndDrain();
    server.push({ type: "Ping" });
    expect(server.drain()).toEqual([{ type: "Pong" }]);
  });

  it("reconnects after 30s of server silence", async () => {
    await openAndDrain();
    vi.advanceTimersByTime(29_000);
    server.push({ type: "Ping" }); // resets the watchdog
    vi.advanceTimersByTime(29_000);
    expect(server.sockets).toHaveLength(1);
    vi.advanceTimersByTime(2_000);
    expect(server.sockets[0]!.closedByClient).toBe(true);
    vi.advanceTimersByTime(20_000); // backoff
    expect(server.sockets).toHaveLength(2);
    const sent = await openAndDrain();
    expect(sent[0]).toMatchObject({ type: "Connect", connectionCount: 1, lastCloseReason: "ServerInactivity" });
  });

  it("reconnects with backoff after the server closes the socket", async () => {
    await openAndDrain();
    server.close();
    expect(client.connectionState().isWebSocketConnected).toBe(false);
    expect(server.sockets).toHaveLength(1);
    vi.advanceTimersByTime(20_000);
    expect(server.sockets).toHaveLength(2);
  });

  it("notifies connection state subscribers", async () => {
    const states: ConnectionState[] = [];
    client.subscribeToConnectionState((s) => states.push(s));
    await openAndDrain();
    expect(states.at(-1)?.isWebSocketConnected).toBe(true);
    expect(client.connectionState()).toBe(states.at(-1));
    void client.mutation(api.todos.add, { text: "x" }).catch(() => {});
    expect(client.connectionState().hasInflightRequests).toBe(true);
    expect(client.connectionState().inflightMutations).toBe(1);
  });
});

describe("query set", () => {
  it("dedups identical queries (canonical args) and removes the query when the last watcher leaves", async () => {
    await openAndDrain();
    const a = client.watchQuery(api.todos.get, { id: "1" });
    const b = client.watchQuery(api.todos.get, { id: "1" });
    const unsubA = a.onUpdate(() => {});
    const unsubB = b.onUpdate(() => {});
    const unsubC = client
      .watchQuery(makeFunctionReference<"query", { x: number; y: { b: 1; a: 2 } }, unknown>("query", "m:q"), {
        y: { b: 1, a: 2 },
        x: 1,
      })
      .onUpdate(() => {});
    const unsubD = client
      .watchQuery(makeFunctionReference<"query", { x: number; y: { a: 2; b: 1 } }, unknown>("query", "m:q"), {
        x: 1,
        y: { a: 2, b: 1 },
      })
      .onUpdate(() => {});
    await flushMicrotasks();
    const [mqs] = server.drain();
    expect(mqs).toMatchObject({ type: "ModifyQuerySet", baseVersion: 0, newVersion: 1 });
    expect((mqs as Extract<ClientMessage, { type: "ModifyQuerySet" }>).modifications).toEqual([
      { type: "Add", queryId: 0, udfPath: "todos:get", args: { id: "1" } },
      { type: "Add", queryId: 1, udfPath: "m:q", args: { y: { b: 1, a: 2 }, x: 1 } },
    ]);

    unsubA();
    unsubC();
    await flushMicrotasks();
    expect(server.drain()).toEqual([]); // still watched by b and d

    unsubB();
    unsubB(); // idempotent
    await flushMicrotasks();
    expect(server.drain()).toEqual([
      { type: "ModifyQuerySet", baseVersion: 1, newVersion: 2, modifications: [{ type: "Remove", queryId: 0 }] },
    ]);

    // Re-watching after removal allocates a fresh queryId (ids are never reused).
    client.watchQuery(api.todos.get, { id: "1" }).onUpdate(() => {});
    unsubD();
    await flushMicrotasks();
    expect(server.drain()).toEqual([
      {
        type: "ModifyQuerySet",
        baseVersion: 2,
        newVersion: 3,
        modifications: [
          { type: "Add", queryId: 2, udfPath: "todos:get", args: { id: "1" } },
          { type: "Remove", queryId: 1 },
        ],
      },
    ]);
  });

  it("batches all changes made in one tick into a single ModifyQuerySet", async () => {
    await openAndDrain();
    client.watchQuery(api.todos.list, {}).onUpdate(() => {});
    client.watchQuery(api.todos.count).onUpdate(() => {});
    client.watchQuery(api.todos.get, { id: "a" }).onUpdate(() => {});
    await flushMicrotasks();
    const msgs = server.drain();
    expect(msgs).toHaveLength(1);
    expect(msgs[0]).toMatchObject({ type: "ModifyQuerySet", baseVersion: 0, newVersion: 1 });
    expect((msgs[0] as Extract<ClientMessage, { type: "ModifyQuerySet" }>).modifications).toHaveLength(3);
  });

  it("sends nothing when a query is added and removed in the same tick", async () => {
    await openAndDrain();
    const unsub = client.watchQuery(api.todos.list, {}).onUpdate(() => {});
    unsub();
    await flushMicrotasks();
    expect(server.drain()).toEqual([]);
  });

  it("keeps the subscription when a watcher leaves and another arrives in the same tick", async () => {
    await openAndDrain();
    const unsub = client.watchQuery(api.todos.list, {}).onUpdate(() => {});
    await flushMicrotasks();
    server.drain();
    unsub();
    client.watchQuery(api.todos.list, {}).onUpdate(() => {});
    await flushMicrotasks();
    expect(server.drain()).toEqual([]);
  });
});

describe("transitions", () => {
  it("delivers results and applies multi-query transitions atomically", async () => {
    await openAndDrain();
    const list = client.watchQuery(api.todos.list, {});
    const count = client.watchQuery(api.todos.count);
    const seen: { list: unknown; count: unknown }[] = [];
    const onEither = () => seen.push({ list: list.localQueryResult(), count: count.localQueryResult() });
    list.onUpdate(onEither);
    count.onUpdate(onEither);
    await flushMicrotasks();
    expect(list.localQueryState()).toEqual({ status: "loading" });
    expect(list.localQueryResult()).toBeUndefined();

    server.transition({ querySet: 1, ts: 10 }, [
      { type: "QueryUpdated", queryId: 0, value: [], logLines: [] },
      { type: "QueryUpdated", queryId: 1, value: 0, logLines: [] },
    ]);
    // Every callback observes BOTH new values: no tearing.
    expect(seen).toEqual([
      { list: [], count: 0 },
      { list: [], count: 0 },
    ]);

    seen.length = 0;
    const todo = { _id: "t1", text: "a" };
    server.transition({ ts: 11 }, [
      { type: "QueryUpdated", queryId: 0, value: [todo], logLines: [] },
      { type: "QueryUpdated", queryId: 1, value: 1, logLines: [] },
    ]);
    expect(seen).toEqual([
      { list: [todo], count: 1 },
      { list: [todo], count: 1 },
    ]);
  });

  it("does not notify watchers when a query's value is unchanged", async () => {
    await openAndDrain();
    const watch = client.watchQuery(api.todos.count);
    const cb = vi.fn();
    watch.onUpdate(cb);
    await flushMicrotasks();
    server.transition({ querySet: 1, ts: 1 }, [{ type: "QueryUpdated", queryId: 0, value: { n: [1] } }]);
    expect(cb).toHaveBeenCalledTimes(1);
    const first = watch.localQueryState();
    server.transition({ ts: 2 }, [{ type: "QueryUpdated", queryId: 0, value: { n: [1] } }]);
    expect(cb).toHaveBeenCalledTimes(1);
    expect(watch.localQueryState()).toBe(first); // referentially stable snapshot
  });

  it("treats a startVersion mismatch as a protocol error and reconnects", async () => {
    const watch = client.watchQuery(api.todos.count);
    watch.onUpdate(() => {});
    await openAndDrain();
    server.transition({ querySet: 1, ts: 5 }, [{ type: "QueryUpdated", queryId: 0, value: 3 }]);
    expect(watch.localQueryResult()).toBe(3);

    // Server claims to start from a version the client is not at.
    server.push({
      type: "Transition",
      startVersion: { querySet: 7, identity: 0, ts: 5 },
      endVersion: { querySet: 7, identity: 0, ts: 6 },
      modifications: [{ type: "QueryUpdated", queryId: 0, value: 999 }],
    });
    expect(watch.localQueryResult()).toBe(3); // not applied
    expect(server.sockets[0]!.closedByClient).toBe(true);

    vi.advanceTimersByTime(20_000);
    expect(server.sockets).toHaveLength(2);
    const sent = await openAndDrain();
    expect(sent).toEqual([
      expect.objectContaining({ type: "Connect", connectionCount: 1, lastCloseReason: "TransitionVersionMismatch" }),
      {
        type: "ModifyQuerySet",
        baseVersion: 0,
        newVersion: 1,
        modifications: [{ type: "Add", queryId: 0, udfPath: "todos:count", args: {} }],
      },
    ]);
    // Stale value stays visible until the new socket reports.
    expect(watch.localQueryResult()).toBe(3);
    server.transition({ querySet: 1, ts: 9 }, [{ type: "QueryUpdated", queryId: 0, value: 4 }]);
    expect(watch.localQueryResult()).toBe(4);
  });

  it("ignores results for queries removed while the transition was in flight", async () => {
    await openAndDrain();
    const unsub = client.watchQuery(api.todos.count).onUpdate(() => {});
    await flushMicrotasks();
    unsub();
    await flushMicrotasks();
    server.drain();
    server.transition({ querySet: 1, ts: 1 }, [{ type: "QueryUpdated", queryId: 0, value: 1 }]);
    server.transition({ querySet: 2 }, [{ type: "QueryRemoved", queryId: 0 }]);
    expect(client.watchQuery(api.todos.count).localQueryState()).toEqual({ status: "loading" });
  });
});

describe("mutations", () => {
  async function setup() {
    await openAndDrain();
    const watch = client.watchQuery(api.todos.list, {});
    const values: unknown[] = [];
    watch.onUpdate(() => values.push(watch.localQueryResult()));
    await flushMicrotasks();
    server.drain();
    server.transition({ querySet: 1, ts: 3 }, [{ type: "QueryUpdated", queryId: 0, value: [] }]);
    values.length = 0;
    return { watch, values };
  }

  it("resolves only after a transition with ts >= the mutation ts (response first)", async () => {
    await setup();
    const resolved = vi.fn();
    void client.mutation(api.todos.add, { text: "hi" }).then(resolved);
    expect(server.drain()).toEqual([{ type: "Mutation", requestId: 0, udfPath: "todos:add", args: { text: "hi" } }]);

    server.push({ type: "MutationResponse", requestId: 0, success: true, result: "id1", ts: 5, logLines: [] });
    await flushMicrotasks();
    expect(resolved).not.toHaveBeenCalled();

    server.transition({ ts: 4 }); // older ts: still waiting
    await flushMicrotasks();
    expect(resolved).not.toHaveBeenCalled();

    server.transition({ ts: 5 }); // empty transition advancing ts is enough
    await flushMicrotasks();
    expect(resolved).toHaveBeenCalledWith("id1");
    expect(client.connectionState().hasInflightRequests).toBe(false);
  });

  it("resolves immediately when the transition arrived before the response", async () => {
    await setup();
    const resolved = vi.fn();
    void client.mutation(api.todos.add, { text: "hi" }).then(resolved);
    server.transition({ ts: 7 });
    await flushMicrotasks();
    expect(resolved).not.toHaveBeenCalled();
    server.push({ type: "MutationResponse", requestId: 0, success: true, result: "id1", ts: 6, logLines: [] });
    await flushMicrotasks();
    expect(resolved).toHaveBeenCalledWith("id1");
  });

  it("rejects failures immediately with FerrixError carrying errorData", async () => {
    await setup();
    const p = client.mutation(api.todos.add, { text: "" });
    server.push({
      type: "MutationResponse",
      requestId: 0,
      success: false,
      errorMessage: "text must not be empty",
      errorData: { code: "EMPTY_TEXT", data: { field: "text" } },
      logLines: [],
    });
    const err = await p.catch((e: unknown) => e);
    expect(err).toBeInstanceOf(FerrixError);
    expect(err).toMatchObject({ message: "text must not be empty", code: "EMPTY_TEXT", data: { field: "text" } });
  });

  it("applies an optimistic update and swaps it for server data without flickering back", async () => {
    const { watch, values } = await setup();
    const p = client.mutation(
      api.todos.add,
      { text: "hi" },
      {
        optimisticUpdate: (store, args) => {
          const cur = store.getQuery(api.todos.list, {});
          if (cur) store.setQuery(api.todos.list, {}, [...cur, { _id: "optimistic" as Id<"todos">, text: args.text }]);
        },
      },
    );
    expect(watch.localQueryResult()).toEqual([{ _id: "optimistic", text: "hi" }]);

    // An unrelated transition (ts below the mutation's) re-applies the optimistic update over new server data.
    server.transition({ ts: 4 }, [{ type: "QueryUpdated", queryId: 0, value: [{ _id: "other", text: "o" }] }]);
    expect(watch.localQueryResult()).toEqual([
      { _id: "other", text: "o" },
      { _id: "optimistic", text: "hi" },
    ]);

    server.push({ type: "MutationResponse", requestId: 0, success: true, result: "real", ts: 5, logLines: [] });
    expect(watch.localQueryResult()).toEqual([
      { _id: "other", text: "o" },
      { _id: "optimistic", text: "hi" },
    ]);
    server.transition({ ts: 5 }, [
      {
        type: "QueryUpdated",
        queryId: 0,
        value: [
          { _id: "other", text: "o" },
          { _id: "real", text: "hi" },
        ],
      },
    ]);
    await expect(p).resolves.toBe("real");
    expect(values).toEqual([
      [{ _id: "optimistic", text: "hi" }],
      [
        { _id: "other", text: "o" },
        { _id: "optimistic", text: "hi" },
      ],
      [
        { _id: "other", text: "o" },
        { _id: "real", text: "hi" },
      ],
    ]);
  });

  it("rolls back an optimistic update when the mutation fails", async () => {
    const { watch, values } = await setup();
    const p = client.mutation(
      api.todos.add,
      { text: "hi" },
      { optimisticUpdate: (store) => store.setQuery(api.todos.list, {}, [{ _id: "o" as Id<"todos">, text: "hi" }]) },
    );
    expect(watch.localQueryResult()).toHaveLength(1);
    server.push({ type: "MutationResponse", requestId: 0, success: false, errorMessage: "nope", errorData: null });
    expect(watch.localQueryResult()).toEqual([]);
    await expect(p).rejects.toMatchObject({ message: "nope", code: undefined });
    expect(values).toEqual([[{ _id: "o", text: "hi" }], []]);
  });

  it("layers several optimistic updates in order", async () => {
    const { watch } = await setup();
    const append = (text: string) => ({
      optimisticUpdate: (store: import("../src/index.js").OptimisticLocalStore) => {
        const cur = store.getQuery(api.todos.list, {}) ?? [];
        store.setQuery(api.todos.list, {}, [...cur, { _id: text as Id<"todos">, text }]);
      },
    });
    const p1 = client.mutation(api.todos.add, { text: "a" }, append("a"));
    const p2 = client.mutation(api.todos.add, { text: "b" }, append("b"));
    expect(watch.localQueryResult()?.map((t) => t.text)).toEqual(["a", "b"]);
    server.push({ type: "MutationResponse", requestId: 1, success: false, errorMessage: "x", errorData: null });
    expect(watch.localQueryResult()?.map((t) => t.text)).toEqual(["a"]);
    await expect(p2).rejects.toBeInstanceOf(FerrixError);
    server.push({ type: "MutationResponse", requestId: 0, success: true, result: "a1", ts: 4 });
    server.transition({ ts: 4 }, [{ type: "QueryUpdated", queryId: 0, value: [{ _id: "a1", text: "a" }] }]);
    await expect(p1).resolves.toBe("a1");
    expect(watch.localQueryResult()).toEqual([{ _id: "a1", text: "a" }]);
  });

  it("getAllQueries lists every query of a function", async () => {
    await openAndDrain();
    client.watchQuery(api.todos.get, { id: "1" }).onUpdate(() => {});
    client.watchQuery(api.todos.get, { id: "2" }).onUpdate(() => {});
    await flushMicrotasks();
    server.transition({ querySet: 1, ts: 1 }, [
      { type: "QueryUpdated", queryId: 0, value: { _id: "1", text: "one" } },
      { type: "QueryUpdated", queryId: 1, value: null },
    ]);
    let all: unknown;
    void client
      .mutation(api.todos.add, { text: "x" }, { optimisticUpdate: (store) => (all = store.getAllQueries(api.todos.get)) })
      .catch(() => {});
    expect(all).toEqual([
      { args: { id: "1" }, value: { _id: "1", text: "one" } },
      { args: { id: "2" }, value: null },
    ]);
  });
});

describe("actions", () => {
  it("resolves on ActionResponse and maps errors", async () => {
    await openAndDrain();
    const ok = client.action(api.ai.summarize, { text: "t" });
    const bad = client.action(api.ai.summarize, { text: "u" });
    expect(server.drain()).toEqual([
      { type: "Action", requestId: 0, udfPath: "ai:summarize", args: { text: "t" } },
      { type: "Action", requestId: 1, udfPath: "ai:summarize", args: { text: "u" } },
    ]);
    server.push({ type: "ActionResponse", requestId: 0, success: true, result: "sum", logLines: [] });
    server.push({
      type: "ActionResponse",
      requestId: 1,
      success: false,
      errorMessage: "quota",
      errorData: { code: "RATE_LIMITED", data: 3 },
      logLines: [],
    });
    await expect(ok).resolves.toBe("sum");
    await expect(bad).rejects.toMatchObject({ code: "RATE_LIMITED", data: 3, message: "quota" });
  });
});

describe("reconnect", () => {
  it("re-adds live queries, resends unacknowledged mutations and rejects in-flight actions", async () => {
    const watch = client.watchQuery(api.todos.list, {});
    watch.onUpdate(() => {});
    client.watchQuery(api.todos.count).onUpdate(() => {});
    const dropped = client.watchQuery(api.todos.get, { id: "x" }).onUpdate(() => {});
    await openAndDrain();
    server.transition({ querySet: 1, ts: 2 }, [
      { type: "QueryUpdated", queryId: 0, value: [] },
      { type: "QueryUpdated", queryId: 1, value: 0 },
      { type: "QueryUpdated", queryId: 2, value: null },
    ]);
    dropped();
    await flushMicrotasks();

    const acked = vi.fn();
    const m1 = client.mutation(api.todos.add, { text: "acked" }).then(acked);
    const m2 = client.mutation(api.todos.add, { text: "unacked" });
    const a = client.action(api.ai.summarize, { text: "t" });
    server.push({ type: "MutationResponse", requestId: 0, success: true, result: "r0", ts: 3 });
    server.drain();

    server.close(1006, "network");
    const actionError = await a.catch((e: unknown) => e);
    expect(actionError).toBeInstanceOf(FerrixError);
    expect((actionError as FerrixError).code).toBe(CONNECTION_LOST);

    // Calls made while disconnected are queued.
    const a2 = client.action(api.ai.summarize, { text: "later" });

    vi.advanceTimersByTime(20_000);
    const sent = await openAndDrain();
    expect(sent).toEqual([
      {
        type: "Connect",
        sessionId: client.sessionId,
        connectionCount: 1,
        lastCloseReason: "1006: network",
        clientVersion: "0.1.0",
      },
      {
        type: "ModifyQuerySet",
        baseVersion: 0,
        newVersion: 1,
        modifications: [
          { type: "Add", queryId: 0, udfPath: "todos:list", args: {} },
          { type: "Add", queryId: 1, udfPath: "todos:count", args: {} },
        ],
      },
      { type: "Mutation", requestId: 1, udfPath: "todos:add", args: { text: "unacked" } },
      { type: "Action", requestId: 3, udfPath: "ai:summarize", args: { text: "later" } },
    ]);

    // The acked mutation (ts 3) waits for the new socket's queries to be at ts >= 3.
    await flushMicrotasks();
    expect(acked).not.toHaveBeenCalled();
    server.transition({ querySet: 1, ts: 4 }, [
      { type: "QueryUpdated", queryId: 0, value: [{ _id: "r0", text: "acked" }] },
      { type: "QueryUpdated", queryId: 1, value: 1 },
    ]);
    await m1;
    expect(acked).toHaveBeenCalledWith("r0");
    expect(watch.localQueryResult()).toEqual([{ _id: "r0", text: "acked" }]);

    server.push({ type: "MutationResponse", requestId: 1, success: true, result: "r1", ts: 5 });
    server.transition({ ts: 5 });
    await expect(m2).resolves.toBe("r1");
    server.push({ type: "ActionResponse", requestId: 3, success: true, result: "ok" });
    await expect(a2).resolves.toBe("ok");
  });

  it("does not complete a mutation on an identity-only transition while resyncing", async () => {
    client.setAuth(async () => "tok");
    client.watchQuery(api.todos.count).onUpdate(() => {});
    await openAndDrain();
    server.transition({ querySet: 1, identity: 1, ts: 1 }, [{ type: "QueryUpdated", queryId: 0, value: 0 }]);
    const done = vi.fn();
    void client.mutation(api.todos.add, { text: "x" }).then(done);
    server.push({ type: "MutationResponse", requestId: 0, success: true, result: "r", ts: 2 });
    server.close();
    vi.advanceTimersByTime(20_000);
    await openAndDrain();
    server.transition({ identity: 1, ts: 2 }); // Authenticate processed; the query set is not re-added yet
    await flushMicrotasks();
    expect(done).not.toHaveBeenCalled();
    server.transition({ querySet: 1 }, [{ type: "QueryUpdated", queryId: 0, value: 1 }]);
    await flushMicrotasks();
    expect(done).toHaveBeenCalledWith("r");
  });
});

describe("errors", () => {
  it("maps QueryFailed errorData.code onto FerrixError.code", async () => {
    const watch = client.watchQuery(api.todos.get, { id: "zzz" });
    watch.onUpdate(() => {});
    await openAndDrain();
    server.transition({ querySet: 1, ts: 1 }, [
      {
        type: "QueryFailed",
        queryId: 0,
        errorMessage: "todo not found",
        errorData: { code: "NOT_FOUND", data: { id: "zzz" } },
        logLines: [],
      },
    ]);
    const state = watch.localQueryState();
    expect(state.status).toBe("error");
    expect(() => watch.localQueryResult()).toThrow(FerrixError);
    try {
      watch.localQueryResult();
    } catch (e) {
      expect(e).toMatchObject({ code: "NOT_FOUND", data: { id: "zzz" }, message: "todo not found" });
    }

    // A later success replaces the error (the subscription stays alive).
    server.transition({ ts: 2 }, [{ type: "QueryUpdated", queryId: 0, value: { _id: "zzz", text: "" } }]);
    expect(watch.localQueryResult()).toEqual({ _id: "zzz", text: "" });
  });

  it("uses a plain errorMessage when errorData is null", async () => {
    const watch = client.watchQuery(api.todos.count);
    watch.onUpdate(() => {});
    await openAndDrain();
    server.transition({ querySet: 1, ts: 1 }, [
      { type: "QueryFailed", queryId: 0, errorMessage: "Server Error", errorData: null },
    ]);
    const s = watch.localQueryState();
    expect(s.status === "error" && s.error.code).toBe(undefined);
    expect(s.status === "error" && s.error.message).toBe("Server Error");
  });

  it("reconnects on FatalError", async () => {
    await openAndDrain();
    server.push({ type: "FatalError", error: "boom" });
    expect(server.sockets[0]!.closedByClient).toBe(true);
    vi.advanceTimersByTime(20_000);
    const sent = await openAndDrain();
    expect(sent[0]).toMatchObject({ type: "Connect", lastCloseReason: "FatalError: boom" });
  });
});

describe("auth", () => {
  it("sends Authenticate before the query set and re-sends it on reconnect", async () => {
    const onChange = vi.fn();
    const fetchToken = vi.fn(async () => "tok-1");
    client.setAuth(fetchToken, onChange);
    client.watchQuery(api.todos.count).onUpdate(() => {});
    await flushMicrotasks();
    const sent = await openAndDrain();
    expect(sent.map((m) => m.type)).toEqual(["Connect", "Authenticate", "ModifyQuerySet"]);
    expect(sent[1]).toEqual({ type: "Authenticate", baseVersion: 0, token: "tok-1" });
    expect(fetchToken).toHaveBeenCalledWith({ forceRefresh: false });

    expect(onChange).not.toHaveBeenCalled();
    server.transition({ querySet: 1, identity: 1, ts: 1 }, [{ type: "QueryUpdated", queryId: 0, value: 1 }]);
    expect(onChange).toHaveBeenCalledWith(true);
    expect(client.connectionState().isAuthenticated).toBe(true);

    server.close();
    vi.advanceTimersByTime(20_000);
    const resent = await openAndDrain();
    expect(resent.map((m) => m.type)).toEqual(["Connect", "Authenticate", "ModifyQuerySet"]);
    expect(resent[1]).toEqual({ type: "Authenticate", baseVersion: 0, token: "tok-1" });

    client.clearAuth();
    expect(server.drain()).toEqual([{ type: "Authenticate", baseVersion: 1, token: null }]);
    expect(onChange).toHaveBeenLastCalledWith(false);
  });

  it("holds back subscriptions while the first token is fetched", async () => {
    let release!: (t: string) => void;
    client.setAuth(() => new Promise<string>((r) => (release = r)));
    client.watchQuery(api.todos.count).onUpdate(() => {});
    server.open();
    await flushMicrotasks();
    expect(server.drain().map((m) => m.type)).toEqual(["Connect"]);
    release("tok");
    await flushMicrotasks();
    expect(server.drain()).toEqual([
      { type: "Authenticate", baseVersion: 0, token: "tok" },
      {
        type: "ModifyQuerySet",
        baseVersion: 0,
        newVersion: 1,
        modifications: [{ type: "Add", queryId: 0, udfPath: "todos:count", args: {} }],
      },
    ]);
  });

  it("works with an external provider's token getter (Clerk, Auth0, Supabase)", async () => {
    // e.g. client.setAuth(({ forceRefresh }) => clerk.session.getToken({ skipCache: forceRefresh }))
    const b64 = (o: unknown) => btoa(JSON.stringify(o)).replace(/=+$/, "").replace(/\+/g, "-").replace(/\//g, "_");
    const issued: string[] = [];
    let signedIn = true;
    const getToken = vi.fn(async ({ forceRefresh }: { forceRefresh: boolean }) => {
      if (!signedIn) return null;
      const exp = Math.floor(Date.now() / 1000) + 60; // short-lived, as Clerk's are
      const token = `${b64({ alg: "RS256", kid: "k1" })}.${b64({ iss: "https://clerk.example.com", sub: "user_1", exp })}.sig${forceRefresh ? "!" : ""}`;
      issued.push(token);
      return token;
    });
    const onChange = vi.fn();
    client.setAuth(getToken, onChange);
    await flushMicrotasks();
    const sent = await openAndDrain();
    expect(sent[1]).toEqual({ type: "Authenticate", baseVersion: 0, token: issued[0] });
    server.transition({ identity: 1, ts: 1 });
    expect(onChange).toHaveBeenLastCalledWith(true);

    // Refreshed 30s before `exp` with forceRefresh, and sent on the same socket (kept alive by the server's pings).
    vi.advanceTimersByTime(15_000);
    server.push({ type: "Ping" });
    vi.advanceTimersByTime(15_000);
    await flushMicrotasks();
    expect(getToken).toHaveBeenLastCalledWith({ forceRefresh: true });
    expect(server.drain().filter((m) => m.type !== "Pong")).toEqual([{ type: "Authenticate", baseVersion: 1, token: issued[1] }]);
    server.transition({ identity: 2, ts: 1 });

    // The user is deleted (or signs out at the provider): AuthError, the getter has nothing more, anonymous again.
    signedIn = false;
    server.push({ type: "AuthError", error: "session has ended" });
    await flushMicrotasks();
    expect(getToken).toHaveBeenLastCalledWith({ forceRefresh: true });
    expect(onChange).toHaveBeenLastCalledWith(false);
    expect(client.connectionState().isAuthenticated).toBe(false);
    vi.advanceTimersByTime(20_000);
    const after = await openAndDrain();
    expect(after.map((m) => m.type)).toEqual(["Connect"]);
  });

  it("refetches with forceRefresh after an AuthError", async () => {
    const tokens = ["bad", "good"];
    const fetchToken = vi.fn(async () => tokens.shift() ?? null);
    const onChange = vi.fn();
    client.setAuth(fetchToken, onChange);
    await flushMicrotasks();
    await openAndDrain();
    server.push({ type: "AuthError", error: "expired" });
    await flushMicrotasks();
    expect(fetchToken).toHaveBeenLastCalledWith({ forceRefresh: true });
    vi.advanceTimersByTime(20_000);
    const sent = await openAndDrain();
    expect(sent[1]).toEqual({ type: "Authenticate", baseVersion: 0, token: "good" });
    server.transition({ identity: 1, ts: 1 });
    expect(onChange).toHaveBeenCalledWith(true);
  });
});

describe("one-shot query", () => {
  it("resolves with the first result and unsubscribes", async () => {
    await openAndDrain();
    const p = client.query(api.todos.count);
    await flushMicrotasks();
    expect(server.drain()).toMatchObject([{ type: "ModifyQuerySet", modifications: [{ type: "Add", queryId: 0 }] }]);
    server.transition({ querySet: 1, ts: 1 }, [{ type: "QueryUpdated", queryId: 0, value: 42 }]);
    await expect(p).resolves.toBe(42);
    await flushMicrotasks();
    expect(server.drain()).toEqual([
      { type: "ModifyQuerySet", baseVersion: 1, newVersion: 2, modifications: [{ type: "Remove", queryId: 0 }] },
    ]);
  });
});

describe("close", () => {
  it("rejects pending requests", async () => {
    await openAndDrain();
    const p = client.mutation(api.todos.add, { text: "x" });
    client.close();
    await expect(p).rejects.toMatchObject({ code: "ClientClosed" });
    expect(server.socket.closedByClient).toBe(true);
  });
});

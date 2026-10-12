import { afterEach, beforeEach, describe, expect, expectTypeOf, it, vi } from "vitest";
import {
  RxFerrixClient,
  getAllPages,
  insertAtTop,
  makeFunctionReference,
  type Id,
  type PaginatedQuery,
  type PaginatedQueryArgs,
  type PaginatedQueryItem,
  type PaginationOpts,
  type PaginationResult,
  type PaginationStatus,
} from "../src/index.js";
import type { ClientMessage } from "../src/protocol.js";
import { FakeServer, flushMicrotasks } from "../src/testing.js";

type Message = { _id: Id<"messages">; body: string };

const api = {
  messages: {
    list: makeFunctionReference<
      "query",
      { channel: string; paginationOpts: PaginationOpts },
      PaginationResult<Message>
    >("query", "messages:list"),
    send: makeFunctionReference<"mutation", { channel: string; body: string }, Id<"messages">>(
      "mutation",
      "messages:send",
    ),
    count: makeFunctionReference<"query", Record<string, never>, number>("query", "messages:count"),
  },
};

const m = (id: string): Message => ({ _id: id as Id<"messages">, body: `body ${id}` });
const page = (ids: string[], continueCursor: string, isDone = false): PaginationResult<Message> => ({
  page: ids.map(m),
  isDone,
  continueCursor,
});
const ids = (q: PaginatedQuery<Message>) => q.getCurrent().results.map((r) => r._id);

let server: FakeServer;
let client: RxFerrixClient;

beforeEach(async () => {
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "setInterval", "clearInterval", "Date"] });
  server = new FakeServer();
  client = new RxFerrixClient("http://localhost:3210", {
    webSocketConstructor: server.WebSocket,
    unsavedChangesWarning: false,
  });
  server.open();
  await flushMicrotasks();
  server.drain();
});

afterEach(() => {
  client.close();
  vi.useRealTimers();
});

/** The modifications of every ModifyQuerySet sent since the last call. */
async function sentModifications(): Promise<unknown[]> {
  await flushMicrotasks();
  return server
    .drain()
    .filter((msg): msg is Extract<ClientMessage, { type: "ModifyQuerySet" }> => msg.type === "ModifyQuerySet")
    .flatMap((msg) => msg.modifications);
}

function update(queryId: number, value: unknown) {
  return { type: "QueryUpdated" as const, queryId, value, logLines: [] };
}

/** A paginated query on channel "a" with 2 items per first page, subscribed, with page 0 (query 0) loaded. */
async function loadedFirstPage() {
  const q = client.paginatedQuery(api.messages.list, { channel: "a" }, { initialNumItems: 2 });
  const statuses: PaginationStatus[] = [];
  const unsubscribe = q.subscribe(() => statuses.push(q.getCurrent().status));
  expect(await sentModifications()).toEqual([
    {
      type: "Add",
      queryId: 0,
      udfPath: "messages:list",
      args: { channel: "a", paginationOpts: { numItems: 2, cursor: null } },
    },
  ]);
  server.transition({ querySet: 1, ts: 1 }, [update(0, page(["m1", "m2"], "c1"))]);
  return { q, statuses, unsubscribe };
}

describe("paginatedQuery", () => {
  it("starts with LoadingFirstPage and an empty, stable result", () => {
    const q = client.paginatedQuery(api.messages.list, { channel: "a" }, { initialNumItems: 2 });
    const first = q.getCurrent();
    expect(first).toMatchObject({ results: [], status: "LoadingFirstPage", isLoading: true, error: undefined });
    expect(q.getCurrent()).toBe(first);
    expect(first.loadMore(5)).toBe(false);
  });

  it("walks pages, pinning each page to the cursor its successor starts at", async () => {
    const { q, statuses } = await loadedFirstPage();
    expect(q.getCurrent()).toMatchObject({ status: "CanLoadMore", isLoading: false });
    expect(ids(q)).toEqual(["m1", "m2"]);

    expect(q.getCurrent().loadMore(3)).toBe(true);
    expect(q.getCurrent().status).toBe("LoadingMore");
    expect(ids(q)).toEqual(["m1", "m2"]);
    expect(await sentModifications()).toEqual([
      {
        type: "Add",
        queryId: 1,
        udfPath: "messages:list",
        args: { channel: "a", paginationOpts: { numItems: 2, cursor: null, endCursor: "c1" } },
      },
      {
        type: "Add",
        queryId: 2,
        udfPath: "messages:list",
        args: { channel: "a", paginationOpts: { numItems: 3, cursor: "c1" } },
      },
    ]);
    // A second loadMore while the last page is loading does nothing.
    expect(q.getCurrent().loadMore(3)).toBe(false);

    // Page 1 arrives before the pinned copy of page 0: page 0 still shows the unpinned subscription.
    server.transition({ querySet: 2, ts: 1 }, [update(2, page(["m3", "m4", "m5"], "c2"))]);
    expect(ids(q)).toEqual(["m1", "m2", "m3", "m4", "m5"]);
    expect(q.getCurrent().status).toBe("CanLoadMore");
    server.transition({ ts: 2 }, [update(1, page(["m1", "m2"], "c1"))]);
    expect(await sentModifications()).toEqual([{ type: "Remove", queryId: 0 }]);

    q.getCurrent().loadMore(10);
    expect(await sentModifications()).toEqual([
      {
        type: "Add",
        queryId: 3,
        udfPath: "messages:list",
        args: { channel: "a", paginationOpts: { numItems: 3, cursor: "c1", endCursor: "c2" } },
      },
      {
        type: "Add",
        queryId: 4,
        udfPath: "messages:list",
        args: { channel: "a", paginationOpts: { numItems: 10, cursor: "c2" } },
      },
    ]);
    server.transition({ querySet: 3, ts: 2 }, [
      update(3, page(["m3", "m4", "m5"], "c2")),
      update(4, page(["m6"], "end", true)),
    ]);
    expect(ids(q)).toEqual(["m1", "m2", "m3", "m4", "m5", "m6"]);
    expect(q.getCurrent()).toMatchObject({ status: "Exhausted", isLoading: false });
    expect(q.getCurrent().loadMore(1)).toBe(false);
    expect(await sentModifications()).toEqual([{ type: "Remove", queryId: 2 }]);
    expect(statuses).toEqual(["CanLoadMore", "LoadingMore", "CanLoadMore", "LoadingMore", "Exhausted"]);

    // The last page is not pinned: new items after it show up there and it can load more again.
    server.transition({ ts: 3 }, [update(4, page(["m6", "m7"], "c3"))]);
    expect(ids(q)).toEqual(["m1", "m2", "m3", "m4", "m5", "m6", "m7"]);
    expect(q.getCurrent().status).toBe("CanLoadMore");
  });

  it("keeps the unpinned page until the pinned one arrives, without a loading flash", async () => {
    const { q, statuses } = await loadedFirstPage();
    const snapshots: string[][] = [];
    q.subscribe(() => snapshots.push(ids(q)));
    q.getCurrent().loadMore(2);
    await sentModifications(); // 1 = pinned page 0, 2 = page 1
    server.transition({ querySet: 2, ts: 1 }, [update(2, page(["m3", "m4"], "c2"))]);

    // An insert at the top: the unpinned page 0 shifts (m2 falls off its end), so m2 is briefly missing.
    server.transition({ ts: 2 }, [update(0, page(["m0", "m1"], "c0"))]);
    expect(ids(q)).toEqual(["m0", "m1", "m3", "m4"]);
    // The pinned page has every item up to the boundary.
    server.transition({ ts: 2 }, [update(1, page(["m0", "m1", "m2"], "c1"))]);
    expect(ids(q)).toEqual(["m0", "m1", "m2", "m3", "m4"]);
    expect(await sentModifications()).toEqual([{ type: "Remove", queryId: 0 }]);
    // Later updates of the unpinned query (in flight when it was removed) are ignored.
    server.transition({ ts: 3 }, [update(0, page(["x"], "cx"))]);
    expect(ids(q)).toEqual(["m0", "m1", "m2", "m3", "m4"]);

    expect(statuses).toEqual(["CanLoadMore", "LoadingMore", "CanLoadMore", "CanLoadMore", "CanLoadMore"]);
    expect(snapshots).toEqual([
      ["m1", "m2"],
      ["m1", "m2", "m3", "m4"],
      ["m0", "m1", "m3", "m4"],
      ["m0", "m1", "m2", "m3", "m4"],
    ]);
  });

  it("keeps showing results across a reconnect and re-adds every page", async () => {
    const { q } = await loadedFirstPage();
    q.getCurrent().loadMore(2);
    await sentModifications();
    server.transition({ querySet: 2, ts: 2 }, [update(1, page(["m1", "m2"], "c1")), update(2, page(["m3"], "e", true))]);
    expect(await sentModifications()).toEqual([{ type: "Remove", queryId: 0 }]);
    const before = q.getCurrent();
    expect(before).toMatchObject({ status: "Exhausted" });

    server.close(1006, "network");
    expect(q.getCurrent()).toBe(before);
    vi.advanceTimersByTime(20_000);
    server.open();
    expect(q.getCurrent()).toBe(before);
    const resent = (await sentModifications()) as { type: string; queryId: number; args: unknown }[];
    expect(resent.map((x) => [x.type, x.queryId])).toEqual([
      ["Add", 1],
      ["Add", 2],
    ]);
    expect(resent[0]!.args).toEqual({ channel: "a", paginationOpts: { numItems: 2, cursor: null, endCursor: "c1" } });

    server.transition({ querySet: 1, ts: 5 }, [update(1, page(["m1", "m2"], "c1")), update(2, page(["m3", "m4"], "e", true))]);
    expect(ids(q)).toEqual(["m1", "m2", "m3", "m4"]);
  });

  it("unsubscribing drops every page; a new paginated query (new args) starts from the first page", async () => {
    const { q, unsubscribe } = await loadedFirstPage();
    q.getCurrent().loadMore(2);
    await sentModifications();
    unsubscribe();
    expect(await sentModifications()).toEqual([
      { type: "Remove", queryId: 0 },
      { type: "Remove", queryId: 1 },
      { type: "Remove", queryId: 2 },
    ]);

    // Args change = a new paginated query.
    const other = client.paginatedQuery(api.messages.list, { channel: "b" }, { initialNumItems: 5 });
    const stop = other.subscribe(() => {});
    expect(await sentModifications()).toEqual([
      {
        type: "Add",
        queryId: 3,
        udfPath: "messages:list",
        args: { channel: "b", paginationOpts: { numItems: 5, cursor: null } },
      },
    ]);
    expect(other.getCurrent().status).toBe("LoadingFirstPage");
    stop();

    // Resubscribing the first one picks up where it was (same cursors).
    q.subscribe(() => {});
    const readded = (await sentModifications()) as { type: string; args: { paginationOpts: unknown } }[];
    expect(readded.filter((x) => x.type === "Add").map((x) => x.args.paginationOpts)).toEqual([
      { numItems: 2, cursor: null },
      { numItems: 2, cursor: null, endCursor: "c1" },
      { numItems: 2, cursor: "c1" },
    ]);
  });

  it("manages paginationOpts itself and validates numItems", async () => {
    expect(() => client.paginatedQuery(api.messages.list, { channel: "a" }, { initialNumItems: 0 })).toThrow(
      /initialNumItems/,
    );
    const q = client.paginatedQuery(
      api.messages.list,
      { channel: "a", paginationOpts: { numItems: 99, cursor: "zzz" } } as never,
      { initialNumItems: 1 },
    );
    q.subscribe(() => {});
    expect(await sentModifications()).toEqual([
      {
        type: "Add",
        queryId: 0,
        udfPath: "messages:list",
        args: { channel: "a", paginationOpts: { numItems: 1, cursor: null } },
      },
    ]);
    server.transition({ querySet: 1, ts: 1 }, [update(0, page(["m1"], "c1"))]);
    expect(() => q.getCurrent().loadMore(1.5)).toThrow(/loadMore/);
  });

  it("surfaces a failed page as an error", async () => {
    const { q } = await loadedFirstPage();
    q.getCurrent().loadMore(2);
    await sentModifications();
    server.transition({ querySet: 2, ts: 2 }, [
      { type: "QueryFailed", queryId: 2, errorMessage: "paginate: cursor does not belong", errorData: null, logLines: [] },
    ]);
    const current = q.getCurrent();
    expect(current.error?.message).toBe("paginate: cursor does not belong");
    expect(current.isLoading).toBe(false);
    expect(current.results.map((r) => r._id)).toEqual(["m1", "m2"]);
    expect(current.loadMore(1)).toBe(false);
  });
});

describe("optimistic updates on paginated queries", () => {
  it("getAllQueries / getAllPages list every page; insertAtTop prepends to the first page", async () => {
    const { q } = await loadedFirstPage();
    q.getCurrent().loadMore(2);
    await sentModifications();
    server.transition({ querySet: 2, ts: 2 }, [update(1, page(["m1", "m2"], "c1")), update(2, page(["m3"], "e", true))]);
    // Another channel's list, which must not be touched.
    const other = client.paginatedQuery(api.messages.list, { channel: "b" }, { initialNumItems: 2 });
    other.subscribe(() => {});
    await sentModifications();
    server.transition({ querySet: 3, ts: 2 }, [update(3, page(["b1"], "e", true))]);

    let seen: unknown[] = [];
    const send = client.mutation(
      api.messages.send,
      { channel: "a", body: "hi" },
      {
        optimisticUpdate: (store, args) => {
          seen = store.getAllQueries(api.messages.list).map((x) => x.args.paginationOpts);
          expect(getAllPages(store, api.messages.list, { channel: "a" }).map((p) => p.value.page.length)).toEqual([
            2, 1,
          ]);
          insertAtTop({
            paginatedQuery: api.messages.list,
            argsToMatch: { channel: args.channel },
            localQueryStore: store,
            item: { _id: "optimistic" as Id<"messages">, body: args.body },
          });
        },
      },
    );
    expect(seen).toEqual([
      { numItems: 2, cursor: null, endCursor: "c1" },
      { numItems: 2, cursor: "c1" },
      { numItems: 2, cursor: null },
    ]);
    expect(ids(q)).toEqual(["optimistic", "m1", "m2", "m3"]);
    expect(other.getCurrent().results.map((r) => r._id)).toEqual(["b1"]);

    server.push({ type: "MutationResponse", requestId: 0, success: true, result: "real", ts: 3, logLines: [] });
    server.transition({ ts: 3 }, [update(1, page(["real", "m1", "m2"], "c1"))]);
    await expect(send).resolves.toBe("real");
    expect(ids(q)).toEqual(["real", "m1", "m2", "m3"]);
  });
});

describe("types", () => {
  it("infers the item type and requires the args without paginationOpts", () => {
    expectTypeOf<PaginatedQueryItem<typeof api.messages.list>>().toEqualTypeOf<Message>();
    expectTypeOf<PaginatedQueryArgs<typeof api.messages.list>>().toEqualTypeOf<{ channel: string }>();
    const _check = (c: RxFerrixClient) => {
      const q = c.paginatedQuery(api.messages.list, { channel: "a" }, { initialNumItems: 1 });
      expectTypeOf(q.getCurrent().results).toEqualTypeOf<Message[]>();
      // @ts-expect-error missing `channel`
      c.paginatedQuery(api.messages.list, {}, { initialNumItems: 1 });
      // @ts-expect-error not a paginated query
      c.paginatedQuery(api.messages.count, {}, { initialNumItems: 1 });
      // @ts-expect-error initialNumItems is required
      c.paginatedQuery(api.messages.list, { channel: "a" }, {});
    };
    void _check;
  });
});

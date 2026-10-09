import { Component, StrictMode, type ReactNode } from "react";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, expectTypeOf, it, vi } from "vitest";
import {
  FerrixClient,
  makeFunctionReference,
  type Id,
  type PaginationOpts,
  type PaginationResult,
} from "@ferrix/client";
import { FakeServer, flushMicrotasks } from "@ferrix/client/testing";
import {
  FerrixProvider,
  usePaginatedQuery,
  usePaginatedQueryWithStatus,
  type UsePaginatedQueryResult,
  type UsePaginatedQueryWithStatusResult,
} from "../src/index.js";

type Message = { _id: Id<"messages">; body: string };
const api = {
  messages: {
    list: makeFunctionReference<
      "query",
      { channel: string; paginationOpts: PaginationOpts },
      PaginationResult<Message>
    >("query", "messages:list"),
  },
};

const page = (ids: string[], continueCursor: string, isDone = false): PaginationResult<Message> => ({
  page: ids.map((id) => ({ _id: id as Id<"messages">, body: id })),
  isDone,
  continueCursor,
});
const update = (queryId: number, value: unknown) => ({ type: "QueryUpdated" as const, queryId, value, logLines: [] });

let server: FakeServer;
let client: FerrixClient;

beforeEach(async () => {
  server = new FakeServer();
  client = new FerrixClient("http://localhost:3210", {
    webSocketConstructor: server.WebSocket,
    unsavedChangesWarning: false,
  });
  server.open();
  await flushMicrotasks();
  server.drain();
});

afterEach(() => {
  cleanup();
  client.close();
});

async function settle() {
  await act(async () => {
    await flushMicrotasks();
  });
}

/** The modifications of the ModifyQuerySets sent since the last call. */
function sentModifications(): unknown[] {
  return server.drain().flatMap((m) => (m.type === "ModifyQuerySet" ? m.modifications : []));
}

function Messages({ channel }: { channel: string | null }) {
  const { results, status, isLoading, loadMore } = usePaginatedQuery(
    api.messages.list,
    channel === null ? "skip" : { channel },
    { initialNumItems: 2 },
  );
  return (
    <div>
      <p data-testid="status">
        {status} {isLoading ? "loading" : "idle"}
      </p>
      <ul>
        {results.map((m) => (
          <li key={m._id}>{m.body}</li>
        ))}
      </ul>
      <button onClick={() => loadMore(3)}>more</button>
    </div>
  );
}

function renderMessages(channel: string | null, strict = false) {
  const tree = (c: string | null) => (
    <FerrixProvider client={client}>
      <Messages channel={c} />
    </FerrixProvider>
  );
  const r = render(strict ? <StrictMode>{tree(channel)}</StrictMode> : tree(channel));
  return { rerender: (c: string | null) => r.rerender(strict ? <StrictMode>{tree(c)}</StrictMode> : tree(c)) };
}

const items = () => screen.queryAllByRole("listitem").map((li) => li.textContent);
const status = () => screen.getByTestId("status").textContent;

describe("usePaginatedQuery", () => {
  it("loads the first page, loads more and pins the first page without a loading flash", async () => {
    renderMessages("a", true);
    expect(status()).toBe("LoadingFirstPage loading");
    await settle();
    expect(sentModifications()).toEqual([
      { type: "Add", queryId: 0, udfPath: "messages:list", args: { channel: "a", paginationOpts: { numItems: 2, cursor: null } } },
    ]);
    act(() => server.transition({ querySet: 1, ts: 1 }, [update(0, page(["m1", "m2"], "c1"))]));
    expect(items()).toEqual(["m1", "m2"]);
    expect(status()).toBe("CanLoadMore idle");

    fireEvent.click(screen.getByText("more"));
    expect(status()).toBe("LoadingMore loading");
    await settle();
    expect(sentModifications()).toEqual([
      {
        type: "Add",
        queryId: 1,
        udfPath: "messages:list",
        args: { channel: "a", paginationOpts: { numItems: 2, cursor: null, endCursor: "c1" } },
      },
      { type: "Add", queryId: 2, udfPath: "messages:list", args: { channel: "a", paginationOpts: { numItems: 3, cursor: "c1" } } },
    ]);
    act(() => server.transition({ querySet: 2, ts: 1 }, [update(2, page(["m3"], "end", true))]));
    expect(items()).toEqual(["m1", "m2", "m3"]);
    expect(status()).toBe("Exhausted idle");

    act(() => server.transition({ ts: 2 }, [update(1, page(["m0", "m1", "m2"], "c1"))]));
    expect(items()).toEqual(["m0", "m1", "m2", "m3"]);
    await settle();
    expect(sentModifications()).toEqual([{ type: "Remove", queryId: 0 }]);
  });

  it("starts over when the args change, and subscribes to nothing when skipped", async () => {
    const { rerender } = renderMessages(null);
    await settle();
    expect(status()).toBe("LoadingFirstPage loading");
    expect(server.drain()).toEqual([]);

    rerender("a");
    await settle();
    act(() => server.transition({ querySet: 1, ts: 1 }, [update(0, page(["a1", "a2"], "c1"))]));
    fireEvent.click(screen.getByText("more"));
    await settle();
    act(() => server.transition({ querySet: 2, ts: 1 }, [update(1, page(["a1", "a2"], "c1")), update(2, page(["a3"], "e", true))]));
    expect(items()).toEqual(["a1", "a2", "a3"]);
    await settle();
    server.drain();

    rerender("b");
    expect(status()).toBe("LoadingFirstPage loading");
    expect(items()).toEqual([]);
    await settle();
    expect(sentModifications()).toEqual([
      { type: "Add", queryId: 3, udfPath: "messages:list", args: { channel: "b", paginationOpts: { numItems: 2, cursor: null } } },
      { type: "Remove", queryId: 1 },
      { type: "Remove", queryId: 2 },
    ]);
    act(() => server.transition({ querySet: 4, ts: 2 }, [update(3, page(["b1"], "e", true))]));
    expect(items()).toEqual(["b1"]);

    rerender(null);
    await settle();
    expect(sentModifications()).toEqual([{ type: "Remove", queryId: 3 }]);
    expect(items()).toEqual([]);
  });

  it("throws a failed page to an ErrorBoundary", async () => {
    class Boundary extends Component<{ children: ReactNode }, { error: Error | null }> {
      override state = { error: null as Error | null };
      static getDerivedStateFromError(error: Error) {
        return { error };
      }
      override render() {
        return this.state.error ? <p>caught {this.state.error.message}</p> : this.props.children;
      }
    }
    vi.spyOn(console, "error").mockImplementation(() => {});
    render(
      <FerrixProvider client={client}>
        <Boundary>
          <Messages channel="a" />
        </Boundary>
      </FerrixProvider>,
    );
    await settle();
    act(() =>
      server.transition({ querySet: 1, ts: 1 }, [
        { type: "QueryFailed", queryId: 0, errorMessage: "nope", errorData: null, logLines: [] },
      ]),
    );
    expect(screen.getByText("caught nope")).toBeTruthy();
  });

  it("returns a failed page as `error` with usePaginatedQueryWithStatus, keeping the loaded pages", async () => {
    function Status() {
      const { results, status, isLoading, error, loadMore } = usePaginatedQueryWithStatus(
        api.messages.list,
        { channel: "a" },
        { initialNumItems: 2 },
      );
      return (
        <div>
          <p data-testid="status">
            {status} {String(isLoading)} {error ? `error: ${error.message}` : "ok"} [{results.map((m) => m.body).join(",")}]
          </p>
          <button onClick={() => loadMore(2)}>more</button>
        </div>
      );
    }
    render(
      <FerrixProvider client={client}>
        <Status />
      </FerrixProvider>,
    );
    await settle();
    expect(screen.getByTestId("status").textContent).toBe("LoadingFirstPage true ok []");
    act(() => server.transition({ querySet: 1, ts: 1 }, [update(0, page(["m1", "m2"], "c1"))]));
    expect(screen.getByTestId("status").textContent).toBe("CanLoadMore false ok [m1,m2]");
    fireEvent.click(screen.getByText("more"));
    await settle();
    sentModifications();
    // The second page fails: no throw, the first page stays.
    act(() =>
      server.transition({ querySet: 2, ts: 2 }, [
        { type: "QueryFailed", queryId: 2, errorMessage: "not_a_member", errorData: null, logLines: [] },
      ]),
    );
    const shown = screen.getByTestId("status").textContent!;
    expect(shown).toBe("LoadingMore false error: not_a_member [m1,m2]");
  });

  it("infers the item type and requires the args without paginationOpts", () => {
    const _check = () => {
      const r = usePaginatedQuery(api.messages.list, { channel: "x" }, { initialNumItems: 1 });
      expectTypeOf(r).toEqualTypeOf<UsePaginatedQueryResult<Message>>();
      const s = usePaginatedQueryWithStatus(api.messages.list, { channel: "x" }, { initialNumItems: 1 });
      expectTypeOf(s).toEqualTypeOf<UsePaginatedQueryWithStatusResult<Message>>();
      usePaginatedQuery(api.messages.list, "skip", { initialNumItems: 1 });
      // @ts-expect-error missing `channel`
      usePaginatedQuery(api.messages.list, {}, { initialNumItems: 1 });
    };
    void _check;
  });
});

import { Component, StrictMode, useState, type ReactNode } from "react";
import { act, cleanup, render, screen, fireEvent } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { FerrixClient, makeFunctionReference, type Id } from "@ferrix/client";
import { FakeServer, flushMicrotasks } from "@ferrix/client/testing";
import {
  FerrixProvider,
  useAction,
  useConnectionState,
  useMutation,
  useQuery,
  useQueryWithStatus,
} from "../src/index.js";

type Todo = { _id: Id<"todos">; text: string };
const api = {
  todos: {
    list: makeFunctionReference<"query", { done?: boolean | null }, Todo[]>("query", "todos:list"),
    get: makeFunctionReference<"query", { id: string }, Todo | null>("query", "todos:get"),
    add: makeFunctionReference<"mutation", { text: string }, Id<"todos">>("mutation", "todos:add"),
  },
  ai: { run: makeFunctionReference<"action", Record<string, never>, string>("action", "ai:run") },
};

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

function renderWithClient(ui: ReactNode, strict = false) {
  const tree = <FerrixProvider client={client}>{ui}</FerrixProvider>;
  return render(strict ? <StrictMode>{tree}</StrictMode> : tree);
}

async function settle() {
  await act(async () => {
    await flushMicrotasks();
  });
}

function TodoList({ done }: { done?: boolean }) {
  // Inline args object: must not resubscribe on every render.
  const todos = useQuery(api.todos.list, { done: done ?? null });
  if (todos === undefined) return <p>loading</p>;
  return (
    <ul>
      {todos.map((t) => (
        <li key={t._id}>{t.text}</li>
      ))}
    </ul>
  );
}

describe("useQuery", () => {
  it("goes loading -> value -> updated value", async () => {
    renderWithClient(<TodoList />, true);
    expect(screen.getByText("loading")).toBeTruthy();
    await settle();
    const mqs = server.sentOfType("ModifyQuerySet");
    expect(mqs).toHaveLength(1); // StrictMode double-mount does not churn the subscription
    expect(mqs[0]!.modifications).toEqual([{ type: "Add", queryId: 0, udfPath: "todos:list", args: { done: null } }]);

    act(() => server.transition({ querySet: 1, ts: 1 }, [{ type: "QueryUpdated", queryId: 0, value: [{ _id: "1", text: "first" }] }]));
    expect(screen.getByText("first")).toBeTruthy();

    act(() =>
      server.transition({ ts: 2 }, [
        {
          type: "QueryUpdated",
          queryId: 0,
          value: [
            { _id: "1", text: "first" },
            { _id: "2", text: "second" },
          ],
        },
      ]),
    );
    expect(screen.getAllByRole("listitem").map((li) => li.textContent)).toEqual(["first", "second"]);
  });

  it("does not resubscribe when re-rendered with equal inline args", async () => {
    const { rerender } = renderWithClient(<TodoList done={false} />);
    await settle();
    server.drain();
    rerender(
      <FerrixProvider client={client}>
        <TodoList done={false} />
      </FerrixProvider>,
    );
    await settle();
    expect(server.drain()).toEqual([]);

    rerender(
      <FerrixProvider client={client}>
        <TodoList done={true} />
      </FerrixProvider>,
    );
    await settle();
    expect(server.drain()).toEqual([
      {
        type: "ModifyQuerySet",
        baseVersion: 1,
        newVersion: 2,
        modifications: [
          { type: "Add", queryId: 1, udfPath: "todos:list", args: { done: true } },
          { type: "Remove", queryId: 0 },
        ],
      },
    ]);
  });

  it("subscribes to nothing when skipped", async () => {
    function Maybe({ id }: { id: string | null }) {
      const todo = useQuery(api.todos.get, id === null ? "skip" : { id });
      return <p>{todo === undefined ? "none" : (todo?.text ?? "null")}</p>;
    }
    const { rerender } = renderWithClient(<Maybe id={null} />);
    await settle();
    expect(screen.getByText("none")).toBeTruthy();
    expect(server.drain()).toEqual([]);

    rerender(
      <FerrixProvider client={client}>
        <Maybe id="a" />
      </FerrixProvider>,
    );
    await settle();
    expect(server.sentOfType("ModifyQuerySet")).toHaveLength(1);
    act(() => server.transition({ querySet: 1, ts: 1 }, [{ type: "QueryUpdated", queryId: 0, value: { _id: "a", text: "A" } }]));
    expect(screen.getByText("A")).toBeTruthy();
  });

  it("throws query errors to an ErrorBoundary", async () => {
    class Boundary extends Component<{ children: ReactNode }, { error: Error | null }> {
      override state = { error: null as Error | null };
      static getDerivedStateFromError(error: Error) {
        return { error };
      }
      override render() {
        if (this.state.error) {
          const code = (this.state.error as { code?: string }).code;
          return <p>caught {this.state.error.message} ({code})</p>;
        }
        return this.props.children;
      }
    }
    vi.spyOn(console, "error").mockImplementation(() => {});
    renderWithClient(
      <Boundary>
        <TodoList />
      </Boundary>,
    );
    await settle();
    act(() =>
      server.transition({ querySet: 1, ts: 1 }, [
        { type: "QueryFailed", queryId: 0, errorMessage: "denied", errorData: { code: "FORBIDDEN", data: null } },
      ]),
    );
    expect(screen.getByText("caught denied (FORBIDDEN)")).toBeTruthy();
  });
});

describe("useQueryWithStatus", () => {
  it("returns data, error and isLoading", async () => {
    const seen: unknown[] = [];
    function Status() {
      const r = useQueryWithStatus(api.todos.list);
      seen.push(r);
      return <p>{r.isLoading ? "loading" : r.error ? `error:${r.error.message}` : `n=${r.data?.length}`}</p>;
    }
    renderWithClient(<Status />);
    expect(screen.getByText("loading")).toBeTruthy();
    await settle();
    act(() => server.transition({ querySet: 1, ts: 1 }, [{ type: "QueryUpdated", queryId: 0, value: [] }]));
    expect(screen.getByText("n=0")).toBeTruthy();
    act(() => server.transition({ ts: 2 }, [{ type: "QueryFailed", queryId: 0, errorMessage: "bad", errorData: null }]));
    expect(screen.getByText("error:bad")).toBeTruthy();
  });
});

describe("useMutation", () => {
  it("returns a stable function and applies optimistic updates", async () => {
    const mutationFns = new Set<unknown>();
    let resultPromise: Promise<Id<"todos">> | null = null;
    function App() {
      const [, force] = useState(0);
      const add = useMutation(api.todos.add);
      mutationFns.add(add);
      const addOptimistic = add.withOptimisticUpdate((store, args) => {
        const cur = store.getQuery(api.todos.list, { done: null });
        if (cur) store.setQuery(api.todos.list, { done: null }, [...cur, { _id: "tmp" as Id<"todos">, text: args.text }]);
      });
      return (
        <>
          <TodoList />
          <button onClick={() => (resultPromise = addOptimistic({ text: "new" }))}>add</button>
          <button onClick={() => force((n) => n + 1)}>rerender</button>
        </>
      );
    }
    renderWithClient(<App />);
    await settle();
    act(() => server.transition({ querySet: 1, ts: 1 }, [{ type: "QueryUpdated", queryId: 0, value: [{ _id: "1", text: "old" }] }]));
    fireEvent.click(screen.getByText("rerender"));
    expect(mutationFns.size).toBe(1);

    fireEvent.click(screen.getByText("add"));
    expect(screen.getAllByRole("listitem").map((li) => li.textContent)).toEqual(["old", "new"]);
    expect(server.sentOfType("Mutation")).toEqual([
      { type: "Mutation", requestId: 0, udfPath: "todos:add", args: { text: "new" } },
    ]);

    act(() => server.push({ type: "MutationResponse", requestId: 0, success: true, result: "2", ts: 2 }));
    act(() =>
      server.transition({ ts: 2 }, [
        {
          type: "QueryUpdated",
          queryId: 0,
          value: [
            { _id: "1", text: "old" },
            { _id: "2", text: "new" },
          ],
        },
      ]),
    );
    expect(screen.getAllByRole("listitem").map((li) => li.textContent)).toEqual(["old", "new"]);
    await expect(resultPromise).resolves.toBe("2");
  });

  it("rolls back the optimistic update on failure", async () => {
    let p: Promise<unknown> | null = null;
    function App() {
      const add = useMutation(api.todos.add).withOptimisticUpdate((store) =>
        store.setQuery(api.todos.list, { done: null }, []),
      );
      return (
        <>
          <TodoList />
          <button onClick={() => (p = add({ text: "x" }).catch((e: unknown) => e))}>add</button>
        </>
      );
    }
    renderWithClient(<App />);
    await settle();
    act(() => server.transition({ querySet: 1, ts: 1 }, [{ type: "QueryUpdated", queryId: 0, value: [{ _id: "1", text: "old" }] }]));
    fireEvent.click(screen.getByText("add"));
    expect(screen.queryAllByRole("listitem")).toHaveLength(0);
    act(() =>
      server.push({ type: "MutationResponse", requestId: 0, success: false, errorMessage: "no", errorData: { code: "X" } }),
    );
    expect(screen.getAllByRole("listitem").map((li) => li.textContent)).toEqual(["old"]);
    await expect(p).resolves.toMatchObject({ code: "X" });
  });
});

describe("useAction and useConnectionState", () => {
  it("runs actions and reports connection state", async () => {
    let run: (() => Promise<string>) | null = null;
    function App() {
      const action = useAction(api.ai.run);
      run = () => action();
      const state = useConnectionState();
      return <p>{state.isWebSocketConnected ? "online" : "offline"}</p>;
    }
    renderWithClient(<App />);
    expect(screen.getByText("online")).toBeTruthy();
    const p = run!();
    expect(server.sentOfType("Action")).toEqual([{ type: "Action", requestId: 0, udfPath: "ai:run", args: {} }]);
    act(() => server.push({ type: "ActionResponse", requestId: 0, success: true, result: "done" }));
    await expect(p).resolves.toBe("done");

    vi.spyOn(console, "info").mockImplementation(() => {});
    act(() => server.close());
    expect(screen.getByText("offline")).toBeTruthy();
  });
});

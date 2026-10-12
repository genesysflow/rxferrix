import { describe, expectTypeOf, it } from "vitest";
import {
  RxFerrixClient,
  makeFunctionReference,
  type ArgsOf,
  type Id,
  type OptionalArgs,
  type OutputOf,
} from "../src/index.js";

type Doc = { _id: Id<"todos">; text: string };
const api = {
  todos: {
    list: makeFunctionReference<"query", { done?: boolean | null }, Doc[]>("query", "todos:list"),
    get: makeFunctionReference<"query", { id: Id<"todos"> }, Doc | null>("query", "todos:get"),
    add: makeFunctionReference<"mutation", { text: string }, Id<"todos">>("mutation", "todos:add"),
    clear: makeFunctionReference<"mutation", Record<string, never>, null>("mutation", "todos:clear"),
  },
};

describe("type inference", () => {
  it("infers args and outputs from generated references", () => {
    expectTypeOf<ArgsOf<typeof api.todos.list>>().toEqualTypeOf<{ done?: boolean | null }>();
    expectTypeOf<OutputOf<typeof api.todos.get>>().toEqualTypeOf<Doc | null>();
    expectTypeOf<OptionalArgs<typeof api.todos.list>>().toEqualTypeOf<[args?: { done?: boolean | null }]>();
    expectTypeOf<OptionalArgs<typeof api.todos.get>>().toEqualTypeOf<[args: { id: Id<"todos"> }]>();

    // Compile-time only: never executed.
    const _check = (client: RxFerrixClient) => {
    expectTypeOf(client.watchQuery(api.todos.list).localQueryResult()).toEqualTypeOf<Doc[] | undefined>();
    expectTypeOf(client.query(api.todos.get, { id: "1" as Id<"todos"> })).toEqualTypeOf<Promise<Doc | null>>();
    expectTypeOf(client.mutation(api.todos.add, { text: "a" })).toEqualTypeOf<Promise<Id<"todos">>>();
    void client.mutation(api.todos.clear);
    // @ts-expect-error missing required args
    void client.watchQuery(api.todos.get);
    // @ts-expect-error wrong arg type
    void client.mutation(api.todos.add, { text: 1 });
    // @ts-expect-error a mutation is not a query
    void client.watchQuery(api.todos.add, { text: "a" });
    };
    void _check;
  });
});

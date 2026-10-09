// Type-level usage of the code generated from the `it` test binary's registry (see tests/it/codegen.rs).
// Copied next to `generated/` and checked with `tsc --noEmit`; nothing here runs.
import { FerrixClient, type ArgsOf, type OutputOf, type PaginatedQueryArgs, type PaginatedQueryItem } from "@ferrix/client";
import {
  api,
  type DataModel,
  type Doc,
  type Id,
  type Label,
  type PaginationOpts,
  type PaginationResult,
  type Priority,
  type TableNames,
} from "./generated";

type Equals<A, B> = (<T>() => T extends A ? 1 : 2) extends <T>() => T extends B ? 1 : 2 ? true : false;
function assertType<T extends true>(): T | undefined {
  return undefined;
}

export async function check(client: FerrixClient, taskId: Id<"tasks">, noteId: Id<"notes">): Promise<void> {
  // Tables and documents.
  assertType<
    Equals<TableNames, "attachments" | "dash_items" | "items" | "lossy" | "notes" | "private_notes" | "tasks" | "todos">
  >();
  // ... and the built-in `_storage` table (file storage).
  assertType<Equals<DataModel["attachments"]["file"], Id<"_storage">>>();
  const file = {} as Doc<"_storage">;
  assertType<Equals<typeof file.size, number>>();
  assertType<Equals<typeof file.contentType, string | null>>();
  assertType<Equals<ArgsOf<typeof api.storage.app.url_of>, { file: Id<"_storage"> }>>();
  // App tables may reference the built-in `_users` table (auth); its documents carry no secrets.
  assertType<Equals<DataModel["private_notes"]["owner"], Id<"_users">>>();
  const user = {} as Doc<"_users">;
  assertType<Equals<typeof user._id, Id<"_users">>>();
  assertType<Equals<typeof user.email, string | null>>();
  // @ts-expect-error no password hash on users
  void user.passwordHash;
  // @ts-expect-error other system tables are not exposed
  void ({} as Id<"_sessions">);
  assertType<Equals<DataModel["tasks"]["priority"], Priority>>();
  assertType<Equals<Priority, "low" | "high">>();
  const doc = {} as Doc<"tasks">;
  assertType<Equals<typeof doc._id, Id<"tasks">>>();
  assertType<Equals<typeof doc._creationTime, number>>();
  assertType<Equals<typeof doc.labels, Array<Label>>>();
  assertType<Equals<typeof doc.parent, Id<"tasks"> | null>>();

  // Queries: optional args may be omitted; outputs are typed.
  const all = client.watchQuery(api.codegen.tasks.list);
  assertType<Equals<ReturnType<typeof all.localQueryResult>, Doc<"tasks">[] | undefined>>();
  client.watchQuery(api.codegen.tasks.list, { priority: "high" });
  client.watchQuery(api.codegen.tasks.list, {});
  client.watchQuery(api.codegen.tasks.list, { priority: null });
  // @ts-expect-error not a Priority
  client.watchQuery(api.codegen.tasks.list, { priority: "urgent" });
  // @ts-expect-error unknown arg
  client.watchQuery(api.codegen.tasks.list, { prio: "high" });

  const stats = await client.query(api.codegen.tasks.stats);
  const total: number = stats.total; // u64 is a number, not a bigint
  const top: Label | null = stats.topLabel;
  void total;
  void top;
  // @ts-expect-error zero-arg queries take no args
  client.watchQuery(api.codegen.tasks.stats, { x: 1 });

  const labels = await client.query(api.codegen.tasks.labels);
  assertType<Equals<(typeof labels)[string], Array<{ name: string; color: Label["color"] }>>>();

  // Mutations.
  const id = await client.mutation(api.codegen.tasks.add, {
    title: "t",
    priority: "low",
    labels: [{ name: "x", color: "Red" }, { name: "y", color: { Custom: { hex: "#fff" } } }],
  });
  assertType<Equals<typeof id, Id<"tasks">>>();
  await client.mutation(api.codegen.tasks.add, { title: "child", priority: "high", labels: [], parent: taskId });
  // @ts-expect-error missing required `title`
  await client.mutation(api.codegen.tasks.add, { priority: "low", labels: [] });
  // @ts-expect-error a mutation needs its args
  await client.mutation(api.codegen.tasks.add);
  // @ts-expect-error wrong table id
  await client.mutation(api.codegen.tasks.add, { title: "t", priority: "low", labels: [], parent: noteId });
  // @ts-expect-error not a color
  await client.mutation(api.codegen.tasks.add, { title: "t", priority: "low", labels: [{ name: "x", color: "Blue" }] });
  // @ts-expect-error a mutation is not a query
  client.watchQuery(api.codegen.tasks.add, { title: "t", priority: "low", labels: [] });

  // Zero-arg mutation and action.
  const n: number = await client.mutation(api.common.todos.noop);
  void n;
  await client.mutation(api.common.todos.noop, {});
  const pong: string = await client.action(api.codegen.tasks.ping);
  void pong;
  assertType<Equals<ArgsOf<typeof api.codegen.tasks.ping>, Record<string, never>>>();
  assertType<Equals<OutputOf<typeof api.common.todos.list>, Array<Doc<"todos">>>>();

  // Hand-expanded and real-macro modules.
  await client.mutation(api.common.todos.set_done, { id: {} as Id<"todos">, done: true });
  client.watchQuery(api.macros.notes.on_board, { board: "b" });
  // @ts-expect-error `board` is required
  client.watchQuery(api.macros.notes.on_board, { limit: 3 });

  // Pagination: `paginationOpts: PaginationOpts` in the args, `PaginationResult<Doc<"tasks">>` as the output.
  assertType<Equals<ArgsOf<typeof api.codegen.tasks.page>["paginationOpts"], PaginationOpts>>();
  assertType<Equals<OutputOf<typeof api.codegen.tasks.page>, PaginationResult<Doc<"tasks">>>>();
  assertType<Equals<PaginatedQueryItem<typeof api.codegen.tasks.page>, Doc<"tasks">>>();
  assertType<Equals<PaginatedQueryArgs<typeof api.codegen.tasks.page>, { priority?: Priority | null }>>();
  const onePage = await client.query(api.codegen.tasks.page, { paginationOpts: { numItems: 10, cursor: null } });
  const isDone: boolean = onePage.isDone;
  const next: string = onePage.continueCursor;
  void isDone;
  await client.query(api.codegen.tasks.page, { paginationOpts: { numItems: 10, cursor: next, endCursor: next } });
  const pages = client.paginatedQuery(api.codegen.tasks.page, { priority: "high" }, { initialNumItems: 10 });
  assertType<Equals<ReturnType<typeof pages.getCurrent>["results"], Doc<"tasks">[]>>();
  client.paginatedQuery(api.codegen.tasks.page, {}, { initialNumItems: 10 });
  // @ts-expect-error not a Priority
  client.paginatedQuery(api.codegen.tasks.page, { priority: "urgent" }, { initialNumItems: 10 });
  // @ts-expect-error not a paginated query
  client.paginatedQuery(api.codegen.tasks.list, {}, { initialNumItems: 10 });
  // @ts-expect-error paginationOpts is required when calling the query directly
  await client.query(api.codegen.tasks.page, {});

  // Internal functions are not in the api.
  // @ts-expect-error internal
  void api.codegen.tasks.purge;
  // @ts-expect-error internal
  void api.common.todos.count;
}

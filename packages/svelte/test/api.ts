import { makeFunctionReference, type Id, type PaginationOpts, type PaginationResult } from "@rxferrix/client";

export type Todo = { _id: Id<"todos">; text: string };
export type Message = { _id: Id<"messages">; body: string };

export const api = {
  todos: {
    list: makeFunctionReference<"query", Record<string, never>, Todo[]>("query", "todos:list"),
    get: makeFunctionReference<"query", { id: string }, Todo | null>("query", "todos:get"),
    add: makeFunctionReference<"mutation", { text: string }, Id<"todos">>("mutation", "todos:add"),
  },
  messages: {
    list: makeFunctionReference<
      "query",
      { channel: string; paginationOpts: PaginationOpts },
      PaginationResult<Message>
    >("query", "messages:list"),
  },
  ai: { run: makeFunctionReference<"action", { prompt: string }, string>("action", "ai:run") },
};

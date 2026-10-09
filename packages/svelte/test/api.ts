import { makeFunctionReference, type Id } from "@ferrix/client";

export type Todo = { _id: Id<"todos">; text: string };

export const api = {
  todos: {
    list: makeFunctionReference<"query", Record<string, never>, Todo[]>("query", "todos:list"),
    get: makeFunctionReference<"query", { id: string }, Todo | null>("query", "todos:get"),
    add: makeFunctionReference<"mutation", { text: string }, Id<"todos">>("mutation", "todos:add"),
  },
  ai: { run: makeFunctionReference<"action", { prompt: string }, string>("action", "ai:run") },
};

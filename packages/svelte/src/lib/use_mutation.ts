import type {
  ArgsOf,
  FerrixClient,
  FunctionReference,
  OptimisticUpdate,
  OptionalArgs,
  OutputOf,
} from "@ferrix/client";
import { getFerrixClient } from "./context.js";

/** A mutation bound to the client: call it with the mutation's args. */
export interface Mutation<M extends FunctionReference<"mutation">> {
  (...args: OptionalArgs<M>): Promise<OutputOf<M>>;
  /** Returns a new mutation function that applies `optimisticUpdate` until the server confirms or rejects it. */
  withOptimisticUpdate(optimisticUpdate: OptimisticUpdate<ArgsOf<M>>): Mutation<M>;
}

/** An action bound to the client: call it with the action's args. */
export type Action<A extends FunctionReference<"action">> = (...args: OptionalArgs<A>) => Promise<OutputOf<A>>;

function createMutation<M extends FunctionReference<"mutation">>(
  client: FerrixClient,
  ref: M,
  optimisticUpdate?: OptimisticUpdate<ArgsOf<M>>,
): Mutation<M> {
  const mutate = ((...args: OptionalArgs<M>) =>
    client.mutation(ref, ...([args[0], { optimisticUpdate }] as never))) as Mutation<M>;
  mutate.withOptimisticUpdate = (update) => {
    if (optimisticUpdate) throw new Error(`Mutation ${ref._path} already has an optimistic update`);
    return createMutation(client, ref, update);
  };
  return mutate;
}

/** Returns a function that runs the mutation. Call during component init (or pass `client`). */
export function useMutation<M extends FunctionReference<"mutation">>(
  ref: M,
  options: { client?: FerrixClient } = {},
): Mutation<M> {
  return createMutation(options.client ?? getFerrixClient(), ref);
}

/** Returns a function that runs the action. Call during component init (or pass `client`). */
export function useAction<A extends FunctionReference<"action">>(
  ref: A,
  options: { client?: FerrixClient } = {},
): Action<A> {
  const client = options.client ?? getFerrixClient();
  return ((...args: OptionalArgs<A>) => client.action(ref, ...args)) as Action<A>;
}

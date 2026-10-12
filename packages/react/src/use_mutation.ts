import { useMemo, useSyncExternalStore } from "react";
import type {
  ArgsOf,
  ConnectionState,
  RxFerrixClient,
  FunctionReference,
  OptimisticUpdate,
  OptionalArgs,
  OutputOf,
} from "@rxferrix/client";
import { useRxFerrix } from "./context.js";

/** A mutation bound to the client: call it with the mutation's args. */
export interface ReactMutation<M extends FunctionReference<"mutation">> {
  (...args: OptionalArgs<M>): Promise<OutputOf<M>>;
  /**
   * Returns a new mutation function that applies `optimisticUpdate` to local query results until the mutation's
   * effect is visible from the server (or it fails).
   */
  withOptimisticUpdate(optimisticUpdate: OptimisticUpdate<ArgsOf<M>>): ReactMutation<M>;
}

/** An action bound to the client: call it with the action's args. */
export type ReactAction<A extends FunctionReference<"action">> = (...args: OptionalArgs<A>) => Promise<OutputOf<A>>;

function createMutation<M extends FunctionReference<"mutation">>(
  client: RxFerrixClient,
  ref: M,
  optimisticUpdate?: OptimisticUpdate<ArgsOf<M>>,
): ReactMutation<M> {
  const mutate = ((...args: OptionalArgs<M>) =>
    client.mutation(ref, ...([args[0], { optimisticUpdate }] as never))) as ReactMutation<M>;
  mutate.withOptimisticUpdate = (update) => {
    if (optimisticUpdate) {
      throw new Error(`Mutation ${ref._path} already has an optimistic update`);
    }
    return createMutation(client, ref, update);
  };
  return mutate;
}

/**
 * Returns a stable function that runs the mutation. Its promise resolves once the result is visible in this
 * client's queries.
 *
 * ```tsx
 * const add = useMutation(api.todos.add).withOptimisticUpdate((store, args) => { ... });
 * await add({ text: "hi" });
 * ```
 */
export function useMutation<M extends FunctionReference<"mutation">>(ref: M): ReactMutation<M> {
  const client = useRxFerrix();
  const path = ref._path;
  // eslint-disable-next-line react-hooks/exhaustive-deps
  return useMemo(() => createMutation(client, ref), [client, path]);
}

/** Returns a stable function that runs the action. */
export function useAction<A extends FunctionReference<"action">>(ref: A): ReactAction<A> {
  const client = useRxFerrix();
  const path = ref._path;
  return useMemo(
    () => ((...args: OptionalArgs<A>) => client.action(ref, ...args)) as ReactAction<A>,
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [client, path],
  );
}

/** The client's connection state, re-rendering when it changes. */
export function useConnectionState(): ConnectionState {
  const client = useRxFerrix();
  const subscribe = useMemo(() => (cb: () => void) => client.subscribeToConnectionState(cb), [client]);
  const getSnapshot = useMemo(() => () => client.connectionState(), [client]);
  return useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
}

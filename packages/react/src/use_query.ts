import { useCallback, useMemo, useSyncExternalStore } from "react";
import {
  canonicalJSON,

  type RxFerrixError,
  type FunctionReference,
  type OptionalArgsOrSkip,
  type OutputOf,
  type QueryState,
  type QueryWatch,
} from "@rxferrix/client";
import { useRxFerrix } from "./context.js";

const SKIPPED = Object.freeze({ status: "skipped" as const });
type Snapshot = QueryState | typeof SKIPPED;

const noopUnsubscribe = () => {};

/** Subscribes to a query and returns its current state (or `SKIPPED`). Shared by the public hooks. */
function useQueryState(ref: FunctionReference<"query">, argsOrSkip: unknown): Snapshot {
  const client = useRxFerrix();
  const skip = argsOrSkip === "skip";
  // Args are compared by canonical JSON so `useQuery(ref, { a: 1 })` with an inline object does not resubscribe.
  const argsKey = skip ? null : canonicalJSON(argsOrSkip ?? {});
  const path = ref._path;

  const watch: QueryWatch<unknown> | null = useMemo(
    () =>
      argsKey === null
        ? null
        : // The args type is enforced by the public hook signatures.
          client.watchQuery(ref, argsOrSkip as never),
    // Keyed by the function path and canonical args, not by object identity: inline `{ ... }` args and
    // re-created references do not resubscribe.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [client, path, argsKey],
  );

  const subscribe = useCallback(
    (onStoreChange: () => void) => (watch ? watch.onUpdate(onStoreChange) : noopUnsubscribe),
    [watch],
  );
  const getSnapshot = useCallback((): Snapshot => (watch ? watch.localQueryState() : SKIPPED), [watch]);
  return useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
}

/**
 * Subscribes to a query. Returns `undefined` while loading (or when skipped with `"skip"`), then the latest value,
 * re-rendering whenever it changes. Throws the query's {@link RxFerrixError} if it fails, so an ErrorBoundary can
 * render it.
 *
 * ```tsx
 * const todos = useQuery(api.todos.list, { done: false });
 * const user = useQuery(api.users.get, userId ? { id: userId } : "skip");
 * ```
 */
export function useQuery<Q extends FunctionReference<"query">>(
  ref: Q,
  ...args: OptionalArgsOrSkip<Q>
): OutputOf<Q> | undefined {
  const state = useQueryState(ref, args[0]);
  if (state.status === "error") throw state.error;
  return state.status === "success" ? (state.value as OutputOf<Q>) : undefined;
}

export type UseQueryResult<T> =
  | { data: undefined; error: undefined; isLoading: true }
  | { data: T; error: undefined; isLoading: false }
  | { data: undefined; error: RxFerrixError; isLoading: false }
  /** Skipped. */
  | { data: undefined; error: undefined; isLoading: false };

/**
 * Like {@link useQuery} but returns `{ data, error, isLoading }` instead of throwing. The returned object is stable
 * while the query's state does not change.
 */
export function useQueryWithStatus<Q extends FunctionReference<"query">>(
  ref: Q,
  ...args: OptionalArgsOrSkip<Q>
): UseQueryResult<OutputOf<Q>> {
  const state = useQueryState(ref, args[0]);
  return useMemo((): UseQueryResult<OutputOf<Q>> => {
    switch (state.status) {
      case "loading":
        return { data: undefined, error: undefined, isLoading: true };
      case "success":
        return { data: state.value as OutputOf<Q>, error: undefined, isLoading: false };
      case "error":
        return { data: undefined, error: state.error, isLoading: false };
      default:
        return { data: undefined, error: undefined, isLoading: false };
    }
  }, [state]);
}


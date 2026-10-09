import { useCallback, useMemo, useSyncExternalStore } from "react";
import {
  canonicalJSON,
  type PaginatedQuery,
  type PaginatedQueryArgs,
  type PaginatedQueryItem,
  type PaginatedQueryReference,
  type PaginatedQueryResult,
  type PaginationStatus,
} from "@ferrix/client";
import { useFerrix } from "./context.js";

export interface UsePaginatedQueryResult<Item> {
  /** The items of every loaded page, in order. */
  results: Item[];
  status: PaginationStatus;
  /** True while the first page or a further page is loading. */
  isLoading: boolean;
  /** Loads `numItems` more items; a no-op unless `status` is `"CanLoadMore"`. */
  loadMore: (numItems: number) => void;
}

export interface UsePaginatedQueryOptions {
  /** The size of the first page. */
  initialNumItems: number;
}

const SKIPPED: PaginatedQueryResult<never> = Object.freeze({
  results: [],
  status: "LoadingFirstPage" as const,
  isLoading: true,
  error: undefined,
  loadMore: () => false,
});

const noopUnsubscribe = () => {};

/**
 * Subscribes to a paginated query: a query that takes `paginationOpts` and returns a `PaginationResult` (in Rust,
 * `.paginate(pagination_opts)`). Pass the args without `paginationOpts`, or `"skip"`. Changing the args (or
 * `initialNumItems`) starts over from the first page. Throws the {@link FerrixError} of a failed page, so an
 * ErrorBoundary can render it.
 *
 * ```tsx
 * const { results, status, loadMore } = usePaginatedQuery(api.messages.list, { channel }, { initialNumItems: 20 });
 * <button disabled={status !== "CanLoadMore"} onClick={() => loadMore(20)}>Load more</button>
 * ```
 */
export function usePaginatedQuery<Q extends PaginatedQueryReference>(
  ref: Q,
  args: PaginatedQueryArgs<Q> | "skip",
  options: UsePaginatedQueryOptions,
): UsePaginatedQueryResult<PaginatedQueryItem<Q>> {
  const client = useFerrix();
  const argsKey = args === "skip" ? null : canonicalJSON(args ?? {});
  const path = ref._path;
  const initialNumItems = options.initialNumItems;

  const query: PaginatedQuery<PaginatedQueryItem<Q>> | null = useMemo(
    () =>
      argsKey === null
        ? null
        : client.paginatedQuery(ref, JSON.parse(argsKey) as PaginatedQueryArgs<Q>, { initialNumItems }),
    // Keyed by the function path and canonical args, like useQuery: inline args objects do not start over.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [client, path, argsKey, initialNumItems],
  );

  const subscribe = useCallback(
    (onStoreChange: () => void) => (query ? query.subscribe(onStoreChange) : noopUnsubscribe),
    [query],
  );
  const getSnapshot = useCallback(
    (): PaginatedQueryResult<PaginatedQueryItem<Q>> => (query ? query.getCurrent() : SKIPPED),
    [query],
  );
  const current = useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
  if (current.error !== undefined) throw current.error;

  return useMemo(
    () => ({
      results: current.results,
      status: current.status,
      isLoading: current.isLoading,
      loadMore: (numItems: number) => {
        current.loadMore(numItems);
      },
    }),
    [current],
  );
}

import { useCallback, useMemo, useSyncExternalStore } from "react";
import {
  canonicalJSON,
  type RxFerrixError,
  type PaginatedQuery,
  type PaginatedQueryArgs,
  type PaginatedQueryItem,
  type PaginatedQueryReference,
  type PaginatedQueryResult,
  type PaginationStatus,
} from "@rxferrix/client";
import { useRxFerrix } from "./context.js";

export interface UsePaginatedQueryResult<Item> {
  /** The items of every loaded page, in order. */
  results: Item[];
  status: PaginationStatus;
  /** True while the first page or a further page is loading. */
  isLoading: boolean;
  /** Loads `numItems` more items; a no-op unless `status` is `"CanLoadMore"`. */
  loadMore: (numItems: number) => void;
}

/** The result of {@link usePaginatedQueryWithStatus}: {@link UsePaginatedQueryResult} plus the error. */
export interface UsePaginatedQueryWithStatusResult<Item> extends UsePaginatedQueryResult<Item> {
  /** The error of the first failed page, if any (then `isLoading` is false). */
  error: RxFerrixError | undefined;
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
 * `initialNumItems`) starts over from the first page. Throws the {@link RxFerrixError} of a failed page, so an
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
  const { error, ...result } = usePaginatedQueryWithStatus(ref, args, options);
  if (error !== undefined) throw error;
  return result;
}

/**
 * Like {@link usePaginatedQuery} but returns the error of a failed page as `error` instead of throwing it, so a
 * component can show it in place (and keep the pages that did load in `results`).
 *
 * ```tsx
 * const { results, status, error, loadMore } = usePaginatedQueryWithStatus(api.messages.list, { channel }, {
 *   initialNumItems: 20,
 * });
 * if (error) return <AccessError error={error} />;
 * ```
 */
export function usePaginatedQueryWithStatus<Q extends PaginatedQueryReference>(
  ref: Q,
  args: PaginatedQueryArgs<Q> | "skip",
  options: UsePaginatedQueryOptions,
): UsePaginatedQueryWithStatusResult<PaginatedQueryItem<Q>> {
  const client = useRxFerrix();
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

  return useMemo(
    () => ({
      results: current.results,
      status: current.status,
      isLoading: current.isLoading,
      error: current.error,
      loadMore: (numItems: number) => {
        current.loadMore(numItems);
      },
    }),
    [current],
  );
}

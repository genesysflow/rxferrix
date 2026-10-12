import { createSubscriber } from "svelte/reactivity";
import {
  canonicalJSON,
  type RxFerrixClient,
  type RxFerrixError,
  type PaginatedQueryArgs,
  type PaginatedQueryItem,
  type PaginatedQueryReference,
  type PaginatedQueryResult,
  type PaginationStatus,
} from "@rxferrix/client";
import { getRxFerrixClient } from "./context.js";

/** The reactive result of {@link usePaginatedQuery}. Read its fields in templates, `$derived` or `$effect`. */
export interface UsePaginatedQueryResult<Item> {
  /** The items of every loaded page, in order. */
  readonly results: Item[];
  readonly status: PaginationStatus;
  /** True while the first page or a further page is loading. */
  readonly isLoading: boolean;
  /** The error of a failed page, if any. */
  readonly error: RxFerrixError | undefined;
  /** Loads `numItems` more items; a no-op unless `status` is `"CanLoadMore"`. */
  loadMore(numItems: number): void;
}

export interface UsePaginatedQueryOptions {
  /** The size of the first page. */
  initialNumItems: number;
  /** Use this client instead of the one from context. */
  client?: RxFerrixClient;
}

const SKIPPED: PaginatedQueryResult<never> = Object.freeze({
  results: [],
  status: "LoadingFirstPage" as const,
  isLoading: true,
  error: undefined,
  loadMore: () => false,
});

/**
 * Subscribes to a paginated query: a query that takes `paginationOpts` and returns a `PaginationResult` (in Rust,
 * `.paginate(pagination_opts)`).
 *
 * ```svelte
 * <script lang="ts">
 *   let { channel } = $props();
 *   const messages = usePaginatedQuery(api.messages.list, () => ({ channel }), { initialNumItems: 20 });
 * </script>
 * {#each messages.results as m (m._id)}<p>{m.body}</p>{/each}
 * <button disabled={messages.status !== "CanLoadMore"} onclick={() => messages.loadMore(20)}>More</button>
 * ```
 *
 * The args thunk (without `paginationOpts`) is tracked: when its value changes (by canonical JSON), the list starts
 * over from the first page. Return `"skip"` to not subscribe. Pages are subscribed while the result is read by at
 * least one effect (e.g. the template).
 */
export function usePaginatedQuery<Q extends PaginatedQueryReference>(
  ref: Q,
  args: () => PaginatedQueryArgs<Q> | "skip",
  options: UsePaginatedQueryOptions,
): UsePaginatedQueryResult<PaginatedQueryItem<Q>> {
  const client = options.client ?? getRxFerrixClient();
  const initialNumItems = options.initialNumItems;

  const argsKey = $derived.by(() => {
    const a = args();
    return a === "skip" ? null : canonicalJSON(a ?? {});
  });

  // Creating a paginated query has no side effects; its pages are subscribed only by `subscribe()` in an effect.
  const query = $derived(
    argsKey === null
      ? null
      : client.paginatedQuery(ref, JSON.parse(argsKey) as PaginatedQueryArgs<Q>, { initialNumItems }),
  );

  const subscribe = $derived.by(() => {
    const q = query;
    return q ? createSubscriber((update) => q.subscribe(update)) : null;
  });

  const current = (): PaginatedQueryResult<PaginatedQueryItem<Q>> => {
    const s = subscribe;
    const q = query;
    s?.();
    return q ? q.getCurrent() : SKIPPED;
  };

  return {
    get results() {
      return current().results;
    },
    get status() {
      return current().status;
    },
    get isLoading() {
      return current().isLoading;
    },
    get error() {
      return current().error;
    },
    loadMore(numItems: number) {
      current().loadMore(numItems);
    },
  };
}

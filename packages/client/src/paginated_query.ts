/**
 * Reactive pagination on top of {@link RxFerrixClient.watchQuery}.
 *
 * A paginated query is a server query that takes `paginationOpts: PaginationOpts` and returns a
 * `PaginationResult<Item>` (in Rust: `Query::paginate`). The client subscribes to it once per loaded page:
 *
 * - Page 0 is `{ ...args, paginationOpts: { numItems: initialNumItems, cursor: null } }`.
 * - `loadMore(n)` subscribes to the next page, starting at the last page's `continueCursor`.
 * - When a page gets a successor, it is re-subscribed with `endCursor` pinned to the cursor the successor starts at.
 *   A pinned page covers a fixed span of the index, so inserts and deletes make it grow or shrink instead of
 *   shifting documents into (or out of) its neighbours: no duplicates or gaps between pages. The unpinned
 *   subscription stays visible until the pinned one has a result, so the swap does not flash a loading state.
 *
 * Only the last page is unpinned; it grows up to its `numItems` when documents are added after it.
 */
import type { ArgsOf, FunctionReference, OutputOf } from "./api.js";
import { jsonEqual, normalizeArgs } from "./canonical.js";
import type { RxFerrixClient, QueryWatch } from "./client.js";
import type { RxFerrixError } from "./errors.js";
import type { OptimisticLocalStore } from "./optimistic_updates.js";
import type { QueryState } from "./query_state.js";

/** Which page to read: the `paginationOpts` argument of a paginated query (Rust `rxferrix::PaginationOpts`). */
export interface PaginationOpts {
  /** How many items to return (ignored when `endCursor` is set). */
  numItems: number;
  /** Where the page starts: `null` for the start, otherwise a previous page's `continueCursor`. */
  cursor: string | null;
  /** Where the page ends. When set, the page holds every item between `cursor` and `endCursor`. */
  endCursor?: string | null;
}

/** One page of results (Rust `rxferrix::PaginationResult<T>`). */
export interface PaginationResult<T> {
  page: T[];
  /** True when there is nothing after this page. */
  isDone: boolean;
  /** Where the next page starts. */
  continueCursor: string;
}

/** A reference to a query whose args include `paginationOpts` and whose output is a {@link PaginationResult}. */
export type PaginatedQueryReference<Item = any> = FunctionReference<
  "query",
  { paginationOpts: PaginationOpts },
  PaginationResult<Item>
>;

/** The item type of a paginated query. */
export type PaginatedQueryItem<Q> = OutputOf<Q> extends PaginationResult<infer Item> ? Item : never;

/** The args of a paginated query without `paginationOpts` (the client manages those). */
export type PaginatedQueryArgs<Q> = Omit<ArgsOf<Q>, "paginationOpts">;

export type PaginationStatus = "LoadingFirstPage" | "CanLoadMore" | "LoadingMore" | "Exhausted";

export interface PaginatedQueryResult<Item> {
  /** The items of every loaded page, in order. */
  readonly results: Item[];
  readonly status: PaginationStatus;
  /** True while the first page or a further page is loading (and no page failed). */
  readonly isLoading: boolean;
  /** The error of the first failed page, if any. A failed page counts as not loaded for `status`. */
  readonly error: RxFerrixError | undefined;
  /**
   * Loads `numItems` more items. Does nothing (and returns false) unless the last page is loaded and not done
   * (status `"CanLoadMore"`).
   */
  loadMore(numItems: number): boolean;
}

export interface PaginatedQueryOptions {
  /** The size of the first page. */
  initialNumItems: number;
}

/** A framework-agnostic paginated query. Pages are subscribed while at least one `subscribe` callback is registered. */
export interface PaginatedQuery<Item> {
  /** Registers `callback` to be called whenever {@link getCurrent} changes. Returns the unsubscribe function. */
  subscribe(callback: () => void): () => void;
  /** The current results. The object is referentially stable until something visible changes. */
  getCurrent(): PaginatedQueryResult<Item>;
}

interface PageSub<Item> {
  readonly opts: PaginationOpts;
  readonly watch: QueryWatch<PaginationResult<Item>>;
  /** Set while the subscription is active (the paginated query has listeners). */
  unsubscribe: (() => void) | null;
}

interface Page<Item> {
  current: PageSub<Item>;
  /** The pinned replacement of `current`, until it has a result. */
  pending: PageSub<Item> | null;
}

function validateNumItems(numItems: number, what: string): void {
  if (!Number.isInteger(numItems) || numItems < 1) {
    throw new Error(`${what} must be a positive integer, got ${numItems}`);
  }
}

/** @internal Created by {@link RxFerrixClient.paginatedQuery}. */
export class PaginatedQueryImpl<Item> implements PaginatedQuery<Item> {
  private readonly pages: Page<Item>[] = [];
  private readonly listeners = new Set<() => void>();
  private readonly args: Record<string, unknown>;
  private snapshot: PaginatedQueryResult<Item> | null = null;
  /** The page states `snapshot` was built from. */
  private snapshotStates: QueryState[] = [];

  constructor(
    private readonly client: RxFerrixClient,
    private readonly ref: FunctionReference<"query">,
    args: Record<string, unknown>,
    options: PaginatedQueryOptions,
  ) {
    validateNumItems(options?.initialNumItems, "initialNumItems");
    const { paginationOpts: _ignored, ...rest } = normalizeArgs(args);
    this.args = rest;
    this.pages.push({ current: this.makeSub({ numItems: options.initialNumItems, cursor: null }), pending: null });
  }

  subscribe(callback: () => void): () => void {
    const entry = () => callback();
    this.listeners.add(entry);
    if (this.listeners.size === 1) {
      for (const page of this.pages) {
        this.activate(page.current);
        if (page.pending) this.activate(page.pending);
      }
    }
    let active = true;
    return () => {
      if (!active) return;
      active = false;
      this.listeners.delete(entry);
      if (this.listeners.size === 0) {
        for (const page of this.pages) {
          this.deactivate(page.current);
          if (page.pending) this.deactivate(page.pending);
        }
      }
    };
  }

  getCurrent(): PaginatedQueryResult<Item> {
    const states = this.pages.map((p) => this.displayed(p));
    if (this.snapshot && states.length === this.snapshotStates.length && states.every((s, i) => s === this.snapshotStates[i])) {
      return this.snapshot;
    }
    let results: Item[] = [];
    let loading = false;
    let error: RxFerrixError | undefined;
    for (const state of states) {
      if (state.status === "success") results = results.concat(state.value.page);
      else if (state.status === "loading") loading = true;
      else error ??= state.error;
    }
    const last = states[states.length - 1];
    let status: PaginationStatus;
    if (loading || error !== undefined) {
      status = results.length === 0 && states[0]?.status !== "success" ? "LoadingFirstPage" : "LoadingMore";
    } else if (last?.status === "success" && last.value.isDone) {
      status = "Exhausted";
    } else {
      status = "CanLoadMore";
    }
    this.snapshotStates = states;
    const previous = this.snapshot;
    // E.g. a page swapped for its pinned copy: new state objects, same items. Keep the snapshot (no re-render).
    if (previous && previous.status === status && previous.error === error && jsonEqual(previous.results, results)) {
      return previous;
    }
    this.snapshot = {
      results,
      status,
      isLoading: error === undefined && (status === "LoadingFirstPage" || status === "LoadingMore"),
      error,
      loadMore: (numItems: number) => this.loadMore(numItems),
    };
    return this.snapshot;
  }

  private loadMore(numItems: number): boolean {
    validateNumItems(numItems, "loadMore(numItems)");
    const last = this.pages[this.pages.length - 1];
    if (!last) return false;
    const state = this.displayed(last);
    if (state.status !== "success" || state.value.isDone) return false;
    const cursor = state.value.continueCursor;
    // Pin the page to where its successor starts (it covers the same span, so it shows the same items).
    const shown = last.pending && last.pending.watch.localQueryState().status !== "loading" ? last.pending : last.current;
    if (shown.opts.endCursor == null) {
      if (last.pending) this.deactivate(last.pending);
      last.pending = this.makeSub({ ...shown.opts, endCursor: cursor });
      if (this.listeners.size > 0) this.activate(last.pending);
    }
    const next: Page<Item> = { current: this.makeSub({ numItems, cursor }), pending: null };
    this.pages.push(next);
    if (this.listeners.size > 0) this.activate(next.current);
    this.notify();
    return true;
  }

  /** The state a page shows: its pinned replacement once that has a result, otherwise the current subscription. */
  private displayed(page: Page<Item>): QueryState<PaginationResult<Item>> {
    if (page.pending) {
      const pending = page.pending.watch.localQueryState();
      if (pending.status !== "loading") return pending;
    }
    return page.current.watch.localQueryState();
  }

  private makeSub(opts: PaginationOpts): PageSub<Item> {
    const args = { ...this.args, paginationOpts: opts };
    const watch = this.client.watchQuery(this.ref, args as never) as QueryWatch<PaginationResult<Item>>;
    return { opts, watch, unsubscribe: null };
  }

  private activate(sub: PageSub<Item>): void {
    if (!sub.unsubscribe) sub.unsubscribe = sub.watch.onUpdate(() => this.onPageUpdate());
  }

  private deactivate(sub: PageSub<Item>): void {
    sub.unsubscribe?.();
    sub.unsubscribe = null;
  }

  private onPageUpdate(): void {
    // Complete the swaps whose pinned page has a result. Done here (not in getCurrent) to keep reads side-effect free.
    for (const page of this.pages) {
      if (page.pending && page.pending.watch.localQueryState().status !== "loading") {
        this.deactivate(page.current);
        page.current = page.pending;
        page.pending = null;
      }
    }
    this.notify();
  }

  private notify(): void {
    const before = this.snapshot;
    if (this.getCurrent() === before) return;
    for (const listener of [...this.listeners]) listener();
  }
}

// ---------------------------------------------------------------------------------------------------------------------
// Optimistic update helpers

/** Does every key of `argsToMatch` equal the corresponding arg of a page? */
function argsMatch(args: Record<string, unknown>, argsToMatch: Record<string, unknown> | undefined): boolean {
  if (!argsToMatch) return true;
  return Object.entries(argsToMatch).every(([k, v]) => v === undefined || jsonEqual(args[k], v));
}

/**
 * Every loaded page of a paginated query in the optimistic store, with its args (including `paginationOpts`).
 * Pass `argsToMatch` (args without `paginationOpts`, or a subset of them) to keep only one paginated query's pages.
 */
export function getAllPages<Q extends PaginatedQueryReference>(
  localStore: OptimisticLocalStore,
  ref: Q,
  argsToMatch?: Partial<PaginatedQueryArgs<Q>>,
): { args: ArgsOf<Q>; value: PaginationResult<PaginatedQueryItem<Q>> }[] {
  const out: { args: ArgsOf<Q>; value: PaginationResult<PaginatedQueryItem<Q>> }[] = [];
  for (const { args, value } of localStore.getAllQueries(ref)) {
    if (value === undefined || !argsMatch(args as Record<string, unknown>, argsToMatch as Record<string, unknown>)) continue;
    out.push({ args, value: value as PaginationResult<PaginatedQueryItem<Q>> });
  }
  return out;
}

/**
 * Optimistically inserts `item` at the top of the first page of a paginated query (every paginated query of `ref`
 * whose args match `argsToMatch`). Typical for "newest first" lists:
 *
 * ```ts
 * const send = useMutation(api.messages.send).withOptimisticUpdate((store, args) => {
 *   insertAtTop({ paginatedQuery: api.messages.list, argsToMatch: { channel: args.channel }, localQueryStore: store,
 *     item: { _id: "optimistic" as Id<"messages">, _creationTime: Date.now(), ...args } });
 * });
 * ```
 */
export function insertAtTop<Q extends PaginatedQueryReference>(options: {
  paginatedQuery: Q;
  argsToMatch?: Partial<PaginatedQueryArgs<Q>>;
  localQueryStore: OptimisticLocalStore;
  item: PaginatedQueryItem<Q>;
}): void {
  const { paginatedQuery, argsToMatch, localQueryStore, item } = options;
  for (const { args, value } of getAllPages(localQueryStore, paginatedQuery, argsToMatch)) {
    const opts = (args as { paginationOpts?: PaginationOpts }).paginationOpts;
    if (opts?.cursor !== null) continue;
    localQueryStore.setQuery(paginatedQuery, args, { ...value, page: [item, ...value.page] } as OutputOf<Q>);
  }
}

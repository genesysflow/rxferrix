import type { ArgsOf, FunctionReference, OutputOf } from "./api.js";
import { jsonEqual, normalizeArgs, queryToken, type QueryToken } from "./canonical.js";
import type { Logger } from "./logging.js";
import type { RequestId } from "./protocol.js";
import { LOADING, type QueryState } from "./query_state.js";

/**
 * A view of the client's query results that optimistic updates can read and write.
 */
export interface OptimisticLocalStore {
  /** The current value of a query, or undefined if it is loading, failed, or not subscribed. */
  getQuery<Q extends FunctionReference<"query">>(ref: Q, args?: ArgsOf<Q>): OutputOf<Q> | undefined;
  /** Overrides a query's value; `undefined` makes it look like it is loading. */
  setQuery<Q extends FunctionReference<"query">>(ref: Q, args: ArgsOf<Q>, value: OutputOf<Q> | undefined): void;
  /** Every query of this function the client knows about, with its args and current value. */
  getAllQueries<Q extends FunctionReference<"query">>(ref: Q): { args: ArgsOf<Q>; value: OutputOf<Q> | undefined }[];
}

/** A function that optimistically applies a mutation's effect to the local store. */
export type OptimisticUpdate<Args> = (localStore: OptimisticLocalStore, args: Args) => void;

export interface ViewEntry {
  readonly udfPath: string;
  readonly args: Record<string, unknown>;
  readonly state: QueryState;
}

export type ViewMap = Map<QueryToken, ViewEntry>;

class LocalStoreImpl implements OptimisticLocalStore {
  constructor(private readonly view: ViewMap) {}

  getQuery<Q extends FunctionReference<"query">>(ref: Q, args?: ArgsOf<Q>): OutputOf<Q> | undefined {
    const entry = this.view.get(queryToken(ref._path, normalizeArgs(args)));
    return entry?.state.status === "success" ? (entry.state.value as OutputOf<Q>) : undefined;
  }

  setQuery<Q extends FunctionReference<"query">>(ref: Q, args: ArgsOf<Q>, value: OutputOf<Q> | undefined): void {
    const normalized = normalizeArgs(args);
    this.view.set(queryToken(ref._path, normalized), {
      udfPath: ref._path,
      args: normalized,
      state: value === undefined ? LOADING : { status: "success", value },
    });
  }

  getAllQueries<Q extends FunctionReference<"query">>(ref: Q): { args: ArgsOf<Q>; value: OutputOf<Q> | undefined }[] {
    const out: { args: ArgsOf<Q>; value: OutputOf<Q> | undefined }[] = [];
    for (const entry of this.view.values()) {
      if (entry.udfPath !== ref._path) continue;
      out.push({
        args: entry.args as ArgsOf<Q>,
        value: entry.state.status === "success" ? (entry.state.value as OutputOf<Q>) : undefined,
      });
    }
    return out;
  }
}

/**
 * Server query results with a stack of optimistic updates layered on top.
 *
 * Whenever the server state changes, the view is rebuilt from the server results and every pending update is
 * replayed in the order the mutations were issued. An update is dropped when its mutation completes (its ts was
 * observed) or fails. The view keeps the previous state object for a query when its visible value did not change, so
 * watchers are only notified (and snapshots only change) on real changes.
 */
export class OptimisticQueryResults {
  private view: ViewMap = new Map();
  private updates: { requestId: RequestId; apply: (store: OptimisticLocalStore) => void }[] = [];

  constructor(private readonly logger: Logger) {}

  get(token: QueryToken): QueryState {
    return this.view.get(token)?.state ?? LOADING;
  }

  addUpdate(requestId: RequestId, apply: (store: OptimisticLocalStore) => void): void {
    this.updates.push({ requestId, apply });
  }

  dropUpdates(requestIds: Iterable<RequestId>): boolean {
    const drop = new Set(requestIds);
    if (drop.size === 0) return false;
    const before = this.updates.length;
    this.updates = this.updates.filter((u) => !drop.has(u.requestId));
    return this.updates.length !== before;
  }

  hasUpdates(): boolean {
    return this.updates.length > 0;
  }

  /** Rebuilds the view from `server` plus all optimistic updates; returns the tokens whose visible state changed. */
  recompute(server: ViewMap): QueryToken[] {
    const next: ViewMap = new Map(server);
    const store = new LocalStoreImpl(next);
    for (const update of this.updates) {
      try {
        update.apply(store);
      } catch (e) {
        this.logger.error("optimistic update threw; ignoring it", e);
      }
    }

    const changed: QueryToken[] = [];
    for (const [token, entry] of next) {
      const previous = this.view.get(token)?.state ?? LOADING;
      const stable = stableState(previous, entry.state);
      if (stable !== entry.state) next.set(token, { ...entry, state: stable });
      if (stable !== previous) changed.push(token);
    }
    for (const [token, entry] of this.view) {
      if (!next.has(token) && entry.state !== LOADING) changed.push(token);
    }
    this.view = next;
    return changed;
  }
}

/** Returns `previous` if `next` shows the same thing, otherwise `next`. */
function stableState(previous: QueryState, next: QueryState): QueryState {
  if (previous === next) return previous;
  if (previous.status !== next.status) return next;
  switch (next.status) {
    case "loading":
      return previous;
    case "success":
      return jsonEqual((previous as { value: unknown }).value, next.value) ? previous : next;
    case "error": {
      const p = (previous as { error: { message: string; code?: string; data?: unknown } }).error;
      return p.message === next.error.message && p.code === next.error.code && jsonEqual(p.data, next.error.data)
        ? previous
        : next;
    }
  }
}

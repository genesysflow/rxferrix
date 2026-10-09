import { queryToken, type QueryToken } from "./canonical.js";
import type { ModifyQuerySetMessage, QueryId, QuerySetModification } from "./protocol.js";

export interface LocalQuery {
  readonly token: QueryToken;
  readonly queryId: QueryId;
  readonly udfPath: string;
  readonly args: Record<string, unknown>;
  refCount: number;
  /** Whether an `Add` for this query is part of the server's query set on the current socket. */
  onServer: boolean;
}

export interface FlushResult {
  /** The message to send, or null when nothing changed (or nothing can be sent right now). */
  message: ModifyQuerySetMessage | null;
  /** Queries dropped locally by this flush (their last watcher left). Their results should be discarded. */
  removed: QueryId[];
}

/**
 * The set of queries this client wants to be subscribed to.
 *
 * Queries are keyed by `udfPath + canonicalJSON(args)`. Watchers of the same query share one queryId and one server
 * subscription; the subscription is dropped when the last watcher leaves. Changes are not sent eagerly: they are
 * diffed against what the server knows on flush, so an add and a remove of the same query in the same tick cancel
 * out and several changes produce a single `ModifyQuerySet`.
 */
export class LocalQuerySet {
  private nextQueryId = 0;
  private readonly byToken = new Map<QueryToken, LocalQuery>();
  private readonly byId = new Map<QueryId, LocalQuery>();
  /** Queries dropped locally whose `Remove` still has to reach the server on this socket. */
  private readonly pendingRemoves = new Set<QueryId>();
  /** The `newVersion` of the last ModifyQuerySet sent on this socket. */
  private querySetVersion = 0;

  /** Adds a watcher; returns the query (created if needed) and whether a flush is needed. */
  addRef(udfPath: string, args: Record<string, unknown>): { query: LocalQuery; needsFlush: boolean } {
    const token = queryToken(udfPath, args);
    const existing = this.byToken.get(token);
    if (existing) {
      existing.refCount += 1;
      // refCount went 0 -> 1 before the flush: the query is revived without any server traffic.
      return { query: existing, needsFlush: false };
    }
    const query: LocalQuery = {
      token,
      queryId: this.nextQueryId++,
      udfPath,
      args,
      refCount: 1,
      onServer: false,
    };
    this.byToken.set(token, query);
    this.byId.set(query.queryId, query);
    return { query, needsFlush: true };
  }

  /** Removes a watcher; returns true when the query has no watchers left (a flush should be scheduled). */
  removeRef(token: QueryToken): boolean {
    const query = this.byToken.get(token);
    if (!query) return false;
    query.refCount = Math.max(0, query.refCount - 1);
    return query.refCount === 0;
  }

  get(token: QueryToken): LocalQuery | undefined {
    return this.byToken.get(token);
  }

  getById(queryId: QueryId): LocalQuery | undefined {
    return this.byId.get(queryId);
  }

  /** All queries currently tracked (including ones whose last watcher left but that are not yet flushed). */
  queries(): IterableIterator<LocalQuery> {
    return this.byToken.values();
  }

  get size(): number {
    return this.byToken.size;
  }

  /**
   * Drops queries without watchers and, if `canSend`, builds the ModifyQuerySet that brings the server's query set
   * in line with the local one (bumping the query set version).
   */
  flush(canSend: boolean): FlushResult {
    const removed: QueryId[] = [];
    for (const query of [...this.byToken.values()]) {
      if (query.refCount > 0) continue;
      this.byToken.delete(query.token);
      this.byId.delete(query.queryId);
      removed.push(query.queryId);
      if (query.onServer) this.pendingRemoves.add(query.queryId);
    }
    if (!canSend) return { message: null, removed };

    const modifications: QuerySetModification[] = [];
    for (const query of this.byToken.values()) {
      if (!query.onServer) {
        modifications.push({ type: "Add", queryId: query.queryId, udfPath: query.udfPath, args: query.args });
        query.onServer = true;
      }
    }
    for (const queryId of this.pendingRemoves) modifications.push({ type: "Remove", queryId });
    this.pendingRemoves.clear();
    if (modifications.length === 0) return { message: null, removed };

    const baseVersion = this.querySetVersion;
    this.querySetVersion += 1;
    return {
      message: { type: "ModifyQuerySet", baseVersion, newVersion: this.querySetVersion, modifications },
      removed,
    };
  }

  /** The server state is fresh on a new socket: nothing is subscribed and the query set version is 0. */
  resetForNewSocket(): void {
    this.querySetVersion = 0;
    this.pendingRemoves.clear();
    for (const query of this.byToken.values()) query.onServer = false;
  }

  /** The `newVersion` of the last ModifyQuerySet sent on this socket. */
  get sentQuerySetVersion(): number {
    return this.querySetVersion;
  }
}

import { queryToken, type QueryToken } from "./canonical.js";
import { MODIFY_QUERY_SET_CHARS, REMOVE_CHARS, utf8Length } from "./limits.js";
import type { ModifyQuerySetMessage, QueryId, QuerySetModification } from "./protocol.js";

export interface LocalQuery {
  readonly token: QueryToken;
  readonly queryId: QueryId;
  readonly udfPath: string;
  readonly args: Record<string, unknown>;
  refCount: number;
  /** Whether an `Add` for this query is part of the server's query set on the current socket. */
  onServer: boolean;
  /** About what its arguments count for on the server ({@link LocalQuerySet.argsBytes}). */
  readonly bytes: number;
}

export interface FlushResult {
  /**
   * The messages to send, in order; none when nothing changed (or nothing can be sent right now). Usually one. A
   * query set too large for one message (it is sent whole on every new socket) is split into several, each one
   * version after the last.
   */
  messages: ModifyQuerySetMessage[];
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
  /** The sum of {@link LocalQuery.bytes} over the queries that have watchers. */
  private bytes = 0;

  /**
   * About what the server counts for the arguments of the watched queries (it caps their JSON at 8 MiB a socket).
   * An upper bound: each query counts with its token, which holds its path as well. A query whose last watcher
   * left no longer counts, though it is tracked until the next flush: that flush removes it before it adds anything.
   */
  get argsBytes(): number {
    return this.bytes;
  }

  /** What watching the query `token` would add to {@link argsBytes}; nothing if it has watchers already. */
  costOf(token: QueryToken): number {
    const tracked = this.byToken.get(token);
    if (!tracked) return utf8Length(token);
    return tracked.refCount > 0 ? 0 : tracked.bytes;
  }

  /** Adds a watcher; returns the query (created if needed) and whether a flush is needed. */
  addRef(udfPath: string, args: Record<string, unknown>): { query: LocalQuery; needsFlush: boolean } {
    const token = queryToken(udfPath, args);
    const existing = this.byToken.get(token);
    if (existing) {
      if (existing.refCount === 0) this.bytes += existing.bytes;
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
      bytes: utf8Length(token),
    };
    this.bytes += query.bytes;
    this.byToken.set(token, query);
    this.byId.set(query.queryId, query);
    return { query, needsFlush: true };
  }

  /** Removes a watcher; returns true when the query has no watchers left (a flush should be scheduled). */
  removeRef(token: QueryToken): boolean {
    const query = this.byToken.get(token);
    if (!query) return false;
    if (query.refCount === 1) this.bytes -= query.bytes;
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
   * Drops queries without watchers and, if `canSend`, builds the ModifyQuerySets that bring the server's query set
   * in line with the local one (bumping the query set version once per message).
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
    if (!canSend) return { messages: [], removed };

    // Split so that no message outgrows the server's limit. The removes go first: the server applies a message in
    // order and caps what is live after each step, so a set near the cap is swapped by making room before using it.
    const batches: QuerySetModification[][] = [[]];
    let chars = 0;
    for (const queryId of this.pendingRemoves) {
      let batch = batches[batches.length - 1]!;
      if (chars + REMOVE_CHARS > MODIFY_QUERY_SET_CHARS) {
        batch = [];
        batches.push(batch);
        chars = 0;
      }
      batch.push({ type: "Remove", queryId });
      chars += REMOVE_CHARS;
    }
    this.pendingRemoves.clear();
    for (const query of this.byToken.values()) {
      if (query.onServer) continue;
      // The token is the path and the canonical JSON of the arguments: about what the Add takes on the wire.
      const size = query.token.length;
      let batch = batches[batches.length - 1]!;
      if (batch.length > 0 && chars + size > MODIFY_QUERY_SET_CHARS) {
        batch = [];
        batches.push(batch);
        chars = 0;
      }
      batch.push({ type: "Add", queryId: query.queryId, udfPath: query.udfPath, args: query.args });
      chars += size;
      query.onServer = true;
    }

    const messages: ModifyQuerySetMessage[] = [];
    for (const modifications of batches) {
      if (modifications.length === 0) continue;
      const baseVersion = this.querySetVersion;
      this.querySetVersion += 1;
      messages.push({ type: "ModifyQuerySet", baseVersion, newVersion: this.querySetVersion, modifications });
    }
    return { messages, removed };
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

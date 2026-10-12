import { applyDiff, DiffMismatch } from "./diff.js";
import { RxFerrixError } from "./errors.js";
import {
  formatVersion,
  versionsEqual,
  type QueryId,
  type StateModification,
  type StateVersion,
  type TransitionMessage,
} from "./protocol.js";
import type { QueryState } from "./query_state.js";

export class TransitionVersionMismatch extends Error {
  constructor(expected: StateVersion, got: StateVersion) {
    super(`Transition startVersion ${formatVersion(got)} does not match client version ${formatVersion(expected)}`);
  }
}

export interface TransitionResult {
  /** Queries whose server result changed (updated, failed or removed). */
  changed: QueryId[];
  logLines: { queryId: QueryId; lines: string[] }[];
}

/**
 * The server's view of the world for the current socket: the StateVersion and the latest result for every query.
 * Transitions are validated (`startVersion` must equal the current version) and then applied atomically.
 */
export class RemoteQuerySet {
  private _version: StateVersion = { querySet: 0, identity: 0, ts: 0 };
  private readonly results = new Map<QueryId, QueryState>();

  get version(): StateVersion {
    return this._version;
  }

  result(queryId: QueryId): QueryState | undefined {
    return this.results.get(queryId);
  }

  /**
   * Applies a transition. Modifications for queries that `isKnown` rejects (removed locally while the transition was
   * in flight) are ignored. Throws {@link TransitionVersionMismatch} without changing anything when the transition
   * does not start at the current version, and {@link DiffMismatch} when a result diff doesn't fit the result held.
   */
  transition(message: TransitionMessage, isKnown: (queryId: QueryId) => boolean): TransitionResult {
    if (!versionsEqual(message.startVersion, this._version)) {
      throw new TransitionVersionMismatch(this._version, message.startVersion);
    }
    // Diffs are applied first, against the results as they were: if one doesn't fit, nothing changes.
    const diffed = new Map<StateModification, unknown>();
    for (const modification of message.modifications ?? []) {
      if (modification.type !== "QueryUpdated" || !("diff" in modification) || !isKnown(modification.queryId)) continue;
      const current = this.results.get(modification.queryId);
      if (current?.status !== "success") {
        throw new DiffMismatch(`query ${modification.queryId} has no result to apply a diff to`);
      }
      diffed.set(modification, applyDiff(current.value, modification.diff));
    }
    const changed: QueryId[] = [];
    const logLines: TransitionResult["logLines"] = [];
    for (const modification of message.modifications ?? []) {
      const queryId = modification.queryId;
      if (!isKnown(queryId)) continue;
      switch (modification.type) {
        case "QueryUpdated":
          this.results.set(queryId, {
            status: "success",
            value: "diff" in modification ? diffed.get(modification) : modification.value,
          });
          break;
        case "QueryFailed":
          this.results.set(queryId, {
            status: "error",
            error: RxFerrixError.fromServer(modification.errorMessage, modification.errorData),
          });
          break;
        case "QueryRemoved":
          this.results.delete(queryId);
          break;
        default:
          continue;
      }
      changed.push(queryId);
      if ("logLines" in modification && modification.logLines && modification.logLines.length > 0) {
        logLines.push({ queryId, lines: modification.logLines });
      }
    }
    this._version = { ...message.endVersion };
    return { changed, logLines };
  }

  /** Forgets a query's result (it was removed from the local query set). */
  remove(queryId: QueryId): void {
    this.results.delete(queryId);
  }

  /**
   * A new socket starts from a fresh server state (all versions 0). Results are kept so the UI keeps showing the
   * last known data until the re-subscribed queries report again.
   */
  resetVersion(): void {
    this._version = { querySet: 0, identity: 0, ts: 0 };
  }
}

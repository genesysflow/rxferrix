import { CONNECTION_LOST, OUTCOME_UNKNOWN, RxFerrixError } from "./errors.js";
import type {
  ActionMessage,
  ActionResponseMessage,
  MutationMessage,
  MutationResponseMessage,
  RequestId,
} from "./protocol.js";

type Settle = { resolve: (value: unknown) => void; reject: (error: unknown) => void };

/**
 * How long after its first send a mutation may still be sent again. The server recognizes a resent mutation (and
 * answers with the recorded result instead of running it again) for 10 minutes (docs/PROTOCOL.md); later, a resend
 * could run it twice. Kept below that, for clock and transit slack.
 */
export const RESEND_WINDOW_MS = 9 * 60_000;

type MutationRequest = Settle & {
  kind: "mutation";
  message: MutationMessage;
  /** Sent on the current socket. */
  sent: boolean;
  /** Set once a successful MutationResponse arrived: waiting for a Transition with ts >= this. */
  completed: { ts: number; result: unknown } | null;
  startedAt: number;
  /** When it first went out, and as whom (`identity` of the client then): a resend must match both. */
  firstSent: { at: number; identity: string | null } | null;
};

type ActionRequest = Settle & {
  kind: "action";
  message: ActionMessage;
  sent: boolean;
  startedAt: number;
};

type Request = MutationRequest | ActionRequest;

export interface SettledMutation {
  requestId: RequestId;
  settle: () => void;
}

/**
 * Tracks in-flight mutations and actions.
 *
 * A mutation's promise resolves only once BOTH its successful `MutationResponse { ts: M }` has arrived AND the client
 * has applied a Transition with `endVersion.ts >= M` (read-your-writes). Failures reject immediately.
 *
 * Settling is split in two (`settle` callbacks are returned rather than invoked) so the client can update its query
 * view (dropping optimistic updates) before promise continuations run.
 */
export class RequestManager {
  private nextRequestId = 0;
  private readonly requests = new Map<RequestId, Request>();

  allocateRequestId(): RequestId {
    return this.nextRequestId++;
  }

  trackMutation(message: MutationMessage): Promise<unknown> {
    return new Promise((resolve, reject) => {
      this.requests.set(message.requestId, {
        kind: "mutation",
        message,
        sent: false,
        completed: null,
        startedAt: Date.now(),
        firstSent: null,
        resolve,
        reject,
      });
    });
  }

  trackAction(message: ActionMessage): Promise<unknown> {
    return new Promise((resolve, reject) => {
      this.requests.set(message.requestId, {
        kind: "action",
        message,
        sent: false,
        startedAt: Date.now(),
        resolve,
        reject,
      });
    });
  }

  /** Messages that still have to go out on the current socket, in request order. */
  unsent(): (MutationMessage | ActionMessage)[] {
    const out: (MutationMessage | ActionMessage)[] = [];
    for (const request of this.requests.values()) {
      if (request.sent) continue;
      if (request.kind === "mutation" && request.completed) continue;
      out.push(request.message);
    }
    return out;
  }

  /** The message went out on the current socket, from `identity` (whom the socket is authenticated as). */
  markSent(requestId: RequestId, identity: string | null = null): void {
    const request = this.requests.get(requestId);
    if (!request) return;
    request.sent = true;
    if (request.kind === "mutation" && !request.firstSent) request.firstSent = { at: Date.now(), identity };
  }

  /**
   * Mutations waiting to be sent again that can't be safely: sent more than {@link RESEND_WINDOW_MS} ago (the server
   * no longer recognizes them, so they could run twice), or from another identity than now (the server never matches
   * a resend across identities: it would run, or fail, as someone else). They are taken off and returned, rejected
   * with {@link OUTCOME_UNKNOWN}: whether the first send ran isn't known.
   */
  expireUnsafeResends(identity: string | null, now: number = Date.now()): SettledMutation[] {
    const expired: SettledMutation[] = [];
    for (const [requestId, request] of this.requests) {
      if (request.kind !== "mutation" || request.sent || request.completed || !request.firstSent) continue;
      const tooOld = now - request.firstSent.at > RESEND_WINDOW_MS;
      if (!tooOld && request.firstSent.identity === identity) continue;
      this.requests.delete(requestId);
      const why = tooOld ? "too long ago to be sent again safely" : "as another user than now";
      const error = new RxFerrixError(
        `The connection was lost after mutation "${request.message.udfPath}" was sent, ${why}: it may or may not have run`,
        { code: OUTCOME_UNKNOWN },
      );
      expired.push({ requestId, settle: () => request.reject(error) });
    }
    return expired;
  }

  /**
   * Handles a MutationResponse. `observedTs` is the ts of the last applied Transition, or null when the client's view
   * is not currently consistent with the server (right after a reconnect).
   *
   * Returns the settled mutation (whose optimistic update must be dropped), or null if it is still waiting for its
   * ts to be observed (or the requestId is unknown).
   */
  onMutationResponse(response: MutationResponseMessage, observedTs: number | null): SettledMutation | null {
    const request = this.requests.get(response.requestId);
    if (!request || request.kind !== "mutation" || request.completed) return null;
    if (!response.success) {
      this.requests.delete(response.requestId);
      const error = RxFerrixError.fromServer(response.errorMessage, response.errorData);
      return { requestId: response.requestId, settle: () => request.reject(error) };
    }
    request.completed = { ts: response.ts, result: response.result };
    if (observedTs !== null && observedTs >= response.ts) {
      this.requests.delete(response.requestId);
      const result = response.result;
      return { requestId: response.requestId, settle: () => request.resolve(result) };
    }
    return null;
  }

  /** Handles an ActionResponse. Returns the settle callback, or null for an unknown request. */
  onActionResponse(response: ActionResponseMessage): (() => void) | null {
    const request = this.requests.get(response.requestId);
    if (!request || request.kind !== "action") return null;
    this.requests.delete(response.requestId);
    if (response.success) {
      const result = response.result;
      return () => request.resolve(result);
    }
    const error = RxFerrixError.fromServer(response.errorMessage, response.errorData);
    return () => request.reject(error);
  }

  /** A Transition with `endVersion.ts = ts` was applied: completes every mutation with a commit ts <= ts. */
  removeCompleted(ts: number): SettledMutation[] {
    const settled: SettledMutation[] = [];
    for (const [requestId, request] of this.requests) {
      if (request.kind !== "mutation" || !request.completed || request.completed.ts > ts) continue;
      this.requests.delete(requestId);
      const result = request.completed.result;
      settled.push({ requestId, settle: () => request.resolve(result) });
    }
    return settled;
  }

  /**
   * The socket dropped. Mutations without a response will be re-sent on the next socket. Actions that were already
   * sent are rejected because they are not idempotent; actions that never left the client stay queued.
   */
  onSocketClosed(): { settle: () => void; requestId: RequestId }[] {
    const rejected: { settle: () => void; requestId: RequestId }[] = [];
    for (const [requestId, request] of this.requests) {
      if (request.kind === "mutation") {
        if (!request.completed) request.sent = false;
        continue;
      }
      if (!request.sent) continue;
      this.requests.delete(requestId);
      const error = new RxFerrixError(
        `Connection lost while action "${request.message.udfPath}" was in flight`,
        { code: CONNECTION_LOST },
      );
      rejected.push({ requestId, settle: () => request.reject(error) });
    }
    return rejected;
  }

  /** Rejects everything (client closed). */
  rejectAll(error: RxFerrixError): void {
    const all = [...this.requests.values()];
    this.requests.clear();
    for (const request of all) request.reject(error);
  }

  hasInflight(): boolean {
    return this.requests.size > 0;
  }

  hasInflightMutations(): boolean {
    for (const request of this.requests.values()) if (request.kind === "mutation") return true;
    return false;
  }

  counts(): { mutations: number; actions: number; oldestStartedAt: number | null } {
    let mutations = 0;
    let actions = 0;
    let oldest: number | null = null;
    for (const request of this.requests.values()) {
      if (request.kind === "mutation") mutations++;
      else actions++;
      if (oldest === null || request.startedAt < oldest) oldest = request.startedAt;
    }
    return { mutations, actions, oldestStartedAt: oldest };
  }

  udfPathOf(requestId: RequestId): string | undefined {
    return this.requests.get(requestId)?.message.udfPath;
  }
}

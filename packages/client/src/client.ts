import type {
  ArgsAndOptions,
  ArgsOf,
  FunctionReference,
  OptionalArgs,
  OutputOf,
} from "./api.js";
import { normalizeArgs, queryToken, type QueryToken } from "./canonical.js";
import { CLIENT_CLOSED, RxFerrixError } from "./errors.js";
import { LocalQuerySet } from "./local_query_set.js";
import { createLogger, type Logger } from "./logging.js";
import {
  OptimisticQueryResults,
  type OptimisticLocalStore,
  type OptimisticUpdate,
  type ViewMap,
} from "./optimistic_updates.js";
import type {
  ActionMessage,
  AuthErrorMessage,
  MutationMessage,
  QueryId,
  ServerMessage,
  TransitionMessage,
} from "./protocol.js";
import type { QueryState } from "./query_state.js";
import {
  PaginatedQueryImpl,
  type PaginatedQuery,
  type PaginatedQueryArgs,
  type PaginatedQueryItem,
  type PaginatedQueryOptions,
  type PaginatedQueryReference,
} from "./paginated_query.js";
import { DiffMismatch } from "./diff.js";
import { RemoteQuerySet, TransitionVersionMismatch } from "./remote_query_set.js";
import { RequestManager, type SettledMutation } from "./request_manager.js";
import { WebSocketManager } from "./web_socket_manager.js";

export interface RxFerrixClientOptions {
  /** WebSocket implementation to use (defaults to `globalThis.WebSocket`). Needed on Node < 22 or for tests. */
  webSocketConstructor?: typeof WebSocket;
  /** Log every protocol message to `console.debug`. */
  verbose?: boolean;
  /** Log connection diagnostics (reconnects and their reasons) to `console.info`. */
  reportDebugInfoToConsole?: boolean;
  /**
   * Show the browser's "unsaved changes" prompt when the page is closed while mutations are in flight.
   * Defaults to true in browsers.
   */
  unsavedChangesWarning?: boolean;
}

export interface MutationOptions<Args> {
  optimisticUpdate?: OptimisticUpdate<Args>;
}

/** Fetches a JWT (or other bearer token). `forceRefresh` is true when the cached token was rejected or expires. */
export type AuthTokenFetcher = (options: { forceRefresh: boolean }) => Promise<string | null | undefined>;

export interface ConnectionState {
  /** The socket is open and the session handshake has been sent. */
  readonly isWebSocketConnected: boolean;
  /** At least one mutation or action has not settled yet. */
  readonly hasInflightRequests: boolean;
  readonly hasEverConnected: boolean;
  /** Number of sockets opened so far, minus one (the `connectionCount` of the current `Connect`). */
  readonly connectionCount: number;
  /** Consecutive failed connection attempts since the last healthy socket. */
  readonly connectionRetries: number;
  readonly inflightMutations: number;
  readonly inflightActions: number;
  readonly timeOfOldestInflightRequest: Date | null;
  /** Whether the server has confirmed the current auth token. */
  readonly isAuthenticated: boolean;
}

export interface QueryWatch<T> {
  /** Subscribes to changes of this query. The server subscription lives while at least one callback is registered. */
  onUpdate(callback: () => void): () => void;
  /** The current value, `undefined` while loading. Throws the query's {@link RxFerrixError} if it failed. */
  localQueryResult(): T | undefined;
  /** The current state. The returned object is referentially stable while the visible value does not change. */
  localQueryState(): QueryState<T>;
}

function deriveSyncUrl(url: string): string {
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    throw new Error(`RxFerrixClient: invalid deployment URL ${JSON.stringify(url)}`);
  }
  switch (parsed.protocol) {
    case "http:":
    case "ws:":
      parsed.protocol = "ws:";
      break;
    case "https:":
    case "wss:":
      parsed.protocol = "wss:";
      break;
    default:
      throw new Error(`RxFerrixClient: unsupported URL protocol ${parsed.protocol}`);
  }
  parsed.pathname = parsed.pathname.replace(/\/+$/, "") + "/api/sync";
  parsed.search = "";
  parsed.hash = "";
  return parsed.toString();
}

function randomSessionId(): string {
  const c = (globalThis as { crypto?: { randomUUID?: () => string } }).crypto;
  if (c?.randomUUID) return c.randomUUID();
  // RFC 4122 v4 fallback.
  return "xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx".replace(/[xy]/g, (ch) => {
    const r = (Math.random() * 16) | 0;
    return (ch === "x" ? r : (r & 0x3) | 0x8).toString(16);
  });
}

/** Seconds-since-epoch `exp` claim of a JWT, if the token is a decodable JWT. */
/** Who a token speaks for, as far as the server's mutation dedup goes: its issuer and `sub` (`null` for no token). A
 * refreshed token keeps it; another user's doesn't. */
function tokenIdentity(token: string | null): string | null {
  if (token === null) return null;
  const parts = token.split(".");
  try {
    const b64 = (parts[1] ?? "").replace(/-/g, "+").replace(/_/g, "/");
    const padded = b64 + "=".repeat((4 - (b64.length % 4)) % 4);
    const payload = JSON.parse(typeof atob === "function" ? atob(padded) : "") as { sub?: unknown; iss?: unknown };
    return `${String(payload.iss ?? "")}|${String(payload.sub ?? "")}`;
  } catch {
    return token;
  }
}

function jwtExpiry(token: string): number | null {
  const parts = token.split(".");
  if (parts.length !== 3 || !parts[1]) return null;
  try {
    const b64 = parts[1].replace(/-/g, "+").replace(/_/g, "/");
    const padded = b64 + "=".repeat((4 - (b64.length % 4)) % 4);
    const json = typeof atob === "function" ? atob(padded) : "";
    const payload = JSON.parse(json) as { exp?: unknown };
    return typeof payload.exp === "number" ? payload.exp : null;
  } catch {
    return null;
  }
}

/** Refresh this long before a JWT's `exp`. */
const TOKEN_REFRESH_LEEWAY_MS = 30_000;
/** Never refresh more often than this, even for a token that expires within the leeway (its session may be ending). */
const MIN_TOKEN_REFRESH_DELAY_MS = 5_000;
/** A token getter that threw (the server unreachable at page load, ...) is retried with this backoff. */
const TOKEN_FETCH_RETRY_BASE_MS = 2_000;
const TOKEN_FETCH_RETRY_MAX_MS = 60_000;
const MAX_TIMEOUT_MS = 2_147_483_647;
/** The server's limit of live queries per socket; past it, a `ModifyQuerySet` is a fatal error (docs/PROTOCOL.md). */
export const MAX_LIVE_QUERIES = 1024;

/** The error for function args that can't be encoded as JSON (a cycle, a BigInt, ...), or null. */
function unserializable(args: Record<string, unknown>): RxFerrixError | null {
  try {
    JSON.stringify(args);
    return null;
  } catch (e) {
    const detail = e instanceof Error ? e.message : String(e);
    return new RxFerrixError(`Arguments can't be serialized as JSON: ${detail}`, { code: "invalid_args", cause: e });
  }
}

interface AuthState {
  fetchToken: AuthTokenFetcher;
  onChange: ((isAuthenticated: boolean) => void) | undefined;
  /** True after one forced refresh in response to an AuthError; reset once a token is confirmed. */
  retriedAfterAuthError: boolean;
}

/**
 * The RxFerrix sync client: subscribes to queries over a WebSocket and runs mutations and actions.
 *
 * ```ts
 * const client = new RxFerrixClient("http://localhost:3210");
 * const watch = client.watchQuery(api.todos.list, {});
 * watch.onUpdate(() => console.log(watch.localQueryResult()));
 * await client.mutation(api.todos.add, { text: "hi" });
 * ```
 */
export class RxFerrixClient {
  /** The http(s) base URL this client was created with. */
  readonly url: string;
  /** The WebSocket URL (`ws(s)://host/api/sync`). */
  readonly syncUrl: string;
  readonly sessionId: string;

  private readonly logger: Logger;
  private readonly ws: WebSocketManager;
  private readonly localQueries = new LocalQuerySet();
  private readonly remoteQueries = new RemoteQuerySet();
  private readonly requests = new RequestManager();
  private readonly optimistic: OptimisticQueryResults;
  private readonly listeners = new Map<QueryToken, Set<() => void>>();
  private readonly connectionListeners = new Set<(state: ConnectionState) => void>();
  private cachedConnectionState: ConnectionState;
  private flushScheduled = false;
  private closed = false;

  /**
   * Set after a reconnect while live queries have not yet received results from the new socket. The local view then
   * still shows results from the old socket, so it is not "at" the new socket's ts; mutation completion waits.
   */
  private resyncQuerySetVersion: number | null = null;

  // Auth
  private auth: AuthState | null = null;
  private authToken: string | null = null;
  /** The token held by the server on this socket (null = none sent / cleared). */
  private serverToken: string | null = null;
  /** Identity version sent on this socket (the server's identity version once processed). */
  private identityVersion = 0;
  /** Identity version whose confirmation (a Transition reaching it) turns `isAuthenticated` on. */
  private awaitingAuthConfirmation: number | null = null;
  private isAuthenticated = false;
  private authGeneration = 0;
  private authRefreshTimer: ReturnType<typeof setTimeout> | null = null;
  /** Token fetches in flight: the last one to finish lets held-back sends through again. */
  private tokenFetchesInFlight = 0;
  private tokenFetchFailures = 0;
  /** Rejecters of `query()` calls still waiting for their first result (settled on `close()`). */
  private readonly oneShotQueries = new Set<(e: RxFerrixError) => void>();

  private readonly removeUnloadListener: () => void;

  constructor(url: string, options: RxFerrixClientOptions = {}) {
    if (typeof url !== "string") throw new Error("RxFerrixClient: url must be a string");
    this.url = url;
    this.syncUrl = deriveSyncUrl(url);
    this.sessionId = randomSessionId();
    this.logger = createLogger({
      verbose: options.verbose ?? false,
      reportDebugInfoToConsole: options.reportDebugInfoToConsole ?? false,
    });
    this.optimistic = new OptimisticQueryResults(this.logger);

    const WS = options.webSocketConstructor ?? (globalThis as { WebSocket?: typeof WebSocket }).WebSocket;
    if (!WS) {
      throw new Error(
        "RxFerrixClient: no global WebSocket available; pass `webSocketConstructor` (e.g. from the `ws` package)",
      );
    }
    this.ws = new WebSocketManager(
      { url: this.syncUrl, sessionId: this.sessionId, webSocketConstructor: WS, logger: this.logger },
      {
        onOpen: () => this.onSocketOpen(),
        onResume: () => this.syncToServer(),
        onMessage: (message) => this.onMessage(message),
        onClose: () => this.onSocketClosed(),
        onStateChange: () => this.notifyConnectionState(),
      },
    );
    this.cachedConnectionState = this.computeConnectionState();
    this.removeUnloadListener = this.installUnloadListener(options.unsavedChangesWarning);
    this.ws.start();
  }

  // ---------------------------------------------------------------------------------------------------------------
  // Queries

  /**
   * Creates a watch on a query. Nothing is sent until `onUpdate` registers the first callback; all watches on the
   * same `(udfPath, canonicalJSON(args))` share one server subscription.
   */
  watchQuery<Q extends FunctionReference<"query">>(ref: Q, ...args: OptionalArgs<Q>): QueryWatch<OutputOf<Q>> {
    const udfPath = ref._path;
    const normalized = normalizeArgs(args[0]);
    const token = queryToken(udfPath, normalized);
    return {
      onUpdate: (callback) => this.subscribe(token, udfPath, normalized, callback),
      localQueryResult: () => {
        const state = this.optimistic.get(token) as QueryState<OutputOf<Q>>;
        if (state.status === "error") throw state.error;
        return state.status === "success" ? state.value : undefined;
      },
      localQueryState: () => this.optimistic.get(token) as QueryState<OutputOf<Q>>,
    };
  }

  /** Runs a query once: subscribes, waits for the first result, then unsubscribes. */
  query<Q extends FunctionReference<"query">>(ref: Q, ...args: OptionalArgs<Q>): Promise<OutputOf<Q>> {
    if (this.closed) return Promise.reject(new RxFerrixError("Client is closed", { code: CLIENT_CLOSED }));
    const watch = this.watchQuery(ref, ...args);
    return new Promise<OutputOf<Q>>((resolve, reject) => {
      let unsubscribe: (() => void) | null = null;
      let done = false;
      const finish = (settle: () => void) => {
        if (done) return;
        done = true;
        this.oneShotQueries.delete(closed);
        if (unsubscribe) unsubscribe();
        settle();
      };
      const closed = (e: RxFerrixError) => finish(() => reject(e));
      const check = () => {
        const state = watch.localQueryState();
        if (state.status === "loading") return;
        finish(() => (state.status === "success" ? resolve(state.value) : reject(state.error)));
      };
      this.oneShotQueries.add(closed);
      unsubscribe = watch.onUpdate(check);
      check();
    });
  }

  /**
   * Creates a reactive paginated query over a query that takes `paginationOpts` and returns a `PaginationResult`
   * (see `paginated_query.ts`). Pages are subscribed while the returned object has subscribers.
   *
   * ```ts
   * const messages = client.paginatedQuery(api.messages.list, { channel }, { initialNumItems: 20 });
   * messages.subscribe(() => render(messages.getCurrent()));   // { results, status, loadMore, ... }
   * ```
   */
  paginatedQuery<Q extends PaginatedQueryReference>(
    ref: Q,
    args: PaginatedQueryArgs<Q>,
    options: PaginatedQueryOptions,
  ): PaginatedQuery<PaginatedQueryItem<Q>> {
    return new PaginatedQueryImpl<PaginatedQueryItem<Q>>(this, ref, args as Record<string, unknown>, options);
  }

  private subscribe(
    token: QueryToken,
    udfPath: string,
    args: Record<string, unknown>,
    callback: () => void,
  ): () => void {
    if (this.closed) return () => {};
    if (!this.localQueries.get(token) && this.localQueries.size >= MAX_LIVE_QUERIES) {
      // The server would answer the subscription with a fatal error and every reconnect would repeat it.
      throw new RxFerrixError(
        `Too many live queries: at most ${MAX_LIVE_QUERIES} distinct queries can be watched at once`,
        { code: "too_many_queries" },
      );
    }
    let set = this.listeners.get(token);
    if (!set) {
      set = new Set();
      this.listeners.set(token, set);
    }
    // Wrap so the same callback can be registered twice and removed independently.
    const entry = () => callback();
    set.add(entry);
    const { needsFlush } = this.localQueries.addRef(udfPath, args);
    if (needsFlush) this.scheduleFlush();
    let active = true;
    return () => {
      if (!active) return;
      active = false;
      const current = this.listeners.get(token);
      current?.delete(entry);
      if (current && current.size === 0) this.listeners.delete(token);
      if (this.localQueries.removeRef(token)) this.scheduleFlush();
    };
  }

  /** Batches query-set changes made in the same tick into one ModifyQuerySet. */
  private scheduleFlush(): void {
    if (this.flushScheduled || this.closed) return;
    this.flushScheduled = true;
    queueMicrotask(() => {
      this.flushScheduled = false;
      if (this.closed) return;
      this.flushQuerySet();
    });
  }

  private flushQuerySet(): void {
    const { message, removed } = this.localQueries.flush(this.ws.canSend());
    if (removed.length > 0) {
      for (const queryId of removed) this.remoteQueries.remove(queryId);
      this.recomputeAndNotify();
    }
    if (message) this.ws.send(message);
  }

  // ---------------------------------------------------------------------------------------------------------------
  // Mutations and actions

  /**
   * Runs a mutation. The returned promise resolves with the mutation's result once the client's queries reflect the
   * mutation (read-your-writes); it rejects with a {@link RxFerrixError} if the mutation fails.
   */
  mutation<M extends FunctionReference<"mutation">>(
    ref: M,
    ...argsAndOptions: ArgsAndOptions<M, MutationOptions<ArgsOf<M>>>
  ): Promise<OutputOf<M>> {
    if (this.closed) return Promise.reject(new RxFerrixError("Client is closed", { code: CLIENT_CLOSED }));
    const args = normalizeArgs(argsAndOptions[0]);
    const options = argsAndOptions[1] as MutationOptions<ArgsOf<M>> | undefined;
    // Checked before it is queued: a message that can't be encoded would otherwise be retried on every socket and
    // block every later request behind it.
    const invalid = unserializable(args);
    if (invalid) return Promise.reject(invalid);
    const requestId = this.requests.allocateRequestId();
    const message: MutationMessage = { type: "Mutation", requestId, udfPath: ref._path, args };
    const promise = this.requests.trackMutation(message) as Promise<OutputOf<M>>;
    if (options?.optimisticUpdate) {
      const update = options.optimisticUpdate;
      const typedArgs = args as ArgsOf<M>;
      this.optimistic.addUpdate(requestId, (store: OptimisticLocalStore) => update(store, typedArgs));
      this.recomputeAndNotify();
    }
    this.sendUnsentRequests();
    this.notifyConnectionState();
    return promise;
  }

  /** Runs an action. Rejects with a "connection lost" error if the socket drops while it is in flight. */
  action<A extends FunctionReference<"action">>(ref: A, ...args: OptionalArgs<A>): Promise<OutputOf<A>> {
    if (this.closed) return Promise.reject(new RxFerrixError("Client is closed", { code: CLIENT_CLOSED }));
    const normalized = normalizeArgs(args[0]);
    const invalid = unserializable(normalized);
    if (invalid) return Promise.reject(invalid);
    const requestId = this.requests.allocateRequestId();
    const message: ActionMessage = { type: "Action", requestId, udfPath: ref._path, args: normalized };
    const promise = this.requests.trackAction(message) as Promise<OutputOf<A>>;
    this.sendUnsentRequests();
    this.notifyConnectionState();
    return promise;
  }

  private sendUnsentRequests(): void {
    if (!this.ws.canSend()) return;
    // Who the server takes these requests from: the identity of the token sent last (Authenticate goes out first).
    const identity = tokenIdentity(this.serverToken);
    const expired = this.requests.expireUnsafeResends(identity);
    if (expired.length > 0) this.finishMutations(expired);
    for (const message of this.requests.unsent()) {
      if (!this.ws.send(message)) break;
      this.requests.markSent(message.requestId, identity);
    }
  }

  // ---------------------------------------------------------------------------------------------------------------
  // Auth

  /**
   * Sets the auth token source. The client fetches a token right away (holding back query subscriptions until it
   * has one), sends it with `Authenticate`, re-sends it on every reconnect, refreshes JWTs shortly before they expire,
   * and retries once with `forceRefresh: true` when the server rejects the token.
   *
   * `onChange(true)` fires once the server has confirmed the token; `onChange(false)` when auth is lost or cleared.
   */
  setAuth(fetchToken: AuthTokenFetcher, onChange?: (isAuthenticated: boolean) => void): void {
    if (this.closed) return;
    this.auth = { fetchToken, onChange, retriedAfterAuthError: false };
    void this.fetchAndApplyToken(false, true);
  }

  /** Removes the token: the server falls back to an anonymous identity. */
  clearAuth(): void {
    const previous = this.auth;
    this.auth = null;
    this.authGeneration++;
    this.clearAuthRefreshTimer();
    this.applyToken(null);
    if (this.ws.isPaused) this.ws.resume();
    this.setAuthenticated(false, previous?.onChange);
  }

  private async fetchAndApplyToken(forceRefresh: boolean, pause: boolean): Promise<void> {
    const auth = this.auth;
    if (!auth) return;
    const generation = ++this.authGeneration;
    if (pause) this.ws.pause();
    this.tokenFetchesInFlight++;
    try {
      let token: string | null;
      try {
        token = (await auth.fetchToken({ forceRefresh })) ?? null;
      } catch (e) {
        if (generation !== this.authGeneration || this.auth !== auth || this.closed) return;
        // The getter failed (the server unreachable at page load, a refresh that timed out): the session it would
        // have returned may well be valid, so the token in hand (if any) is kept and the fetch is retried with
        // backoff instead of treating the user as signed out.
        this.tokenFetchFailures++;
        const delay = Math.min(
          TOKEN_FETCH_RETRY_MAX_MS,
          TOKEN_FETCH_RETRY_BASE_MS * 2 ** Math.min(this.tokenFetchFailures - 1, 10),
        );
        this.logger.warn(`fetchToken failed; retrying in ${delay}ms`, e);
        this.scheduleTokenFetch(delay, forceRefresh);
        return;
      }
      if (generation !== this.authGeneration || this.auth !== auth || this.closed) return;
      this.tokenFetchFailures = 0;
      this.applyToken(token);
      if (token === null) this.setAuthenticated(false, auth.onChange);
      this.scheduleTokenRefresh(token);
    } finally {
      this.tokenFetchesInFlight--;
      // Whichever fetch finishes last lets sends through again: a superseded fetch (a newer `setAuth` or
      // `clearAuth`) must not leave the socket paused for good.
      if (this.tokenFetchesInFlight === 0 && this.ws.isPaused && !this.closed) this.ws.resume();
    }
  }

  private scheduleTokenFetch(delay: number, forceRefresh: boolean): void {
    this.clearAuthRefreshTimer();
    this.authRefreshTimer = setTimeout(() => {
      this.authRefreshTimer = null;
      void this.fetchAndApplyToken(forceRefresh, false);
    }, delay);
  }

  /** Records the desired token and sends `Authenticate` when the server's token differs. */
  private applyToken(token: string | null): void {
    this.authToken = token;
    this.syncToServer();
  }

  private sendAuthenticateIfNeeded(): void {
    if (this.authToken === this.serverToken) return;
    const baseVersion = this.identityVersion;
    const sent = this.ws.send({ type: "Authenticate", baseVersion, token: this.authToken });
    if (!sent) return;
    this.identityVersion = baseVersion + 1;
    this.serverToken = this.authToken;
    this.awaitingAuthConfirmation = this.authToken !== null ? this.identityVersion : null;
  }

  private scheduleTokenRefresh(token: string | null): void {
    this.clearAuthRefreshTimer();
    if (!token) return;
    const exp = jwtExpiry(token);
    if (exp === null) return;
    // Floored: a token that already expires within the leeway (its session is about to end) would otherwise be
    // refreshed again the moment the refreshed one arrives, in a tight loop.
    const delay = Math.min(
      MAX_TIMEOUT_MS,
      Math.max(MIN_TOKEN_REFRESH_DELAY_MS, exp * 1000 - Date.now() - TOKEN_REFRESH_LEEWAY_MS),
    );
    this.scheduleTokenFetch(delay, true);
  }

  private clearAuthRefreshTimer(): void {
    if (this.authRefreshTimer !== null) {
      clearTimeout(this.authRefreshTimer);
      this.authRefreshTimer = null;
    }
  }

  private onAuthError(message: AuthErrorMessage): void {
    this.logger.warn(`authentication rejected: ${message.error}`);
    const auth = this.auth;
    // The server closes the socket after AuthError; the next socket must not resend the rejected token blindly.
    this.ws.reconnect("AuthError");
    if (auth && !auth.retriedAfterAuthError) {
      auth.retriedAfterAuthError = true;
      void this.fetchAndApplyToken(true, true);
      return;
    }
    this.authToken = null;
    this.clearAuthRefreshTimer();
    this.setAuthenticated(false, auth?.onChange);
  }

  private setAuthenticated(value: boolean, onChange: ((v: boolean) => void) | undefined): void {
    const changed = this.isAuthenticated !== value;
    this.isAuthenticated = value;
    if (changed) this.notifyConnectionState();
    if (onChange && (changed || !value)) {
      try {
        onChange(value);
      } catch (e) {
        this.logger.error("auth onChange threw", e);
      }
    }
  }

  // ---------------------------------------------------------------------------------------------------------------
  // Socket lifecycle

  /** A new socket is open and `Connect` was sent: rebuild the server's state from scratch. */
  private onSocketOpen(): void {
    this.remoteQueries.resetVersion();
    this.localQueries.resetForNewSocket();
    this.identityVersion = 0;
    this.serverToken = null;
    this.awaitingAuthConfirmation = null;
    this.syncToServer();
    // The view still shows the previous socket's results until the re-added queries report from this socket.
    const resent = this.localQueries.sentQuerySetVersion;
    this.resyncQuerySetVersion = resent > 0 ? resent : null;
  }

  /** Sends everything the server does not know yet, in protocol order: Authenticate, ModifyQuerySet, requests. */
  private syncToServer(): void {
    if (!this.ws.canSend()) return;
    this.sendAuthenticateIfNeeded();
    this.flushQuerySet();
    this.sendUnsentRequests();
  }

  private onSocketClosed(): void {
    const rejected = this.requests.onSocketClosed();
    for (const r of rejected) r.settle();
    if (rejected.length > 0) this.notifyConnectionState();
  }

  private onMessage(message: ServerMessage): void {
    switch (message.type) {
      case "Transition":
        this.onTransition(message);
        return;
      case "MutationResponse": {
        const udfPath = this.requests.udfPathOf(message.requestId) ?? "?";
        this.logger.logLines("mutation", udfPath, message.logLines);
        const settled = this.requests.onMutationResponse(message, this.observedTs());
        if (settled) this.finishMutations([settled]);
        return;
      }
      case "ActionResponse": {
        const udfPath = this.requests.udfPathOf(message.requestId) ?? "?";
        this.logger.logLines("action", udfPath, message.logLines);
        const settle = this.requests.onActionResponse(message);
        if (settle) {
          settle();
          this.notifyConnectionState();
        }
        return;
      }
      case "FatalError":
        this.logger.error(`server reported a fatal error: ${message.error}`);
        this.ws.reconnect(`FatalError: ${message.error}`);
        return;
      case "AuthError":
        this.onAuthError(message);
        return;
      case "Ping":
        return; // Pong is sent by the socket manager.
      default:
        this.logger.warn("unknown server message", message);
    }
  }

  /** The ts the local view is consistent with, or null while resyncing after a reconnect. */
  private observedTs(): number | null {
    return this.resyncQuerySetVersion === null ? this.remoteQueries.version.ts : null;
  }

  private onTransition(message: TransitionMessage): void {
    let result;
    try {
      result = this.remoteQueries.transition(message, (queryId) => this.localQueries.getById(queryId) !== undefined);
    } catch (e) {
      if (e instanceof TransitionVersionMismatch) {
        this.logger.warn(e.message);
        this.ws.reconnect("TransitionVersionMismatch");
        return;
      }
      if (e instanceof DiffMismatch) {
        // A new connection starts from full results.
        this.logger.warn(e.message);
        this.ws.reconnect("DiffMismatch");
        return;
      }
      throw e;
    }
    this.ws.markHealthy();
    for (const { queryId, lines } of result.logLines) {
      this.logger.logLines("query", this.localQueries.getById(queryId)?.udfPath ?? "?", lines);
    }

    const end = message.endVersion;
    if (this.resyncQuerySetVersion !== null && end.querySet >= this.resyncQuerySetVersion) {
      this.resyncQuerySetVersion = null;
    }
    if (this.awaitingAuthConfirmation !== null && end.identity >= this.awaitingAuthConfirmation) {
      this.awaitingAuthConfirmation = null;
      if (this.auth) this.auth.retriedAfterAuthError = false;
      this.setAuthenticated(true, this.auth?.onChange);
    }

    const ts = this.observedTs();
    const settled = ts === null ? [] : this.requests.removeCompleted(ts);
    this.finishMutations(settled, result.changed.length > 0);
  }

  /** Drops the settled mutations' optimistic updates, updates the view and notifies watchers, then settles. */
  private finishMutations(settled: SettledMutation[], forceRecompute = false): void {
    this.optimistic.dropUpdates(settled.map((s) => s.requestId));
    if (forceRecompute || settled.length > 0) this.recomputeAndNotify();
    for (const s of settled) s.settle();
    if (settled.length > 0) this.notifyConnectionState();
  }

  // ---------------------------------------------------------------------------------------------------------------
  // View

  private serverView(): ViewMap {
    const view: ViewMap = new Map();
    for (const query of this.localQueries.queries()) {
      const state = this.remoteQueries.result(query.queryId);
      if (state) view.set(query.token, { udfPath: query.udfPath, args: query.args, state });
    }
    return view;
  }

  /** Rebuilds the visible view; after the whole update is applied, notifies the watchers of changed queries. */
  private recomputeAndNotify(): void {
    const changed = this.optimistic.recompute(this.serverView());
    const callbacks: (() => void)[] = [];
    for (const token of changed) {
      const set = this.listeners.get(token);
      if (set) callbacks.push(...set);
    }
    for (const callback of callbacks) {
      try {
        callback();
      } catch (e) {
        this.logger.error("query watcher threw", e);
      }
    }
  }

  // ---------------------------------------------------------------------------------------------------------------
  // Connection state

  /** The current connection state. Referentially stable until something changes. */
  connectionState(): ConnectionState {
    return this.cachedConnectionState;
  }

  subscribeToConnectionState(callback: (state: ConnectionState) => void): () => void {
    this.connectionListeners.add(callback);
    return () => {
      this.connectionListeners.delete(callback);
    };
  }

  private computeConnectionState(): ConnectionState {
    const counts = this.requests.counts();
    return {
      isWebSocketConnected: this.ws.state === "ready",
      hasInflightRequests: counts.mutations + counts.actions > 0,
      hasEverConnected: this.ws.hasEverConnected,
      connectionCount: this.ws.connectionCount,
      connectionRetries: this.ws.connectionRetries,
      inflightMutations: counts.mutations,
      inflightActions: counts.actions,
      timeOfOldestInflightRequest: counts.oldestStartedAt === null ? null : new Date(counts.oldestStartedAt),
      isAuthenticated: this.isAuthenticated,
    };
  }

  private notifyConnectionState(): void {
    // Called from the socket manager's constructor path before fields are ready.
    if (!this.connectionListeners) return;
    const next = this.computeConnectionState();
    const prev = this.cachedConnectionState;
    if (
      prev &&
      prev.isWebSocketConnected === next.isWebSocketConnected &&
      prev.hasInflightRequests === next.hasInflightRequests &&
      prev.hasEverConnected === next.hasEverConnected &&
      prev.connectionCount === next.connectionCount &&
      prev.connectionRetries === next.connectionRetries &&
      prev.inflightMutations === next.inflightMutations &&
      prev.inflightActions === next.inflightActions &&
      prev.timeOfOldestInflightRequest?.getTime() === next.timeOfOldestInflightRequest?.getTime() &&
      prev.isAuthenticated === next.isAuthenticated
    ) {
      return;
    }
    this.cachedConnectionState = next;
    for (const listener of [...this.connectionListeners]) {
      try {
        listener(next);
      } catch (e) {
        this.logger.error("connection state listener threw", e);
      }
    }
  }

  // ---------------------------------------------------------------------------------------------------------------

  /** Closes the socket for good. Pending mutations and actions are rejected. */
  close(): void {
    if (this.closed) return;
    this.closed = true;
    this.authGeneration++;
    this.clearAuthRefreshTimer();
    this.ws.stop();
    this.removeUnloadListener();
    const closed = new RxFerrixError("Client is closed", { code: CLIENT_CLOSED });
    this.requests.rejectAll(closed);
    // One-shot queries still waiting for a first result would never settle once the listeners are gone.
    for (const reject of [...this.oneShotQueries]) reject(closed);
    this.oneShotQueries.clear();
    this.listeners.clear();
    this.notifyConnectionState();
  }

  private installUnloadListener(enabled: boolean | undefined): () => void {
    const w = globalThis as {
      window?: unknown;
      addEventListener?: (t: string, l: (e: { preventDefault(): void; returnValue?: unknown }) => void) => void;
      removeEventListener?: (t: string, l: (e: { preventDefault(): void; returnValue?: unknown }) => void) => void;
    };
    if (enabled === false || typeof w.window === "undefined" || typeof w.addEventListener !== "function") {
      return () => {};
    }
    const handler = (e: { preventDefault(): void; returnValue?: unknown }) => {
      if (!this.requests.hasInflightMutations()) return;
      e.preventDefault();
      const message = "Changes you made may not be saved. Are you sure you want to leave?";
      e.returnValue = message;
      return message;
    };
    w.addEventListener("beforeunload", handler);
    return () => w.removeEventListener?.("beforeunload", handler);
  }

  /** @internal Exposed for tests and devtools. */
  get _debug(): { queryIds: Map<QueryToken, QueryId> } {
    const queryIds = new Map<QueryToken, QueryId>();
    for (const q of this.localQueries.queries()) queryIds.set(q.token, q.queryId);
    return { queryIds };
  }
}

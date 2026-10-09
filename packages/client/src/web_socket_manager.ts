import { CLIENT_VERSION, type ClientMessage, type ServerMessage } from "./protocol.js";
import type { Logger } from "./logging.js";

/** Silence threshold: the server pings every 15s, so 30s without any frame means the socket is dead. */
export const SERVER_INACTIVITY_TIMEOUT_MS = 30_000;
const INITIAL_BACKOFF_MS = 250;
const MAX_BACKOFF_MS = 16_000;

export type SocketState = "disconnected" | "connecting" | "ready" | "stopped";

export interface WebSocketManagerCallbacks {
  /**
   * The socket is open, `Connect` has been sent, and the manager is not paused. The client must now (re)build the
   * server state for this socket: Authenticate, ModifyQuerySet, pending mutations.
   */
  onOpen(): void;
  /** `resume()` was called while the socket was already initialized: the client may flush anything it held back. */
  onResume(): void;
  /** A parsed server message (Ping is answered internally and also forwarded). */
  onMessage(message: ServerMessage): void;
  /** The current socket is gone (closed by either side). Called once per socket that reached `connecting`. */
  onClose(reason: string, wasOpen: boolean): void;
  /** Any observable state change (for connection-state subscribers). */
  onStateChange(): void;
}

export interface WebSocketManagerOptions {
  url: string;
  sessionId: string;
  webSocketConstructor: typeof WebSocket;
  logger: Logger;
}

/**
 * Owns the WebSocket: connecting, the `Connect` handshake, the server-silence watchdog, Pong replies, and
 * reconnecting with exponential backoff and jitter.
 *
 * Outgoing traffic is only possible while the socket is `ready` and not paused. While a reconnect is underway (the
 * socket is `connecting` or `disconnected`) nothing is sent: the client keeps its state locally and rebuilds the
 * server's view of it in `onOpen` on the next socket. `pause()` additionally holds back `onOpen` (or lets the client
 * hold back its own sends) while the client fetches an auth token, so that queries are not run with stale identity.
 */
export class WebSocketManager {
  private readonly url: string;
  private readonly sessionId: string;
  private readonly WS: typeof WebSocket;
  private readonly logger: Logger;
  private readonly callbacks: WebSocketManagerCallbacks;

  private ws: WebSocket | null = null;
  private _state: SocketState = "disconnected";
  /** True once `Connect` was sent and `onOpen` delivered for the current socket. */
  private initialized = false;
  private paused = false;
  private _connectionCount = -1;
  private _hasEverConnected = false;
  private lastCloseReason: string | null = null;
  private retries = 0;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private watchdogTimer: ReturnType<typeof setTimeout> | null = null;
  private readonly removeEnvListeners: () => void;

  constructor(options: WebSocketManagerOptions, callbacks: WebSocketManagerCallbacks) {
    this.url = options.url;
    this.sessionId = options.sessionId;
    this.WS = options.webSocketConstructor;
    this.logger = options.logger;
    this.callbacks = callbacks;
    this.removeEnvListeners = this.installEnvListeners();
  }

  /** Opens the first socket. */
  start(): void {
    if (this.ws === null && this.reconnectTimer === null) this.connect();
  }

  get state(): SocketState {
    return this._state;
  }
  get connectionCount(): number {
    return Math.max(this._connectionCount, 0);
  }
  get hasEverConnected(): boolean {
    return this._hasEverConnected;
  }
  get connectionRetries(): number {
    return this.retries;
  }
  get isPaused(): boolean {
    return this.paused;
  }

  /** True when a message handed to `send` would go out right now. */
  canSend(): boolean {
    return this._state === "ready" && this.initialized && !this.paused;
  }

  /** Sends if possible; returns whether the message was sent. */
  send(message: ClientMessage): boolean {
    if (!this.canSend()) {
      this.logger.debug("not sent (socket not ready):", message.type);
      return false;
    }
    return this.rawSend(message);
  }

  /** Called by the client once the server has proven healthy (a Transition was applied): resets the backoff. */
  markHealthy(): void {
    this.retries = 0;
  }

  /** Holds back `onOpen` and client sends until `resume()`. Pings are still answered. */
  pause(): void {
    if (this._state === "stopped") return;
    this.paused = true;
    this.callbacks.onStateChange();
  }

  resume(): void {
    if (!this.paused) return;
    this.paused = false;
    if (this._state === "ready") {
      if (!this.initialized) {
        this.initialized = true;
        this.callbacks.onOpen();
      } else {
        this.callbacks.onResume();
      }
    }
    this.callbacks.onStateChange();
  }

  /** Drops the current socket and reconnects after a backoff delay. */
  reconnect(reason: string): void {
    if (this._state === "stopped") return;
    this.logger.debug(`reconnecting: ${reason}`);
    this.closeCurrent(reason);
    this.scheduleReconnect(reason);
  }

  /** Permanently closes the socket. */
  stop(): void {
    if (this._state === "stopped") return;
    this.closeCurrent("ClientClosed");
    this.clearReconnectTimer();
    this._state = "stopped";
    this.removeEnvListeners();
    this.callbacks.onStateChange();
  }

  // -------------------------------------------------------------------------------------------------------------

  private rawSend(message: ClientMessage): boolean {
    const ws = this.ws;
    if (!ws) return false;
    try {
      this.logger.debug("->", message);
      ws.send(JSON.stringify(message));
      return true;
    } catch (e) {
      this.logger.warn("WebSocket send failed", e);
      this.reconnect("SendFailed");
      return false;
    }
  }

  private connect(): void {
    if (this._state === "stopped") return;
    this.clearReconnectTimer();
    let ws: WebSocket;
    try {
      ws = new this.WS(this.url);
    } catch (e) {
      this.logger.warn("failed to construct WebSocket", e);
      this._state = "disconnected";
      this.scheduleReconnect("ConstructorFailed");
      return;
    }
    this.ws = ws;
    this._state = "connecting";
    this.initialized = false;
    this._connectionCount += 1;
    this.callbacks.onStateChange();

    ws.onopen = () => {
      if (this.ws !== ws) return;
      this._state = "ready";
      this._hasEverConnected = true;
      this.resetWatchdog();
      this.rawSend({
        type: "Connect",
        sessionId: this.sessionId,
        connectionCount: this._connectionCount,
        lastCloseReason: this.lastCloseReason,
        clientVersion: CLIENT_VERSION,
      });
      if (!this.paused) {
        this.initialized = true;
        this.callbacks.onOpen();
      }
      this.callbacks.onStateChange();
    };
    ws.onmessage = (event: MessageEvent) => {
      if (this.ws !== ws) return;
      this.resetWatchdog();
      let message: ServerMessage;
      try {
        message = JSON.parse(typeof event.data === "string" ? event.data : String(event.data)) as ServerMessage;
      } catch (e) {
        this.logger.warn("received malformed frame", e);
        this.reconnect("MalformedMessage");
        return;
      }
      this.logger.debug("<-", message);
      if (message.type === "Ping") this.rawSend({ type: "Pong" });
      this.callbacks.onMessage(message);
    };
    ws.onerror = () => {
      // An error is always followed by a close event; reconnection is handled there.
      if (this.ws === ws) this.logger.debug("WebSocket error");
    };
    ws.onclose = (event: CloseEvent) => {
      if (this.ws !== ws) return;
      const reason = event?.reason ? `${event.code}: ${event.reason}` : `closed (${event?.code ?? "?"})`;
      this.closeCurrent(reason);
      this.scheduleReconnect(reason);
    };
  }

  /** Detaches and closes the current socket (if any), reporting `onClose` once. */
  private closeCurrent(reason: string): void {
    this.clearWatchdog();
    const ws = this.ws;
    if (!ws) return;
    const wasOpen = this._state === "ready";
    this.ws = null;
    this.initialized = false;
    this._state = "disconnected";
    this.lastCloseReason = reason;
    ws.onopen = null;
    ws.onmessage = null;
    ws.onerror = null;
    ws.onclose = null;
    try {
      ws.close();
    } catch {
      // ignore
    }
    this.callbacks.onClose(reason, wasOpen);
    this.callbacks.onStateChange();
  }

  private scheduleReconnect(reason: string): void {
    if (this._state === "stopped" || this.reconnectTimer !== null) return;
    const delay = this.nextBackoff();
    this.retries += 1;
    this.logger.info(`connection lost (${reason}); reconnecting in ${Math.round(delay)}ms`);
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      this.connect();
    }, delay);
    this.callbacks.onStateChange();
  }

  /** Exponential backoff with "equal jitter": half fixed, half random. */
  private nextBackoff(): number {
    const cap = Math.min(MAX_BACKOFF_MS, INITIAL_BACKOFF_MS * 2 ** Math.min(this.retries, 16));
    return cap / 2 + Math.random() * (cap / 2);
  }

  private clearReconnectTimer(): void {
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
  }

  private resetWatchdog(): void {
    this.clearWatchdog();
    this.watchdogTimer = setTimeout(() => {
      this.watchdogTimer = null;
      this.reconnect("ServerInactivity");
    }, SERVER_INACTIVITY_TIMEOUT_MS);
  }

  private clearWatchdog(): void {
    if (this.watchdogTimer !== null) {
      clearTimeout(this.watchdogTimer);
      this.watchdogTimer = null;
    }
  }

  /** Skip the backoff wait when the browser comes back online or the tab becomes visible. */
  private installEnvListeners(): () => void {
    const g = globalThis as {
      addEventListener?: (t: string, l: () => void) => void;
      removeEventListener?: (t: string, l: () => void) => void;
      document?: { visibilityState?: string };
    };
    if (typeof g.addEventListener !== "function" || typeof g.removeEventListener !== "function") return () => {};
    const kick = () => {
      if (g.document?.visibilityState === "hidden") return;
      if (this._state === "disconnected" && this.reconnectTimer !== null) {
        this.connect();
      }
    };
    g.addEventListener("online", kick);
    g.addEventListener("visibilitychange", kick);
    return () => {
      g.removeEventListener?.("online", kick);
      g.removeEventListener?.("visibilitychange", kick);
    };
  }
}

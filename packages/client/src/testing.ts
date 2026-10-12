/**
 * Test utilities: a scriptable fake of the RxFerrix sync endpoint (`@rxferrix/client/testing`).
 *
 * ```ts
 * const server = new FakeServer();
 * const client = new RxFerrixClient("http://localhost", { webSocketConstructor: server.WebSocket });
 * server.open();                       // the socket connects
 * await flushMicrotasks();             // batched ModifyQuerySet goes out
 * server.sent;                         // what the client sent on this socket
 * server.transition({ querySet: 1, ts: 1 }, [{ type: "QueryUpdated", queryId: 0, value: [] }]);
 * ```
 */
import type { ClientMessage, ServerMessage, StateVersion } from "./protocol.js";

type Modification = Extract<ServerMessage, { type: "Transition" }>["modifications"][number];

export class FakeSocket {
  static readonly CONNECTING = 0;
  static readonly OPEN = 1;
  static readonly CLOSING = 2;
  static readonly CLOSED = 3;

  readyState = FakeSocket.CONNECTING;
  readonly sent: ClientMessage[] = [];
  closedByClient = false;
  /** The server-side StateVersion of this socket, advanced by `transition`. */
  version: StateVersion = { querySet: 0, identity: 0, ts: 0 };

  onopen: ((ev: unknown) => void) | null = null;
  onmessage: ((ev: { data: string }) => void) | null = null;
  onclose: ((ev: { code: number; reason: string }) => void) | null = null;
  onerror: ((ev: unknown) => void) | null = null;

  constructor(readonly url: string) {}

  send(data: string): void {
    if (this.readyState !== FakeSocket.OPEN) throw new Error("send on a socket that is not open");
    this.sent.push(JSON.parse(data) as ClientMessage);
  }

  close(): void {
    if (this.readyState === FakeSocket.CLOSED) return;
    this.closedByClient = true;
    this.readyState = FakeSocket.CLOSED;
  }
}

export class FakeServer {
  readonly sockets: FakeSocket[] = [];
  readonly WebSocket: typeof WebSocket;

  constructor() {
    const sockets = this.sockets;
    class BoundFakeSocket extends FakeSocket {
      constructor(url: string) {
        super(url);
        sockets.push(this);
      }
    }
    this.WebSocket = BoundFakeSocket as unknown as typeof WebSocket;
  }

  /** The most recently created socket. */
  get socket(): FakeSocket {
    const s = this.sockets[this.sockets.length - 1];
    if (!s) throw new Error("no socket created yet");
    return s;
  }

  /** Everything the client sent on the current socket. */
  get sent(): ClientMessage[] {
    return this.socket.sent;
  }

  /** Messages of one type sent on the current socket. */
  sentOfType<T extends ClientMessage["type"]>(type: T): Extract<ClientMessage, { type: T }>[] {
    return this.sent.filter((m): m is Extract<ClientMessage, { type: T }> => m.type === type);
  }

  /** Returns and forgets what the client sent on the current socket so far. */
  drain(): ClientMessage[] {
    return this.socket.sent.splice(0);
  }

  open(): void {
    const s = this.socket;
    s.readyState = FakeSocket.OPEN;
    s.onopen?.({});
  }

  push(message: ServerMessage | Record<string, unknown>): void {
    const s = this.socket;
    if (s.readyState !== FakeSocket.OPEN) throw new Error("push on a socket that is not open");
    s.onmessage?.({ data: JSON.stringify(message) });
  }

  /**
   * Pushes a Transition from the socket's current version. `end` overrides fields of the end version; unspecified
   * fields keep their current value.
   */
  transition(end: Partial<StateVersion>, modifications: Modification[] = []): void {
    const s = this.socket;
    const startVersion = { ...s.version };
    const endVersion = { ...s.version, ...end };
    s.version = endVersion;
    this.push({ type: "Transition", startVersion, endVersion, modifications });
  }

  /** The server closes the current socket. */
  close(code = 1011, reason = "server closed"): void {
    const s = this.socket;
    s.readyState = FakeSocket.CLOSED;
    s.onclose?.({ code, reason });
  }
}

/** Lets queued microtasks (query-set flush, promise continuations) run. */
export async function flushMicrotasks(): Promise<void> {
  for (let i = 0; i < 5; i++) await Promise.resolve();
}

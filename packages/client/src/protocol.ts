/**
 * Wire types for the Ferrix sync protocol (v1). See `docs/PROTOCOL.md` in the repository; that document is normative.
 */

export type QueryId = number;
export type RequestId = number;
export type JSONValue = null | boolean | number | string | JSONValue[] | { [key: string]: JSONValue };

export const CLIENT_VERSION = "0.1.0";

export interface StateVersion {
  querySet: number;
  identity: number;
  ts: number;
}

/** `{ code, data }` attached to user errors. */
export interface ErrorData {
  code: string;
  data?: unknown;
}

// ---------------------------------------------------------------------------------------------------------------
// Client -> server

export interface ConnectMessage {
  type: "Connect";
  sessionId: string;
  connectionCount: number;
  lastCloseReason: string | null;
  clientVersion: string;
}

export type QuerySetModification =
  | { type: "Add"; queryId: QueryId; udfPath: string; args: Record<string, unknown> }
  | { type: "Remove"; queryId: QueryId };

export interface ModifyQuerySetMessage {
  type: "ModifyQuerySet";
  baseVersion: number;
  newVersion: number;
  modifications: QuerySetModification[];
}

export interface MutationMessage {
  type: "Mutation";
  requestId: RequestId;
  udfPath: string;
  args: Record<string, unknown>;
}

export interface ActionMessage {
  type: "Action";
  requestId: RequestId;
  udfPath: string;
  args: Record<string, unknown>;
}

export interface AuthenticateMessage {
  type: "Authenticate";
  baseVersion: number;
  token: string | null;
}

export interface PongMessage {
  type: "Pong";
}

export type ClientMessage =
  | ConnectMessage
  | ModifyQuerySetMessage
  | MutationMessage
  | ActionMessage
  | AuthenticateMessage
  | PongMessage;

// ---------------------------------------------------------------------------------------------------------------
// Server -> client

export type StateModification =
  | { type: "QueryUpdated"; queryId: QueryId; value: unknown; logLines?: string[] }
  | {
      type: "QueryFailed";
      queryId: QueryId;
      errorMessage: string;
      errorData?: ErrorData | null;
      logLines?: string[];
    }
  | { type: "QueryRemoved"; queryId: QueryId };

export interface TransitionMessage {
  type: "Transition";
  startVersion: StateVersion;
  endVersion: StateVersion;
  modifications: StateModification[];
}

export type MutationResponseMessage =
  | { type: "MutationResponse"; requestId: RequestId; success: true; result: unknown; ts: number; logLines?: string[] }
  | {
      type: "MutationResponse";
      requestId: RequestId;
      success: false;
      errorMessage: string;
      errorData?: ErrorData | null;
      logLines?: string[];
    };

export type ActionResponseMessage =
  | { type: "ActionResponse"; requestId: RequestId; success: true; result: unknown; logLines?: string[] }
  | {
      type: "ActionResponse";
      requestId: RequestId;
      success: false;
      errorMessage: string;
      errorData?: ErrorData | null;
      logLines?: string[];
    };

export interface FatalErrorMessage {
  type: "FatalError";
  error: string;
}

export interface AuthErrorMessage {
  type: "AuthError";
  error: string;
}

export interface PingMessage {
  type: "Ping";
}

export type ServerMessage =
  | TransitionMessage
  | MutationResponseMessage
  | ActionResponseMessage
  | FatalErrorMessage
  | AuthErrorMessage
  | PingMessage;

export function versionsEqual(a: StateVersion, b: StateVersion): boolean {
  return a.querySet === b.querySet && a.identity === b.identity && a.ts === b.ts;
}

export function formatVersion(v: StateVersion): string {
  return `{querySet:${v.querySet},identity:${v.identity},ts:${v.ts}}`;
}

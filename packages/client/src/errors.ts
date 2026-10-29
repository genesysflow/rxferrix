import type { ErrorData } from "./protocol.js";

/**
 * An error raised by a RxFerrix function or by the client itself.
 *
 * For a server `UserError`, `code` and `data` carry the error's `errorData` (`{ code, data }`). For any other server
 * error, `code` and `data` are undefined and `message` is the server's `errorMessage` (`"Server Error"` outside
 * `--dev`).
 */
export class RxFerrixError extends Error {
  override name = "RxFerrixError";
  readonly code?: string;
  readonly data?: unknown;

  constructor(message: string, options?: { code?: string; data?: unknown; cause?: unknown }) {
    super(message, options?.cause !== undefined ? { cause: options.cause } : undefined);
    if (options?.code !== undefined) this.code = options.code;
    if (options?.data !== undefined) this.data = options.data;
  }

  /** Builds an error from a server failure payload (`errorMessage` + optional `errorData`). */
  static fromServer(errorMessage: string | undefined, errorData: ErrorData | null | undefined): RxFerrixError {
    const message = typeof errorMessage === "string" && errorMessage.length > 0 ? errorMessage : "Server Error";
    if (errorData && typeof errorData === "object") {
      return new RxFerrixError(message, {
        code: typeof errorData.code === "string" ? errorData.code : undefined,
        data: errorData.data,
      });
    }
    return new RxFerrixError(message);
  }
}

/** Code used for client-side failures (lost connection, client closed). */
export const CONNECTION_LOST = "ConnectionLost";
export const CLIENT_CLOSED = "ClientClosed";
/** A mutation sent before a connection was lost couldn't be sent again safely: it may or may not have run. */
export const OUTCOME_UNKNOWN = "OutcomeUnknown";

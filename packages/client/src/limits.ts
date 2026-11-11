import { RxFerrixError } from "./errors.js";

/**
 * The server's limit on one message, in UTF-8 bytes (docs/PROTOCOL.md, Limits). A larger one is answered with a
 * `FatalError` and the socket is closed, and since the client sends again what the server never answered, the same
 * message would end every socket after it. So nothing that large is ever queued.
 */
export const MAX_MESSAGE_BYTES = 2 * 1024 * 1024;
/**
 * The server's limit on the arguments of all the queries one socket watches, as JSON (docs/PROTOCOL.md, Limits). The
 * query that crosses it is answered with a `FatalError`, and every new socket would re-add the same set and get the
 * same answer: the query is refused here instead.
 */
export const MAX_QUERY_ARGS_BYTES = 8 * 1024 * 1024;
/** Room kept in a message for what goes around the arguments: its type, ids and the function's path. */
const ENVELOPE_BYTES = 1024;
/**
 * How much of a `ModifyQuerySet` its queries' paths and arguments may take, in characters of their canonical JSON.
 * A query set is re-sent whole on every new socket; this splits it into messages well under the limit (a character
 * is at most three bytes, and escaping can add a little).
 */
export const MODIFY_QUERY_SET_CHARS = 400_000;

/** The length of `s` as UTF-8. */
export function utf8Length(s: string): number {
  let bytes = 0;
  for (let i = 0; i < s.length; i++) {
    const c = s.charCodeAt(i);
    if (c < 0x80) bytes += 1;
    else if (c < 0x800) bytes += 2;
    else if (c >= 0xd800 && c <= 0xdbff && i + 1 < s.length) {
      // A surrogate pair: four bytes for the two code units.
      bytes += 4;
      i++;
    } else bytes += 3;
  }
  return bytes;
}

/** About what a `Remove` takes in a `ModifyQuerySet`, in characters. */
export const REMOVE_CHARS = 40;

/** The error for one query too many, by the size of their arguments ({@link MAX_QUERY_ARGS_BYTES}). */
export function tooMuchWatched(udfPath: string): RxFerrixError {
  return new RxFerrixError(
    `Can't watch "${udfPath}" as well: the queries watched at once may hold at most ` +
      `${MAX_QUERY_ARGS_BYTES >> 20} MiB of arguments`,
    { code: "args_too_large" },
  );
}

/** The error for a call whose arguments (`json`: their JSON) don't fit in one message, or null. */
export function tooLarge(udfPath: string, json: string): RxFerrixError | null {
  const room = MAX_MESSAGE_BYTES - ENVELOPE_BYTES - udfPath.length;
  // Each character is one to three bytes: only what is in between needs counting.
  if (json.length * 3 <= room) return null;
  if (json.length <= room && utf8Length(json) <= room) return null;
  return new RxFerrixError(
    `The arguments of "${udfPath}" are too large: a message to the server holds at most ${MAX_MESSAGE_BYTES >> 20} MiB`,
    { code: "args_too_large" },
  );
}

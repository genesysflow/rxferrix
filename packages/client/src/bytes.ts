/**
 * Binary values: `ferrix::Bytes` travels (and is stored) as `{ $bytes: "<base64>" }`, which is also its generated
 * TypeScript type. These helpers convert between that form and `Uint8Array`.
 *
 * ```ts
 * await client.mutation(api.files.save_thumbnail, { id, thumb: bytesToJson(new Uint8Array(png)) });
 * const doc = await client.query(api.files.get, { id });
 * const png: Uint8Array = bytesFromJson(doc.thumb);
 * ```
 */

/** The JSON form of `ferrix::Bytes`: standard, padded base64. */
export type Bytes = { $bytes: string };

const CHUNK = 0x8000;

/** Wrap binary data as `{ $bytes: "<base64>" }` (for args and documents). */
export function bytesToJson(data: Uint8Array | ArrayBuffer | ArrayBufferView): Bytes {
  const bytes =
    data instanceof Uint8Array
      ? data
      : ArrayBuffer.isView(data)
        ? new Uint8Array(data.buffer, data.byteOffset, data.byteLength)
        : new Uint8Array(data);
  let binary = "";
  for (let i = 0; i < bytes.length; i += CHUNK) {
    binary += String.fromCharCode(...bytes.subarray(i, i + CHUNK));
  }
  return { $bytes: btoa(binary) };
}

/** The bytes of a `{ $bytes: "<base64>" }` value (from a query result or document). Throws a `TypeError` otherwise. */
export function bytesFromJson(value: Bytes): Uint8Array {
  if (!isBytes(value)) {
    throw new TypeError("expected bytes as { $bytes: string }");
  }
  let binary: string;
  try {
    binary = atob(value.$bytes);
  } catch {
    throw new TypeError("`$bytes` is not valid base64");
  }
  const out = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) {
    out[i] = binary.charCodeAt(i);
  }
  return out;
}

/** True for a value in the `{ $bytes: string }` form (an object with exactly that key). */
export function isBytes(value: unknown): value is Bytes {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    return false;
  }
  const keys = Object.keys(value);
  return keys.length === 1 && keys[0] === "$bytes" && typeof (value as Bytes).$bytes === "string";
}

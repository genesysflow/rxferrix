import type { FerrixError } from "./errors.js";

/**
 * The local state of a query. Objects are reused while the visible value does not change, so they can be compared
 * by reference (e.g. as a `useSyncExternalStore` snapshot).
 */
export type QueryState<T = unknown> =
  | { readonly status: "loading" }
  | { readonly status: "success"; readonly value: T }
  | { readonly status: "error"; readonly error: FerrixError };

export const LOADING: QueryState<never> = Object.freeze({ status: "loading" as const });

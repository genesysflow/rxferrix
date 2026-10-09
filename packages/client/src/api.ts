/**
 * Function references: typed handles to server functions.
 *
 * Codegen emits an `api` object whose leaves are created with {@link makeFunctionReference}:
 *
 * ```ts
 * export const api = {
 *   todos: {
 *     list: makeFunctionReference<"query", { done?: boolean | null }, Doc<"todos">[]>("query", "todos:list"),
 *   },
 * };
 * ```
 */

/** The three kinds of public server functions. */
export type FunctionKind = "query" | "mutation" | "action";

/**
 * A typed reference to a server function.
 *
 * Only `_kind` and `_path` exist at runtime. `__args` and `__output` are phantom fields that carry the argument and
 * return types for inference; they are never present on the object.
 */
export interface FunctionReference<
  Kind extends FunctionKind = FunctionKind,
  Args = any,
  Output = any,
> {
  readonly _kind: Kind;
  readonly _path: string;
  /** Phantom: never present at runtime. */
  readonly __args: Args;
  /** Phantom: never present at runtime. */
  readonly __output: Output;
}

/** Any reference to a query. */
export type AnyQueryReference = FunctionReference<"query", any, any>;
/** Any reference to a mutation. */
export type AnyMutationReference = FunctionReference<"mutation", any, any>;
/** Any reference to an action. */
export type AnyActionReference = FunctionReference<"action", any, any>;

/** Creates a function reference. Used by generated code; rarely needed by hand. */
export function makeFunctionReference<Kind extends FunctionKind, Args = Record<string, never>, Output = unknown>(
  kind: Kind,
  path: string,
): FunctionReference<Kind, Args, Output> {
  return Object.freeze({ _kind: kind, _path: path }) as unknown as FunctionReference<Kind, Args, Output>;
}

/** The argument object type of a function reference. */
export type ArgsOf<F> = F extends FunctionReference<any, infer A, any> ? A : never;

/** The return type of a function reference. */
export type OutputOf<F> = F extends FunctionReference<any, any, infer O> ? O : never;

/** True when every key of `Args` is optional (so the whole object may be omitted). */
export type HasNoRequiredArgs<Args> = {} extends Args ? true : false;

/**
 * Rest-parameter tuple for a function's args: optional when the args object has no required keys.
 *
 * `fn(ref, ...args: OptionalArgs<typeof ref>)`
 */
export type OptionalArgs<F> = HasNoRequiredArgs<ArgsOf<F>> extends true
  ? [args?: ArgsOf<F>]
  : [args: ArgsOf<F>];

/** Like {@link OptionalArgs} but also accepts `"skip"` in place of the args. */
export type OptionalArgsOrSkip<F> = HasNoRequiredArgs<ArgsOf<F>> extends true
  ? [args?: ArgsOf<F> | "skip"]
  : [args: ArgsOf<F> | "skip"];

/** Args followed by an optional options object; the args are optional when they have no required keys. */
export type ArgsAndOptions<F, Options> = HasNoRequiredArgs<ArgsOf<F>> extends true
  ? [args?: ArgsOf<F>, options?: Options]
  : [args: ArgsOf<F>, options?: Options];

/** A document ID for the given table. At runtime it is a plain string. */
export type Id<TableName extends string> = string & { __tableName: TableName };

/** Returns the `udfPath` (e.g. `"todos:list"`) of a function reference. */
export function getFunctionName(ref: FunctionReference): string {
  return ref._path;
}

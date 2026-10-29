import { createSubscriber } from "svelte/reactivity";
import {
  canonicalJSON,
  type ArgsOf,
  type RxFerrixClient,
  type RxFerrixError,
  type FunctionReference,
  type OutputOf,
  type QueryState,
} from "@rxferrix/client";
import { getRxFerrixClient } from "./context.js";

/** The reactive result of {@link useQuery}. Read its fields in templates, `$derived` or `$effect`. */
export interface UseQueryResult<T> {
  /** The latest value; `undefined` while loading, after an error, or when skipped. */
  readonly data: T | undefined;
  /** The query's error, if its latest result is an error. */
  readonly error: RxFerrixError | undefined;
  /** True until the first result arrives (false when skipped, or while showing previous data). */
  readonly isLoading: boolean;
  /** With `keepPreviousData`: `data` is the previous args' value, shown while the current args load. */
  readonly isStale: boolean;
}

/** Args thunk: re-evaluated reactively; return `"skip"` to not subscribe. */
export type ArgsThunk<Q extends FunctionReference<"query">> = () => ArgsOf<Q> | "skip";

const SKIPPED = Object.freeze({ status: "skipped" as const });

export interface UseQueryOptions {
  /** Use this client instead of the one from context (allows calling `useQuery` outside component init). */
  client?: RxFerrixClient;
  /**
   * While the query loads for new args, keep showing the last value it had (for the previous args) instead of
   * `undefined`, with `isStale` true: a search box or a pager doesn't flash empty between results.
   */
  keepPreviousData?: boolean;
}

/**
 * Subscribes to a query.
 *
 * ```svelte
 * <script lang="ts">
 *   let { channel } = $props();
 *   const messages = useQuery(api.messages.list, () => ({ channel }));
 * </script>
 * {#if messages.isLoading}Loading…{:else if messages.error}{messages.error.message}{:else}…{/if}
 * ```
 *
 * The args thunk is tracked: when the values it reads change, the subscription switches to the new args (args that
 * are equal by canonical JSON keep the existing subscription). The server subscription is active while the result
 * is read by at least one effect (e.g. the template) and is torn down when those effects are destroyed, for
 * example when the component unmounts.
 */
export function useQuery<Q extends FunctionReference<"query">>(
  ref: Q,
  args: ArgsThunk<Q> = () => ({}) as ArgsOf<Q>,
  options: UseQueryOptions = {},
): UseQueryResult<OutputOf<Q>> {
  const client = options.client ?? getRxFerrixClient();

  // A string, so that equal args (even as fresh objects) do not invalidate anything downstream.
  const argsKey = $derived.by(() => {
    const a = args();
    return a === "skip" ? null : canonicalJSON(a ?? {});
  });

  // Creating a watch has no side effects; the subscription only starts when `subscribe()` runs inside an effect.
  const watch = $derived(argsKey === null ? null : client.watchQuery(ref, JSON.parse(argsKey) as ArgsOf<Q>));

  const subscribe = $derived.by(() => {
    const w = watch;
    return w ? createSubscriber((update) => w.onUpdate(update)) : null;
  });

  const live = (): QueryState<OutputOf<Q>> | typeof SKIPPED => {
    const s = subscribe;
    const w = watch;
    s?.();
    return w ? w.localQueryState() : SKIPPED;
  };
  // The last success, for `keepPreviousData` (plain state: reading it doesn't track anything).
  let previous: { value: OutputOf<Q> } | null = null;
  const current = (): { state: QueryState<OutputOf<Q>> | typeof SKIPPED; stale: boolean } => {
    const state = live();
    if (state.status !== "loading") previous = state.status === "success" ? { value: state.value } : null;
    else if (options.keepPreviousData && previous) return { state: { status: "success", value: previous.value }, stale: true };
    return { state, stale: false };
  };

  return {
    get data() {
      const { state } = current();
      return state.status === "success" ? state.value : undefined;
    },
    get error() {
      const { state } = current();
      return state.status === "error" ? state.error : undefined;
    },
    get isLoading() {
      return current().state.status === "loading";
    },
    get isStale() {
      return current().stale;
    },
  };
}

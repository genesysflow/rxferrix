import { createSubscriber } from "svelte/reactivity";
import {
  canonicalJSON,
  type ArgsOf,
  type FerrixClient,
  type FerrixError,
  type FunctionReference,
  type OutputOf,
  type QueryState,
} from "@ferrix/client";
import { getFerrixClient } from "./context.js";

/** The reactive result of {@link useQuery}. Read its fields in templates, `$derived` or `$effect`. */
export interface UseQueryResult<T> {
  /** The latest value; `undefined` while loading, after an error, or when skipped. */
  readonly data: T | undefined;
  /** The query's error, if its latest result is an error. */
  readonly error: FerrixError | undefined;
  /** True until the first result arrives (false when skipped). */
  readonly isLoading: boolean;
}

/** Args thunk: re-evaluated reactively; return `"skip"` to not subscribe. */
export type ArgsThunk<Q extends FunctionReference<"query">> = () => ArgsOf<Q> | "skip";

const SKIPPED = Object.freeze({ status: "skipped" as const });

export interface UseQueryOptions {
  /** Use this client instead of the one from context (allows calling `useQuery` outside component init). */
  client?: FerrixClient;
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
  const client = options.client ?? getFerrixClient();

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

  const current = (): QueryState<OutputOf<Q>> | typeof SKIPPED => {
    const s = subscribe;
    const w = watch;
    s?.();
    return w ? w.localQueryState() : SKIPPED;
  };

  return {
    get data() {
      const s = current();
      return s.status === "success" ? s.value : undefined;
    },
    get error() {
      const s = current();
      return s.status === "error" ? s.error : undefined;
    },
    get isLoading() {
      return current().status === "loading";
    },
  };
}

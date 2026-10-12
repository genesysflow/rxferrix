import { createSubscriber } from "svelte/reactivity";
import type { ConnectionState, RxFerrixClient } from "@rxferrix/client";
import { getRxFerrixClient } from "./context.js";

/** Reactive connection state: read `.current` in a template, `$derived` or `$effect`. */
export function useConnectionState(options: { client?: RxFerrixClient } = {}): { readonly current: ConnectionState } {
  const client = options.client ?? getRxFerrixClient();
  const subscribe = createSubscriber((update) => client.subscribeToConnectionState(update));
  return {
    get current() {
      subscribe();
      return client.connectionState();
    },
  };
}

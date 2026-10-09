import { createSubscriber } from "svelte/reactivity";
import type { ConnectionState, FerrixClient } from "@ferrix/client";
import { getFerrixClient } from "./context.js";

/** Reactive connection state: read `.current` in a template, `$derived` or `$effect`. */
export function useConnectionState(options: { client?: FerrixClient } = {}): { readonly current: ConnectionState } {
  const client = options.client ?? getFerrixClient();
  const subscribe = createSubscriber((update) => client.subscribeToConnectionState(update));
  return {
    get current() {
      subscribe();
      return client.connectionState();
    },
  };
}

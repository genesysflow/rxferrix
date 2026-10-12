import { getContext, hasContext, onDestroy, setContext } from "svelte";
import { RxFerrixClient, type RxFerrixClientOptions } from "@rxferrix/client";

const CLIENT_KEY = Symbol.for("@rxferrix/svelte:client");

/**
 * Puts a RxFerrix client into Svelte context for this component and its descendants. Call it during component
 * initialization (typically in the root layout).
 *
 * Given a URL, a new client is created and closed again when the component is destroyed. Given a client, it is used
 * as is and its lifetime is the caller's responsibility.
 */
export function setupRxFerrix(urlOrClient: string | RxFerrixClient, options?: RxFerrixClientOptions): RxFerrixClient {
  const client = typeof urlOrClient === "string" ? new RxFerrixClient(urlOrClient, options) : urlOrClient;
  setContext(CLIENT_KEY, client);
  if (typeof urlOrClient === "string") onDestroy(() => client.close());
  return client;
}

/** The client set up by {@link setupRxFerrix} in an ancestor component. Must be called during component init. */
export function getRxFerrixClient(): RxFerrixClient {
  if (!hasContext(CLIENT_KEY)) {
    throw new Error("No RxFerrix client found in context. Call setupRxFerrix(url) in a parent component.");
  }
  return getContext<RxFerrixClient>(CLIENT_KEY);
}

import { getContext, hasContext, onDestroy, setContext } from "svelte";
import { FerrixClient, type FerrixClientOptions } from "@ferrix/client";

const CLIENT_KEY = Symbol.for("@ferrix/svelte:client");

/**
 * Puts a Ferrix client into Svelte context for this component and its descendants. Call it during component
 * initialization (typically in the root layout).
 *
 * Given a URL, a new client is created and closed again when the component is destroyed. Given a client, it is used
 * as is and its lifetime is the caller's responsibility.
 */
export function setupFerrix(urlOrClient: string | FerrixClient, options?: FerrixClientOptions): FerrixClient {
  const client = typeof urlOrClient === "string" ? new FerrixClient(urlOrClient, options) : urlOrClient;
  setContext(CLIENT_KEY, client);
  if (typeof urlOrClient === "string") onDestroy(() => client.close());
  return client;
}

/** The client set up by {@link setupFerrix} in an ancestor component. Must be called during component init. */
export function getFerrixClient(): FerrixClient {
  if (!hasContext(CLIENT_KEY)) {
    throw new Error("No Ferrix client found in context. Call setupFerrix(url) in a parent component.");
  }
  return getContext<FerrixClient>(CLIENT_KEY);
}

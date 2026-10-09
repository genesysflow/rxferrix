import { createContext, createElement, useContext, type ReactNode } from "react";
import type { FerrixClient } from "@ferrix/client";

const FerrixContext = createContext<FerrixClient | null>(null);
FerrixContext.displayName = "FerrixContext";

export interface FerrixProviderProps {
  client: FerrixClient;
  children?: ReactNode;
}

/** Makes a {@link FerrixClient} available to the Ferrix hooks below it. */
export function FerrixProvider({ client, children }: FerrixProviderProps): ReactNode {
  return createElement(FerrixContext.Provider, { value: client }, children);
}

/** The client from the nearest {@link FerrixProvider}. Throws if there is none. */
export function useFerrix(): FerrixClient {
  const client = useContext(FerrixContext);
  if (client === null) {
    throw new Error(
      "Could not find a FerrixClient. Wrap your component tree in <FerrixProvider client={new FerrixClient(url)}>.",
    );
  }
  return client;
}

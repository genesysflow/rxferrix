import { createContext, createElement, useContext, type ReactNode } from "react";
import type { RxFerrixClient } from "@rxferrix/client";

const RxFerrixContext = createContext<RxFerrixClient | null>(null);
RxFerrixContext.displayName = "RxFerrixContext";

export interface RxFerrixProviderProps {
  client: RxFerrixClient;
  children?: ReactNode;
}

/** Makes a {@link RxFerrixClient} available to the RxFerrix hooks below it. */
export function RxFerrixProvider({ client, children }: RxFerrixProviderProps): ReactNode {
  return createElement(RxFerrixContext.Provider, { value: client }, children);
}

/** The client from the nearest {@link RxFerrixProvider}. Throws if there is none. */
export function useRxFerrix(): RxFerrixClient {
  const client = useContext(RxFerrixContext);
  if (client === null) {
    throw new Error(
      "Could not find a RxFerrixClient. Wrap your component tree in <RxFerrixProvider client={new RxFerrixClient(url)}>.",
    );
  }
  return client;
}

import { useMemo, useSyncExternalStore } from "react";
import { getAuth, systemApi, type AuthState, type RxFerrixAuth, type User } from "@rxferrix/client";
import { useRxFerrix } from "./context.js";
import { useQuery } from "./use_query.js";

/**
 * The auth state of the client from `RxFerrixProvider`: `{ isLoading, isAuthenticated, userId, isAdmin }`, re-rendering
 * when it changes. `isAuthenticated` turns true once the server has accepted the token.
 *
 * Uses `getAuth(client)`: call `getAuth(client, { storage })` before rendering to customize it.
 */
export function useRxFerrixAuth(): AuthState {
  const auth = getAuth(useRxFerrix());
  const subscribe = useMemo(() => (cb: () => void) => auth.subscribe(cb), [auth]);
  const getSnapshot = useMemo(() => () => auth.state(), [auth]);
  return useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
}

/** Sign-in, sign-out and the other auth actions, bound to the client's {@link RxFerrixAuth}. Stable across renders. */
export type AuthActions = Pick<
  RxFerrixAuth,
  | "signUp"
  | "signIn"
  | "signInAsAdmin"
  | "setupAdmin"
  | "signOut"
  | "signInWithOAuth"
  | "handleRedirectCallback"
  | "requestPasswordReset"
  | "resetPassword"
  | "changePassword"
  | "requestMagicLink"
  | "verifyCode"
  | "verifyMagicLink"
  | "requestEmailVerification"
  | "verifyEmail"
  | "getAccessToken"
  | "providers"
>;

export function useAuthActions(): AuthActions {
  const auth = getAuth(useRxFerrix());
  return useMemo(
    (): AuthActions => ({
      signUp: auth.signUp.bind(auth),
      signIn: auth.signIn.bind(auth),
      signInAsAdmin: auth.signInAsAdmin.bind(auth),
      setupAdmin: auth.setupAdmin.bind(auth),
      signOut: auth.signOut.bind(auth),
      signInWithOAuth: auth.signInWithOAuth.bind(auth),
      handleRedirectCallback: auth.handleRedirectCallback.bind(auth),
      requestPasswordReset: auth.requestPasswordReset.bind(auth),
      resetPassword: auth.resetPassword.bind(auth),
      changePassword: auth.changePassword.bind(auth),
      requestMagicLink: auth.requestMagicLink.bind(auth),
      verifyCode: auth.verifyCode.bind(auth),
      verifyMagicLink: auth.verifyMagicLink.bind(auth),
      requestEmailVerification: auth.requestEmailVerification.bind(auth),
      verifyEmail: auth.verifyEmail.bind(auth),
      getAccessToken: auth.getAccessToken.bind(auth),
      providers: auth.providers.bind(auth),
    }),
    [auth],
  );
}

/**
 * The signed-in user's document (live: profile changes re-render), `null` when signed out, `undefined` while
 * loading.
 */
export function useCurrentUser(): User | null | undefined {
  const { isLoading, isAuthenticated } = useRxFerrixAuth();
  const user = useQuery(systemApi.auth.me, isAuthenticated ? {} : "skip");
  if (isLoading) return undefined;
  if (!isAuthenticated) return null;
  return user;
}

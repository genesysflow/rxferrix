import { useMemo, useSyncExternalStore } from "react";
import { getAuth, systemApi, type AuthState, type FerrixAuth, type User } from "@ferrix/client";
import { useFerrix } from "./context.js";
import { useQuery } from "./use_query.js";

/**
 * The auth state of the client from `FerrixProvider`: `{ isLoading, isAuthenticated, userId, isAdmin }`, re-rendering
 * when it changes. `isAuthenticated` turns true once the server has accepted the token.
 *
 * Uses `getAuth(client)`: call `getAuth(client, { storage })` before rendering to customize it.
 */
export function useFerrixAuth(): AuthState {
  const auth = getAuth(useFerrix());
  const subscribe = useMemo(() => (cb: () => void) => auth.subscribe(cb), [auth]);
  const getSnapshot = useMemo(() => () => auth.state(), [auth]);
  return useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
}

/** Sign-in, sign-out and the other auth actions, bound to the client's {@link FerrixAuth}. Stable across renders. */
export type AuthActions = Pick<
  FerrixAuth,
  | "signUp"
  | "signIn"
  | "signInAsAdmin"
  | "setupAdmin"
  | "signOut"
  | "signInWithOAuth"
  | "handleRedirectCallback"
  | "requestPasswordReset"
  | "resetPassword"
  | "requestMagicLink"
  | "verifyCode"
  | "verifyMagicLink"
  | "requestEmailVerification"
  | "verifyEmail"
  | "getAccessToken"
>;

export function useAuthActions(): AuthActions {
  const auth = getAuth(useFerrix());
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
      requestMagicLink: auth.requestMagicLink.bind(auth),
      verifyCode: auth.verifyCode.bind(auth),
      verifyMagicLink: auth.verifyMagicLink.bind(auth),
      requestEmailVerification: auth.requestEmailVerification.bind(auth),
      verifyEmail: auth.verifyEmail.bind(auth),
      getAccessToken: auth.getAccessToken.bind(auth),
    }),
    [auth],
  );
}

/**
 * The signed-in user's document (live: profile changes re-render), `null` when signed out, `undefined` while
 * loading.
 */
export function useCurrentUser(): User | null | undefined {
  const { isLoading, isAuthenticated } = useFerrixAuth();
  const user = useQuery(systemApi.auth.me, isAuthenticated ? {} : "skip");
  if (isLoading) return undefined;
  if (!isAuthenticated) return null;
  return user;
}

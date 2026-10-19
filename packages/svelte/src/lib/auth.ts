import { createSubscriber } from "svelte/reactivity";
import { getAuth, systemApi, type AuthState, type RxFerrixAuth, type RxFerrixClient, type User } from "@rxferrix/client";
import { getRxFerrixClient } from "./context.js";
import { useQuery } from "./use_query.svelte.js";

/**
 * Reactive auth state plus the auth actions. Read the fields in templates, `$derived` or `$effect`:
 *
 * ```svelte
 * <script lang="ts">
 *   const auth = useAuth();
 * </script>
 * {#if auth.isLoading}…{:else if auth.isAuthenticated}<button onclick={() => auth.signOut()}>Sign out</button>
 * {:else}<button onclick={() => auth.signIn({ email, password })}>Sign in</button>{/if}
 * ```
 *
 * Uses `getAuth(client)`: call `getAuth(client, { storage })` before to customize it.
 */
export interface UseAuthResult extends AuthState {
  /** The underlying {@link RxFerrixAuth} (all actions, `getAccessToken`, `providers`, ...). */
  readonly auth: RxFerrixAuth;
  signUp: RxFerrixAuth["signUp"];
  signIn: RxFerrixAuth["signIn"];
  signInAsAdmin: RxFerrixAuth["signInAsAdmin"];
  /** The second step of a sign-in that resolved `mfaRequired`. */
  verifyMfa: RxFerrixAuth["verifyMfa"];
  setupAdmin: RxFerrixAuth["setupAdmin"];
  signOut: RxFerrixAuth["signOut"];
  signInWithOAuth: RxFerrixAuth["signInWithOAuth"];
  handleRedirectCallback: RxFerrixAuth["handleRedirectCallback"];
  requestPasswordReset: RxFerrixAuth["requestPasswordReset"];
  resetPassword: RxFerrixAuth["resetPassword"];
  changePassword: RxFerrixAuth["changePassword"];
  requestMagicLink: RxFerrixAuth["requestMagicLink"];
  verifyCode: RxFerrixAuth["verifyCode"];
  verifyMagicLink: RxFerrixAuth["verifyMagicLink"];
  requestEmailVerification: RxFerrixAuth["requestEmailVerification"];
  verifyEmail: RxFerrixAuth["verifyEmail"];
  /** The sign-in methods the server offers (`GET /api/auth/providers`), e.g. to show an OAuth button. */
  providers: RxFerrixAuth["providers"];
  getAccessToken: RxFerrixAuth["getAccessToken"];
  /** Two-factor authentication of the signed-in account. */
  getMfaStatus: RxFerrixAuth["getMfaStatus"];
  setupTotp: RxFerrixAuth["setupTotp"];
  confirmTotp: RxFerrixAuth["confirmTotp"];
  disableMfa: RxFerrixAuth["disableMfa"];
  regenerateRecoveryCodes: RxFerrixAuth["regenerateRecoveryCodes"];
}

export function useAuth(options: { client?: RxFerrixClient } = {}): UseAuthResult {
  const auth = getAuth(options.client ?? getRxFerrixClient());
  const subscribe = createSubscriber((update) => auth.subscribe(update));
  const state = () => {
    subscribe();
    return auth.state();
  };
  return {
    auth,
    get isLoading() {
      return state().isLoading;
    },
    get isAuthenticated() {
      return state().isAuthenticated;
    },
    get userId() {
      return state().userId;
    },
    get isAdmin() {
      return state().isAdmin;
    },
    signUp: auth.signUp.bind(auth),
    signIn: auth.signIn.bind(auth),
    signInAsAdmin: auth.signInAsAdmin.bind(auth),
    verifyMfa: auth.verifyMfa.bind(auth),
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
    providers: auth.providers.bind(auth),
    getAccessToken: auth.getAccessToken.bind(auth),
    getMfaStatus: auth.getMfaStatus.bind(auth),
    setupTotp: auth.setupTotp.bind(auth),
    confirmTotp: auth.confirmTotp.bind(auth),
    disableMfa: auth.disableMfa.bind(auth),
    regenerateRecoveryCodes: auth.regenerateRecoveryCodes.bind(auth),
  };
}

/**
 * The signed-in user (live), as `{ data, isLoading }`: `data` is `null` when signed out. Call during component init.
 */
export function useCurrentUser(options: { client?: RxFerrixClient } = {}): {
  readonly data: User | null | undefined;
  readonly isLoading: boolean;
} {
  const client = options.client ?? getRxFerrixClient();
  const auth = useAuth({ client });
  const me = useQuery(systemApi.auth.me, () => (auth.isAuthenticated ? {} : "skip"), { client });
  return {
    get data() {
      if (auth.isLoading) return undefined;
      return auth.isAuthenticated ? me.data : null;
    },
    get isLoading() {
      return auth.isLoading || (auth.isAuthenticated && me.isLoading);
    },
  };
}

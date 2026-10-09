import { createSubscriber } from "svelte/reactivity";
import { getAuth, systemApi, type AuthState, type FerrixAuth, type FerrixClient, type User } from "@ferrix/client";
import { getFerrixClient } from "./context.js";
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
  /** The underlying {@link FerrixAuth} (all actions, `getAccessToken`, `providers`, ...). */
  readonly auth: FerrixAuth;
  signUp: FerrixAuth["signUp"];
  signIn: FerrixAuth["signIn"];
  signInAsAdmin: FerrixAuth["signInAsAdmin"];
  setupAdmin: FerrixAuth["setupAdmin"];
  signOut: FerrixAuth["signOut"];
  signInWithOAuth: FerrixAuth["signInWithOAuth"];
  handleRedirectCallback: FerrixAuth["handleRedirectCallback"];
  requestPasswordReset: FerrixAuth["requestPasswordReset"];
  resetPassword: FerrixAuth["resetPassword"];
  requestMagicLink: FerrixAuth["requestMagicLink"];
  verifyCode: FerrixAuth["verifyCode"];
  verifyMagicLink: FerrixAuth["verifyMagicLink"];
  requestEmailVerification: FerrixAuth["requestEmailVerification"];
  verifyEmail: FerrixAuth["verifyEmail"];
  /** The sign-in methods the server offers (`GET /api/auth/providers`), e.g. to show an OAuth button. */
  providers: FerrixAuth["providers"];
  getAccessToken: FerrixAuth["getAccessToken"];
}

export function useAuth(options: { client?: FerrixClient } = {}): UseAuthResult {
  const auth = getAuth(options.client ?? getFerrixClient());
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
    providers: auth.providers.bind(auth),
    getAccessToken: auth.getAccessToken.bind(auth),
  };
}

/**
 * The signed-in user (live), as `{ data, isLoading }`: `data` is `null` when signed out. Call during component init.
 */
export function useCurrentUser(options: { client?: FerrixClient } = {}): {
  readonly data: User | null | undefined;
  readonly isLoading: boolean;
} {
  const client = options.client ?? getFerrixClient();
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

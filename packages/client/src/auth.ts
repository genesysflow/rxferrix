/**
 * Auth for a {@link RxFerrixClient}: sign-up / sign-in against the server's `/api/auth/*` endpoints, token storage,
 * automatic refresh (wired into `client.setAuth`), OAuth redirects and cross-tab sync.
 *
 * ```ts
 * const client = new RxFerrixClient("http://localhost:8090");
 * const auth = getAuth(client);                      // restores a stored session, if any
 * await auth.signIn({ email, password });
 * auth.subscribe(() => console.log(auth.state()));   // { isLoading, isAuthenticated, userId, isAdmin }
 * ```
 *
 * Tokens are kept in `localStorage` (or the `storage` you pass; an in-memory store when neither exists, e.g. SSR).
 * The access token is short-lived (15 minutes by default) and refreshed with the refresh token shortly before it
 * expires and whenever the server rejects it. Refresh tokens rotate on every use; refreshes are serialized across
 * tabs with the Web Locks API when available, and tabs pick up each other's tokens through `storage` events.
 */
import { makeFunctionReference, type Id } from "./api.js";
import type { RxFerrixClient } from "./client.js";
import { RxFerrixError } from "./errors.js";
import type { ErrorData } from "./protocol.js";

/** Where tokens are persisted. `localStorage` satisfies it; async implementations (e.g. secure storage) work too. */
export interface AuthStorage {
  getItem(key: string): string | null | Promise<string | null>;
  setItem(key: string, value: string): void | Promise<void>;
  removeItem(key: string): void | Promise<void>;
}

/** Keeps tokens in memory only (lost on reload). */
export class MemoryAuthStorage implements AuthStorage {
  private readonly items = new Map<string, string>();
  getItem(key: string): string | null {
    return this.items.get(key) ?? null;
  }
  setItem(key: string, value: string): void {
    this.items.set(key, value);
  }
  removeItem(key: string): void {
    this.items.delete(key);
  }
}

export interface RxFerrixAuthOptions {
  /** Token storage. Defaults to `localStorage` when available, else memory. */
  storage?: AuthStorage;
  /** Storage key. Defaults to `"ferrix.auth:" + client.url`. */
  storageKey?: string;
  /** `fetch` implementation (defaults to the global one). */
  fetch?: typeof fetch;
  /** Follow sign-in / sign-out / refreshes made in other tabs (via `storage` events). Default true. */
  syncTabs?: boolean;
}

/** The auth state, as shown by `useRxFerrixAuth()` / `useAuth()`. */
export interface AuthState {
  /** True until the stored session has been checked, and after a sign-in until the server confirmed the token. */
  readonly isLoading: boolean;
  /** The server has accepted the current access token. */
  readonly isAuthenticated: boolean;
  /** The signed-in user's id (also set while `isLoading` right after a sign-in). */
  readonly userId: Id<"_users"> | null;
  /** True for admin sessions (`signInAsAdmin`). */
  readonly isAdmin: boolean;
}

/** The tokens the server returns on sign-in and refresh. */
export interface AuthTokens {
  accessToken: string;
  refreshToken: string;
  /** Seconds until the access token expires. */
  expiresIn: number;
  userId?: string;
  adminId?: string;
}

/** Proof of email ownership: the token from an email link, or the email address plus the code from the email. */
export type CodeProof = { token: string } | { email: string; code: string };

/** What {@link RxFerrixAuth.handleRedirectCallback} found in the URL. */
export type RedirectResult =
  | { type: "signedIn" }
  | { type: "emailVerified" }
  /** A password reset link: ask for a new password, then call `resetPassword({ token }, password)`. */
  | { type: "passwordReset"; token: string }
  | { type: "error"; error: RxFerrixError };

/** Sign-in methods enabled on the server (`GET /api/auth/providers`). */
export interface AuthProviders {
  password: boolean;
  magicLink: boolean;
  signups: boolean;
  /** OAuth provider ids, e.g. `["github", "google"]`. */
  oauth: string[];
}

/** A user document (`_users`). Contains no secrets. */
export interface User {
  _id: Id<"_users">;
  _creationTime: number;
  email: string | null;
  emailVerified: boolean;
  name: string | null;
  image: string | null;
}

/**
 * Hand-written references to rxferrix's built-in functions (codegen leaves system functions out of `api`).
 *
 * - `systemApi.auth.me`: the signed-in user (reactive), or `null`.
 * - `systemApi.auth.updateProfile`: set `name` / `image`; omitted fields are unchanged, `""` clears.
 */
export const systemApi = {
  auth: {
    me: makeFunctionReference<"query", Record<string, never>, User | null>("query", "_system/auth:me"),
    updateProfile: makeFunctionReference<"mutation", { name?: string | null; image?: string | null }, null>(
      "mutation",
      "_system/auth:update_profile",
    ),
  },
} as const;

interface StoredTokens {
  accessToken: string;
  refreshToken: string;
  /** ms since the epoch. */
  expiresAt: number;
  userId: string | null;
  adminId: string | null;
}

/** Refresh access tokens that expire within this window instead of using them. */
const EXPIRY_LEEWAY_MS = 10_000;

function defaultStorage(): AuthStorage {
  try {
    const ls = (globalThis as { localStorage?: AuthStorage }).localStorage;
    if (ls) {
      // Accessing it can throw (disabled storage, sandboxed iframes).
      ls.getItem("ferrix.auth:probe");
      return ls;
    }
  } catch {
    // fall through
  }
  return new MemoryAuthStorage();
}

function parseStored(raw: string | null): StoredTokens | null {
  if (!raw) return null;
  try {
    const v = JSON.parse(raw) as Partial<StoredTokens>;
    if (typeof v.accessToken !== "string" || typeof v.refreshToken !== "string" || typeof v.expiresAt !== "number") {
      return null;
    }
    return {
      accessToken: v.accessToken,
      refreshToken: v.refreshToken,
      expiresAt: v.expiresAt,
      userId: typeof v.userId === "string" ? v.userId : null,
      adminId: typeof v.adminId === "string" ? v.adminId : null,
    };
  } catch {
    return null;
  }
}

function toStored(t: AuthTokens): StoredTokens {
  return {
    accessToken: t.accessToken,
    refreshToken: t.refreshToken,
    expiresAt: Date.now() + t.expiresIn * 1000,
    userId: t.userId ?? null,
    adminId: t.adminId ?? null,
  };
}

const LOADING_STATE: AuthState = Object.freeze({ isLoading: true, isAuthenticated: false, userId: null, isAdmin: false });
const SIGNED_OUT_STATE: AuthState = Object.freeze({
  isLoading: false,
  isAuthenticated: false,
  userId: null,
  isAdmin: false,
});

/** An error from an auth endpoint (`RxFerrixError` with the server's `code`, e.g. `"invalid_credentials"`). */
function errorFrom(status: number, body: unknown): RxFerrixError {
  const b = (body ?? {}) as { errorMessage?: string; errorData?: ErrorData | null };
  if (typeof b.errorMessage === "string") return RxFerrixError.fromServer(b.errorMessage, b.errorData ?? null);
  return new RxFerrixError(`Auth request failed with HTTP ${status}`, { code: "http_error", data: { status } });
}

/** Auth bound to one {@link RxFerrixClient}. Create it with {@link getAuth}. */
export class RxFerrixAuth {
  readonly client: RxFerrixClient;
  private readonly storage: AuthStorage;
  private readonly storageKey: string;
  private readonly fetchImpl: typeof fetch;
  private readonly baseUrl: string;
  private tokens: StoredTokens | null = null;
  private current: AuthState = LOADING_STATE;
  private readonly listeners = new Set<() => void>();
  private readonly loaded: Promise<void>;
  private refreshing: Promise<string | null> | null = null;
  private removeStorageListener: () => void = () => {};

  constructor(client: RxFerrixClient, options: RxFerrixAuthOptions = {}) {
    this.client = client;
    this.storage = options.storage ?? defaultStorage();
    this.storageKey = options.storageKey ?? `ferrix.auth:${client.url}`;
    const f = options.fetch ?? (globalThis as { fetch?: typeof fetch }).fetch;
    if (!f) throw new Error("RxFerrixAuth: no global fetch available; pass the `fetch` option");
    // Called as a plain function: browsers throw "Illegal invocation" when `window.fetch` is called as a method of
    // another object (e.g. `this.fetchImpl(...)`).
    this.fetchImpl = (input, init) => f(input, init);
    this.baseUrl = client.url.replace(/\/+$/, "");
    this.loaded = this.load();
    // Holds the socket's subscriptions back until the stored session (if any) has been sent with Authenticate.
    this.client.setAuth(this.fetchToken, this.onServerAuthChange);
    if (options.syncTabs !== false) this.listenToOtherTabs();
  }

  // ---------------------------------------------------------------------------------------------------------------
  // State

  /** The current state; referentially stable until it changes. */
  state(): AuthState {
    return this.current;
  }

  /** Calls `listener` whenever {@link state} changes. Returns an unsubscribe function. */
  subscribe(listener: () => void): () => void {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  }

  private setState(next: AuthState): void {
    const prev = this.current;
    if (
      prev.isLoading === next.isLoading &&
      prev.isAuthenticated === next.isAuthenticated &&
      prev.userId === next.userId &&
      prev.isAdmin === next.isAdmin
    ) {
      return;
    }
    this.current = Object.freeze({ ...next });
    for (const l of [...this.listeners]) {
      try {
        l();
      } catch (e) {
        console.error("[rxferrix] auth listener threw", e);
      }
    }
  }

  private pendingState(t: StoredTokens): AuthState {
    return { isLoading: true, isAuthenticated: false, userId: t.userId as Id<"_users"> | null, isAdmin: t.adminId !== null };
  }

  /** Called by the client once the server accepted (true) or rejected / dropped (false) the token. */
  private onServerAuthChange = (isAuthenticated: boolean): void => {
    const t = this.tokens;
    if (isAuthenticated && t) {
      this.setState({ isLoading: false, isAuthenticated: true, userId: t.userId as Id<"_users"> | null, isAdmin: t.adminId !== null });
    } else if (!isAuthenticated) {
      // No token, or the server rejected it (and the forced refresh did not help), or the refresh failed.
      this.setState(SIGNED_OUT_STATE);
    }
  };

  // ---------------------------------------------------------------------------------------------------------------
  // Tokens

  private async load(): Promise<void> {
    try {
      this.tokens = parseStored(await this.storage.getItem(this.storageKey));
    } catch {
      this.tokens = null;
    }
    this.setState(this.tokens ? this.pendingState(this.tokens) : SIGNED_OUT_STATE);
  }

  private async readStorage(): Promise<StoredTokens | null> {
    try {
      return parseStored(await this.storage.getItem(this.storageKey));
    } catch {
      return null;
    }
  }

  private async saveTokens(t: StoredTokens | null): Promise<void> {
    this.tokens = t;
    try {
      if (t) await this.storage.setItem(this.storageKey, JSON.stringify(t));
      else await this.storage.removeItem(this.storageKey);
    } catch (e) {
      console.warn("[rxferrix] could not persist auth tokens", e);
    }
  }

  /** Store fresh tokens and authenticate the socket with them. */
  private async signedIn(tokens: AuthTokens): Promise<void> {
    await this.loaded;
    const t = toStored(tokens);
    await this.saveTokens(t);
    this.setState(this.pendingState(t));
    this.client.setAuth(this.fetchToken, this.onServerAuthChange);
  }

  /** The `fetchToken` given to `client.setAuth`. */
  private fetchToken = async ({ forceRefresh }: { forceRefresh: boolean }): Promise<string | null> => {
    await this.loaded;
    const t = this.tokens;
    if (!t) return null;
    if (!forceRefresh && t.expiresAt - Date.now() > EXPIRY_LEEWAY_MS) return t.accessToken;
    return this.refresh();
  };

  /**
   * A valid access token (refreshed when it is about to expire), or null when signed out. Useful for calling your
   * own HTTP endpoints or the HTTP function API with `Authorization: Bearer`.
   */
  async getAccessToken(options: { forceRefresh?: boolean } = {}): Promise<string | null> {
    return this.fetchToken({ forceRefresh: options.forceRefresh ?? false });
  }

  /** One refresh at a time per tab, and (with Web Locks) per origin. */
  private refresh(): Promise<string | null> {
    if (!this.refreshing) {
      this.refreshing = this.withLock(() => this.doRefresh()).finally(() => {
        this.refreshing = null;
      });
    }
    return this.refreshing;
  }

  private async withLock<T>(fn: () => Promise<T>): Promise<T> {
    const locks = (globalThis as { navigator?: { locks?: { request(name: string, cb: () => Promise<T>): Promise<T> } } })
      .navigator?.locks;
    if (locks && typeof locks.request === "function") return locks.request(`rxferrix-auth-refresh:${this.storageKey}`, fn);
    return fn();
  }

  /** Use tokens another tab stored, if they are newer than ours and still fresh. */
  private async adoptNewerStoredTokens(used: StoredTokens): Promise<string | null> {
    const stored = await this.readStorage();
    if (stored && stored.refreshToken !== used.refreshToken) {
      this.tokens = stored;
      if (stored.expiresAt - Date.now() > EXPIRY_LEEWAY_MS) return stored.accessToken;
    }
    return null;
  }

  private async doRefresh(): Promise<string | null> {
    const before = this.tokens;
    if (!before) return null;
    const adopted = await this.adoptNewerStoredTokens(before);
    if (adopted) return adopted;
    const t = this.tokens;
    if (!t) return null;
    let res: Response;
    try {
      res = await this.fetchImpl(`${this.baseUrl}/api/auth/refresh`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ refreshToken: t.refreshToken }),
      });
    } catch (e) {
      // Offline: keep the session; the next reconnect or AuthError tries again.
      throw new RxFerrixError("Could not reach the server to refresh the session", { code: "network_error", cause: e });
    }
    const body = (await res.json().catch(() => null)) as unknown;
    if (res.ok) {
      const next = toStored(body as AuthTokens);
      await this.saveTokens(next);
      return next.accessToken;
    }
    if (res.status === 401) {
      // Lost a race with another tab that rotated the token a moment ago: use its tokens.
      const raced = await this.adoptNewerStoredTokens(t);
      if (raced) return raced;
      await this.saveTokens(null);
      this.setState(SIGNED_OUT_STATE);
      return null;
    }
    throw errorFrom(res.status, body);
  }

  private listenToOtherTabs(): void {
    const g = globalThis as {
      addEventListener?: (type: string, l: (e: { key: string | null; newValue: string | null }) => void) => void;
      removeEventListener?: (type: string, l: (e: { key: string | null; newValue: string | null }) => void) => void;
    };
    if (typeof g.addEventListener !== "function") return;
    const handler = (e: { key: string | null; newValue: string | null }) => {
      if (e.key !== this.storageKey && e.key !== null) return;
      void this.loaded.then(() => this.onOtherTabChange(parseStored(e.key === null ? null : e.newValue)));
    };
    g.addEventListener("storage", handler);
    this.removeStorageListener = () => g.removeEventListener?.("storage", handler);
  }

  private onOtherTabChange(next: StoredTokens | null): void {
    const had = this.tokens;
    this.tokens = next;
    if (!next) {
      if (had) {
        this.client.clearAuth();
        this.setState(SIGNED_OUT_STATE);
      }
      return;
    }
    const sameSession = had !== null && had.userId === next.userId && had.adminId === next.adminId;
    if (!sameSession) {
      // Signed in (or as someone else) in another tab.
      this.setState(this.pendingState(next));
      this.client.setAuth(this.fetchToken, this.onServerAuthChange);
    }
    // Same session, rotated tokens: the next refresh picks them up.
  }

  /** Stop listening to other tabs. Does not sign out. */
  dispose(): void {
    this.removeStorageListener();
    this.listeners.clear();
  }

  // ---------------------------------------------------------------------------------------------------------------
  // Endpoints

  private async post<T>(path: string, body: unknown, bearer?: string): Promise<T> {
    const headers: Record<string, string> = { "Content-Type": "application/json" };
    if (bearer) headers.Authorization = `Bearer ${bearer}`;
    let res: Response;
    try {
      res = await this.fetchImpl(`${this.baseUrl}/api/auth/${path}`, { method: "POST", headers, body: JSON.stringify(body) });
    } catch (e) {
      throw new RxFerrixError("Could not reach the server", { code: "network_error", cause: e });
    }
    const json = (await res.json().catch(() => null)) as unknown;
    if (!res.ok) throw errorFrom(res.status, json);
    return json as T;
  }

  /** The sign-in methods enabled on the server. */
  async providers(): Promise<AuthProviders> {
    const res = await this.fetchImpl(`${this.baseUrl}/api/auth/providers`);
    const json = (await res.json().catch(() => null)) as unknown;
    if (!res.ok) throw errorFrom(res.status, json);
    return json as AuthProviders;
  }

  /**
   * Create an account with email and password and sign in. Resolves `{ verificationRequired: true }` (without
   * signing in) when the server requires a verified email first.
   */
  async signUp(args: { email: string; password: string; name?: string }): Promise<{ verificationRequired: boolean }> {
    const res = await this.post<AuthTokens | { verificationRequired: true }>("signup", args);
    if ("accessToken" in res) {
      await this.signedIn(res);
      return { verificationRequired: false };
    }
    return { verificationRequired: true };
  }

  /** Sign in with email and password. Rejects with code `"invalid_credentials"` on a wrong email or password. */
  async signIn(args: { email: string; password: string }): Promise<void> {
    await this.signedIn(await this.post<AuthTokens>("signin", args));
  }

  /** Sign in to an admin account (dashboard). Functions then see `ctx.auth.is_admin()`. */
  async signInAsAdmin(args: { email: string; password: string }): Promise<void> {
    await this.signedIn(await this.post<AuthTokens>("admin/signin", args));
  }

  /** Whether the server has no administrator yet (`GET /api/auth/admin/setup`). */
  async needsAdminSetup(): Promise<boolean> {
    const res = await this.fetchImpl(`${this.baseUrl}/api/auth/admin/setup`);
    const json = (await res.json().catch(() => null)) as { needsSetup?: boolean } | null;
    if (!res.ok) throw errorFrom(res.status, json);
    return json?.needsSetup === true;
  }

  /**
   * Create the first administrator with the one-time setup token that `serve` prints as `/_/setup#token=…`, and
   * sign in as them. Rejects with code `"invalid_setup_token"` or `"setup_done"`.
   */
  async setupAdmin(args: { token: string; email: string; password: string }): Promise<void> {
    await this.signedIn(await this.post<AuthTokens>("admin/setup", args));
  }

  /** End the session on the server and forget the tokens (also in other tabs). */
  async signOut(): Promise<void> {
    await this.loaded;
    const t = this.tokens;
    await this.saveTokens(null);
    this.client.clearAuth();
    this.setState(SIGNED_OUT_STATE);
    if (t) {
      try {
        await this.post("signout", { refreshToken: t.refreshToken });
      } catch {
        // The local sign-out already happened; the session expires on its own.
      }
    }
  }

  /** Email a verification link and code (no-op for unknown or already verified addresses). */
  async requestEmailVerification(args: { email: string; redirectTo?: string }): Promise<void> {
    await this.post("verify-email/request", args);
  }

  /** Confirm an email address with the link token or the code. */
  async verifyEmail(proof: CodeProof): Promise<void> {
    await this.post("verify-email/confirm", proof);
  }

  /** Email a password reset link and code. Succeeds whether or not the account exists. */
  async requestPasswordReset(args: { email: string; redirectTo?: string }): Promise<void> {
    await this.post("reset/request", args);
  }

  /** Set a new password with the reset link token or code; ends other sessions and signs in. */
  async resetPassword(proof: CodeProof, password: string): Promise<void> {
    await this.signedIn(await this.post<AuthTokens>("reset/confirm", { ...proof, password }));
  }

  /** Email a magic sign-in link and a one-time code. Succeeds whether or not the account exists. */
  async requestMagicLink(args: { email: string; redirectTo?: string }): Promise<void> {
    await this.post("magic/request", args);
  }

  /** Sign in with the one-time code from a magic link email. */
  async verifyCode(args: { email: string; code: string }): Promise<void> {
    await this.signedIn(await this.post<AuthTokens>("magic/verify", args));
  }

  /** Sign in with the token of a magic link (usually done by {@link handleRedirectCallback}). */
  async verifyMagicLink(token: string): Promise<void> {
    await this.signedIn(await this.post<AuthTokens>("magic/verify", { token }));
  }

  /** The URL that starts an OAuth sign-in with `provider`, returning to `redirectTo` (default: this page). */
  oauthUrl(provider: string, options: { redirectTo?: string } = {}): string {
    const loc = (globalThis as { location?: { href: string } }).location;
    const redirectTo = options.redirectTo ?? (loc ? loc.href.split("#")[0] : undefined);
    const query = redirectTo ? `?redirect_to=${encodeURIComponent(redirectTo)}` : "";
    return `${this.baseUrl}/api/auth/oauth/${encodeURIComponent(provider)}${query}`;
  }

  /**
   * Start an OAuth sign-in: a full-page redirect to the provider. After approval the server redirects back to
   * `redirectTo` (which must be allowed by the server's `--auth-redirect-url` list) with the tokens in the URL
   * fragment; call {@link handleRedirectCallback} on that page.
   */
  signInWithOAuth(provider: string, options: { redirectTo?: string } = {}): void {
    const loc = (globalThis as { location?: { assign(url: string): void } }).location;
    if (!loc) throw new Error("signInWithOAuth needs a browser (window.location)");
    loc.assign(this.oauthUrl(provider, options));
  }

  /**
   * Handle the `#rxferrix_…` fragment that OAuth redirects and email links put on the URL: stores tokens (OAuth),
   * signs in (magic link), confirms the email (verification link) or returns the reset token (password reset). The
   * `rxferrix_` parameters are removed from the address bar first, so a reload can't replay them. Returns null when
   * the URL has none.
   */
  async handleRedirectCallback(url?: string): Promise<RedirectResult | null> {
    const g = globalThis as {
      location?: { href: string };
      history?: { state: unknown; replaceState(state: unknown, title: string, url: string): void };
    };
    const href = url ?? g.location?.href;
    if (!href) return null;
    const parsed = new URL(href);
    const params = new URLSearchParams(parsed.hash.replace(/^#/, ""));
    const ours = new Map<string, string>();
    for (const [k, v] of params) if (k.startsWith("rxferrix_")) ours.set(k, v);
    if (ours.size === 0) return null;
    for (const k of ours.keys()) params.delete(k);
    const rest = params.toString();
    parsed.hash = rest ? `#${rest}` : "";
    if (g.history && g.location && (url === undefined || url === g.location.href)) {
      g.history.replaceState(g.history.state, "", parsed.toString());
    }

    try {
      const error = ours.get("rxferrix_error");
      if (error) {
        const message = ours.get("rxferrix_error_description") ?? "Sign-in failed";
        return { type: "error", error: new RxFerrixError(message, { code: error }) };
      }
      const access = ours.get("rxferrix_access_token");
      const refresh = ours.get("rxferrix_refresh_token");
      if (access && refresh) {
        const expiresIn = Number(ours.get("rxferrix_expires_in") ?? "900");
        const userId = ours.get("rxferrix_user_id");
        await this.signedIn({ accessToken: access, refreshToken: refresh, expiresIn, ...(userId ? { userId } : {}) });
        return { type: "signedIn" };
      }
      const magic = ours.get("rxferrix_magic_token");
      if (magic) {
        await this.verifyMagicLink(magic);
        return { type: "signedIn" };
      }
      const verify = ours.get("rxferrix_verify_token");
      if (verify) {
        await this.verifyEmail({ token: verify });
        return { type: "emailVerified" };
      }
      const reset = ours.get("rxferrix_reset_token");
      if (reset) return { type: "passwordReset", token: reset };
    } catch (e) {
      return { type: "error", error: e instanceof RxFerrixError ? e : new RxFerrixError(String(e), { cause: e }) };
    }
    return null;
  }
}

const instances = new WeakMap<RxFerrixClient, RxFerrixAuth>();

/**
 * The {@link RxFerrixAuth} of `client`, created on first use (with `options`). Later calls return the same instance
 * (and ignore `options`), so hooks like `useRxFerrixAuth()` share it. Call it once early (e.g. next to
 * `new RxFerrixClient`) to pass options such as a custom `storage`.
 */
export function getAuth(client: RxFerrixClient, options?: RxFerrixAuthOptions): RxFerrixAuth {
  let auth = instances.get(client);
  if (!auth) {
    auth = new RxFerrixAuth(client, options);
    instances.set(client, auth);
  }
  return auth;
}

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { RxFerrixAuth, RxFerrixClient, RxFerrixError, MemoryAuthStorage, getAuth, systemApi } from "../src/index.js";
import { FakeServer } from "../src/testing.js";

const URL_BASE = "http://localhost:3210";
const KEY = `rxferrix.auth:${URL_BASE}`;

/** A JWT-shaped token whose payload carries `exp` (the client schedules refreshes from it). */
function jwt(name: string, expSeconds: number): string {
  const b64 = (o: unknown) => btoa(JSON.stringify(o)).replace(/=+$/, "").replace(/\+/g, "-").replace(/\//g, "_");
  return `${b64({ alg: "HS256", typ: "JWT" })}.${b64({ sub: name, exp: expSeconds })}.sig`;
}

function tokens(name: string, ttlSeconds = 900) {
  return {
    accessToken: jwt(name, Math.floor(Date.now() / 1000) + ttlSeconds),
    refreshToken: `refresh-${name}`,
    expiresIn: ttlSeconds,
    userId: "user1",
  };
}

function stored(name: string, expiresAt: number) {
  return JSON.stringify({ accessToken: jwt(name, Math.floor(expiresAt / 1000)), refreshToken: `refresh-${name}`, expiresAt, userId: "user1" });
}

type Route = (body: Record<string, unknown>) => { status: number; body: unknown };

/** A fetch mock answering `/api/auth/*` from `routes` and recording every call. */
function mockFetch(routes: Record<string, Route>) {
  const calls: { path: string; body: Record<string, unknown> | undefined }[] = [];
  const fetch = vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
    const path = new URL(String(input)).pathname;
    const body = init?.body ? (JSON.parse(String(init.body)) as Record<string, unknown>) : undefined;
    calls.push({ path, body });
    const route = routes[path];
    const r = route ? route(body ?? {}) : { status: 404, body: { status: "error", errorMessage: "not found" } };
    return new Response(JSON.stringify(r.body), { status: r.status, headers: { "Content-Type": "application/json" } });
  });
  return { fetch: fetch as unknown as typeof globalThis.fetch, calls };
}

function authError(status: number, code: string, message = code) {
  return { status, body: { status: "error", errorMessage: message, errorData: { code, data: null } } };
}

/** `setImmediate` is not faked, so waiting on it lets real async work (Response bodies) progress. */
const setImmediateReal = (globalThis as unknown as { setImmediate(cb: () => void): void }).setImmediate;

/** Lets promise chains that include real async work (Response bodies) finish. */
async function settle(): Promise<void> {
  for (let i = 0; i < 10; i++) await new Promise<void>((r) => setImmediateReal(r));
}

let server: FakeServer;
let client: RxFerrixClient;
let storage: MemoryAuthStorage;

beforeEach(() => {
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "setInterval", "clearInterval"] });
  server = new FakeServer();
  client = new RxFerrixClient(URL_BASE, { webSocketConstructor: server.WebSocket, unsavedChangesWarning: false });
  storage = new MemoryAuthStorage();
});

afterEach(() => {
  client.close();
  vi.useRealTimers();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

async function openAndDrain() {
  server.open();
  await settle();
  return server.drain();
}

describe("RxFerrixAuth", () => {
  it("restores a stored session and authenticates the socket before subscribing", async () => {
    storage.setItem(KEY, stored("a", Date.now() + 600_000));
    const { fetch, calls } = mockFetch({});
    const auth = new RxFerrixAuth(client, { storage, fetch, syncTabs: false });
    expect(auth.state()).toMatchObject({ isLoading: true, isAuthenticated: false });
    client.watchQuery(systemApi.auth.me).onUpdate(() => {});
    const sent = await openAndDrain();
    expect(sent.map((m) => m.type)).toEqual(["Connect", "Authenticate", "ModifyQuerySet"]);
    expect(sent[1]).toMatchObject({ token: JSON.parse(stored("a", Date.now() + 600_000)).accessToken });
    expect(sent[2]).toMatchObject({ modifications: [{ udfPath: "_system/auth:me" }] });
    expect(calls).toEqual([]);
    server.transition({ identity: 1, querySet: 1, ts: 1 });
    expect(auth.state()).toEqual({ isLoading: false, isAuthenticated: true, userId: "user1", isAdmin: false });
  });

  it("signs in, stores the tokens and sends Authenticate", async () => {
    const t = tokens("b");
    const { fetch, calls } = mockFetch({ "/api/auth/signin": () => ({ status: 200, body: t }) });
    const auth = new RxFerrixAuth(client, { storage, fetch, syncTabs: false });
    const listener = vi.fn();
    auth.subscribe(listener);
    await openAndDrain();
    expect(auth.state()).toMatchObject({ isLoading: false, isAuthenticated: false, userId: null });

    await auth.signIn({ email: "a@b.co", password: "pw" });
    await settle();
    expect(calls).toEqual([{ path: "/api/auth/signin", body: { email: "a@b.co", password: "pw" } }]);
    expect(JSON.parse(storage.getItem(KEY)!)).toMatchObject({ accessToken: t.accessToken, refreshToken: t.refreshToken });
    expect(server.drain()).toEqual([{ type: "Authenticate", baseVersion: 0, token: t.accessToken }]);
    expect(auth.state()).toMatchObject({ isLoading: true, userId: "user1" });
    server.transition({ identity: 1, ts: 1 });
    expect(auth.state()).toMatchObject({ isLoading: false, isAuthenticated: true });
    expect(listener).toHaveBeenCalled();
  });

  it("calls fetch as a plain function (browsers reject a fetch called as a method of another object)", async () => {
    const t = tokens("c");
    const seen: unknown[] = [];
    const fetch = function (this: unknown) {
      seen.push(this);
      return Promise.resolve(new Response(JSON.stringify(t), { status: 200 }));
    } as unknown as typeof globalThis.fetch;
    const auth = new RxFerrixAuth(client, { storage, fetch, syncTabs: false });
    await openAndDrain();
    await auth.signIn({ email: "a@b.co", password: "pw" });
    expect(seen).toEqual([undefined]);
  });

  it("creates the first admin with the setup token and signs in as them", async () => {
    const t = { ...tokens("admin"), userId: undefined, adminId: "admin1" };
    const { fetch, calls } = mockFetch({
      "/api/auth/admin/setup": (b) => (b.token === "good" ? { status: 200, body: t } : authError(401, "invalid_setup_token")),
    });
    const auth = new RxFerrixAuth(client, { storage, fetch, syncTabs: false });
    await openAndDrain();
    const err = await auth.setupAdmin({ token: "bad", email: "a@b.co", password: "pw" }).catch((e: unknown) => e);
    expect(err).toMatchObject({ code: "invalid_setup_token" });
    await auth.setupAdmin({ token: "good", email: "a@b.co", password: "pw" });
    await settle();
    expect(calls.at(-1)).toEqual({ path: "/api/auth/admin/setup", body: { token: "good", email: "a@b.co", password: "pw" } });
    expect(server.drain()).toEqual([{ type: "Authenticate", baseVersion: 0, token: t.accessToken }]);
    server.transition({ identity: 1, ts: 1 });
    expect(auth.state()).toEqual({ isLoading: false, isAuthenticated: true, userId: null, isAdmin: true });
  });

  it("rejects with the server's error code", async () => {
    const { fetch } = mockFetch({ "/api/auth/signin": () => authError(401, "invalid_credentials", "Invalid email or password") });
    const auth = new RxFerrixAuth(client, { storage, fetch, syncTabs: false });
    await openAndDrain();
    const err = await auth.signIn({ email: "a@b.co", password: "nope" }).catch((e: unknown) => e);
    expect(err).toBeInstanceOf(RxFerrixError);
    expect(err).toMatchObject({ code: "invalid_credentials", message: "Invalid email or password" });
    expect(storage.getItem(KEY)).toBeNull();
    expect(auth.state().isAuthenticated).toBe(false);
  });

  it("refreshes an expired stored token first and persists the rotated tokens", async () => {
    storage.setItem(KEY, stored("old", Date.now() - 1000));
    const fresh = tokens("new");
    const { fetch, calls } = mockFetch({ "/api/auth/refresh": () => ({ status: 200, body: fresh }) });
    new RxFerrixAuth(client, { storage, fetch, syncTabs: false });
    const sent = await openAndDrain();
    expect(calls).toEqual([{ path: "/api/auth/refresh", body: { refreshToken: "refresh-old" } }]);
    expect(sent[1]).toEqual({ type: "Authenticate", baseVersion: 0, token: fresh.accessToken });
    expect(JSON.parse(storage.getItem(KEY)!).refreshToken).toBe("refresh-new");
  });

  it("answers an AuthError with a forced refresh and re-authenticates on the next socket", async () => {
    storage.setItem(KEY, stored("a", Date.now() + 600_000));
    const fresh = tokens("b");
    const { fetch, calls } = mockFetch({ "/api/auth/refresh": () => ({ status: 200, body: fresh }) });
    const auth = new RxFerrixAuth(client, { storage, fetch, syncTabs: false });
    await openAndDrain();
    server.push({ type: "AuthError", error: "access token expired" });
    await settle();
    expect(calls.map((c) => c.path)).toEqual(["/api/auth/refresh"]);
    vi.advanceTimersByTime(20_000);
    const sent = await openAndDrain();
    expect(sent[1]).toEqual({ type: "Authenticate", baseVersion: 0, token: fresh.accessToken });
    server.transition({ identity: 1, ts: 1 });
    expect(auth.state().isAuthenticated).toBe(true);
  });

  it("signs out locally when the refresh token is rejected", async () => {
    storage.setItem(KEY, stored("a", Date.now() - 1000));
    const { fetch } = mockFetch({ "/api/auth/refresh": () => authError(401, "refresh_token_reused") });
    const auth = new RxFerrixAuth(client, { storage, fetch, syncTabs: false });
    const sent = await openAndDrain();
    expect(sent.map((m) => m.type)).toEqual(["Connect"]);
    expect(storage.getItem(KEY)).toBeNull();
    expect(auth.state()).toEqual({ isLoading: false, isAuthenticated: false, userId: null, isAdmin: false });
  });

  it("drops to signed out when the server revokes the session of a live socket", async () => {
    storage.setItem(KEY, stored("a", Date.now() + 600_000));
    const { fetch, calls } = mockFetch({ "/api/auth/refresh": () => authError(401, "invalid_refresh_token") });
    const auth = new RxFerrixAuth(client, { storage, fetch, syncTabs: false });
    await openAndDrain();
    server.transition({ identity: 1, ts: 1 });
    expect(auth.state().isAuthenticated).toBe(true);
    // Signed out elsewhere, or revoked from the dashboard: the server ends the socket right away.
    server.push({ type: "AuthError", error: "session has ended" });
    await settle();
    expect(calls.map((c) => c.path)).toEqual(["/api/auth/refresh"]);
    expect(auth.state()).toEqual({ isLoading: false, isAuthenticated: false, userId: null, isAdmin: false });
    expect(storage.getItem(KEY)).toBeNull();
    vi.advanceTimersByTime(20_000);
    const sent = await openAndDrain();
    expect(sent.map((m) => m.type)).toEqual(["Connect"]);
  });

  it("adopts tokens another tab rotated when its own refresh is stale", async () => {
    storage.setItem(KEY, stored("a", Date.now() - 1000));
    const other = stored("other-tab", Date.now() + 600_000);
    const { fetch } = mockFetch({
      "/api/auth/refresh": () => {
        storage.setItem(KEY, other); // the other tab won the race
        return authError(401, "stale_refresh_token");
      },
    });
    new RxFerrixAuth(client, { storage, fetch, syncTabs: false });
    const sent = await openAndDrain();
    expect(sent[1]).toEqual({ type: "Authenticate", baseVersion: 0, token: JSON.parse(other).accessToken });
  });

  it("signs out: forgets the tokens, clears the socket identity and ends the session", async () => {
    storage.setItem(KEY, stored("a", Date.now() + 600_000));
    const { fetch, calls } = mockFetch({ "/api/auth/signout": () => ({ status: 200, body: {} }) });
    const auth = new RxFerrixAuth(client, { storage, fetch, syncTabs: false });
    await openAndDrain();
    server.transition({ identity: 1, ts: 1 });
    await auth.signOut();
    expect(storage.getItem(KEY)).toBeNull();
    expect(server.drain()).toEqual([{ type: "Authenticate", baseVersion: 1, token: null }]);
    expect(calls).toEqual([{ path: "/api/auth/signout", body: { refreshToken: "refresh-a" } }]);
    expect(auth.state()).toMatchObject({ isLoading: false, isAuthenticated: false });
  });

  it("calls the email endpoints", async () => {
    const t = tokens("c");
    const ok = () => ({ status: 200, body: {} });
    const { fetch, calls } = mockFetch({
      "/api/auth/signup": () => ({ status: 200, body: { verificationRequired: true } }),
      "/api/auth/reset/request": ok,
      "/api/auth/reset/confirm": () => ({ status: 200, body: t }),
      "/api/auth/magic/request": ok,
      "/api/auth/magic/verify": () => ({ status: 200, body: t }),
      "/api/auth/verify-email/confirm": ok,
      "/api/auth/password/change": ok,
    });
    const auth = new RxFerrixAuth(client, { storage, fetch, syncTabs: false });
    await openAndDrain();
    expect(await auth.signUp({ email: "a@b.co", password: "long enough" })).toEqual({ verificationRequired: true });
    expect(storage.getItem(KEY)).toBeNull();
    await auth.requestPasswordReset({ email: "a@b.co", redirectTo: "http://app.test/reset" });
    await auth.resetPassword({ email: "a@b.co", code: "123456" }, "new password");
    await auth.requestMagicLink({ email: "a@b.co" });
    await auth.verifyCode({ email: "a@b.co", code: "654321" });
    await auth.verifyEmail({ token: "vt" });
    await auth.changePassword("new password", "newer password");
    const changeInit = (fetch as unknown as { mock: { calls: [unknown, RequestInit][] } }).mock.calls.at(-1)![1];
    expect((changeInit.headers as Record<string, string>).Authorization).toBe(`Bearer ${t.accessToken}`);
    expect(calls.map((c) => [c.path, c.body])).toEqual([
      ["/api/auth/signup", { email: "a@b.co", password: "long enough" }],
      ["/api/auth/reset/request", { email: "a@b.co", redirectTo: "http://app.test/reset" }],
      ["/api/auth/reset/confirm", { email: "a@b.co", code: "123456", password: "new password" }],
      ["/api/auth/magic/request", { email: "a@b.co" }],
      ["/api/auth/magic/verify", { email: "a@b.co", code: "654321" }],
      ["/api/auth/verify-email/confirm", { token: "vt" }],
      ["/api/auth/password/change", { currentPassword: "new password", newPassword: "newer password" }],
    ]);
    expect(JSON.parse(storage.getItem(KEY)!).accessToken).toBe(t.accessToken);
  });

  it("stops at the second factor and signs in with verifyMfa", async () => {
    const t = tokens("m");
    const { fetch, calls } = mockFetch({
      "/api/auth/signin": () => ({ status: 200, body: { mfaRequired: true, mfaToken: "mfa-1", methods: ["totp"] } }),
      "/api/auth/mfa/verify": (b) => (b.code === "123456" ? { status: 200, body: t } : authError(400, "invalid_mfa_code")),
    });
    const auth = new RxFerrixAuth(client, { storage, fetch, syncTabs: false });
    await openAndDrain();

    const r = await auth.signIn({ email: "a@b.co", password: "pw" });
    expect(r).toEqual({ mfaRequired: true, mfaToken: "mfa-1", methods: ["totp"] });
    expect(auth.state()).toMatchObject({ isLoading: false, isAuthenticated: false });
    expect(storage.getItem(KEY)).toBeNull();
    expect(server.drain()).toEqual([]);

    const wrong = await auth.verifyMfa("mfa-1", "000000").catch((e: unknown) => e);
    expect(wrong).toMatchObject({ code: "invalid_mfa_code" });
    await auth.verifyMfa("mfa-1", "123456");
    await settle();
    expect(calls.at(-1)).toEqual({ path: "/api/auth/mfa/verify", body: { mfaToken: "mfa-1", code: "123456" } });
    expect(JSON.parse(storage.getItem(KEY)!)).toMatchObject({ accessToken: t.accessToken });
    expect(server.drain()).toEqual([{ type: "Authenticate", baseVersion: 0, token: t.accessToken }]);
  });

  it("manages the signed-in account's second factor with the bearer token", async () => {
    const t = tokens("f");
    storage.setItem(KEY, JSON.stringify({ ...t, expiresAt: Date.now() + 600_000, adminId: null }));
    const seenAuth: (string | null)[] = [];
    const fetch = vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
      const path = new URL(String(input)).pathname;
      seenAuth.push(new Headers(init?.headers).get("Authorization"));
      const body = init?.body ? (JSON.parse(String(init.body)) as Record<string, unknown>) : {};
      const answer = (v: unknown) => new Response(JSON.stringify(v), { status: 200, headers: { "Content-Type": "application/json" } });
      if (path === "/api/auth/mfa") return answer({ enabled: false, pending: false, recoveryCodesLeft: 0, methods: [] });
      if (path === "/api/auth/mfa/totp/setup") {
        expect(body).toEqual({ password: "pw" });
        return answer({ secret: "ABC", uri: "otpauth://totp/x", qrSvg: "<svg/>" });
      }
      if (path === "/api/auth/mfa/totp/confirm") return answer({ recoveryCodes: ["aaaaa-bbbbb"] });
      if (path === "/api/auth/mfa/recovery-codes") return answer({ recoveryCodes: ["ccccc-ddddd"] });
      if (path === "/api/auth/mfa/disable") {
        expect(body).toEqual({ code: "654321" });
        return answer({});
      }
      return new Response("{}", { status: 404 });
    }) as unknown as typeof globalThis.fetch;
    const auth = new RxFerrixAuth(client, { storage, fetch, syncTabs: false });
    await openAndDrain();

    expect(await auth.getMfaStatus()).toEqual({ enabled: false, pending: false, recoveryCodesLeft: 0, methods: [] });
    expect(await auth.setupTotp({ password: "pw" })).toEqual({ secret: "ABC", uri: "otpauth://totp/x", qrSvg: "<svg/>" });
    expect(await auth.confirmTotp("123456")).toEqual({ recoveryCodes: ["aaaaa-bbbbb"] });
    expect(await auth.regenerateRecoveryCodes("aaaaa-bbbbb")).toEqual({ recoveryCodes: ["ccccc-ddddd"] });
    await auth.disableMfa("654321");
    expect(seenAuth).toEqual(Array(5).fill(`Bearer ${t.accessToken}`));

    await auth.signOut();
    const err = await auth.getMfaStatus().catch((e: unknown) => e);
    expect(err).toMatchObject({ code: "unauthenticated" });
  });

  it("handles OAuth and email-link redirects and cleans the URL", async () => {
    const t = tokens("o");
    const { fetch, calls } = mockFetch({ "/api/auth/magic/verify": () => ({ status: 200, body: t }) });
    const replaceState = vi.fn();
    const location = { href: "", assign: vi.fn() };
    vi.stubGlobal("location", location);
    vi.stubGlobal("history", { state: null, replaceState });
    const auth = new RxFerrixAuth(client, { storage, fetch, syncTabs: false });
    await openAndDrain();

    location.href = `http://app.test/cb?x=1#tab=2&rxferrix_access_token=${t.accessToken}&rxferrix_refresh_token=r1&rxferrix_expires_in=900&rxferrix_user_id=u9`;
    expect(await auth.handleRedirectCallback()).toEqual({ type: "signedIn" });
    expect(replaceState).toHaveBeenLastCalledWith(null, "", "http://app.test/cb?x=1#tab=2");
    expect(JSON.parse(storage.getItem(KEY)!)).toMatchObject({ accessToken: t.accessToken, refreshToken: "r1", userId: "u9" });
    await settle();
    expect(server.drain()).toEqual([{ type: "Authenticate", baseVersion: 0, token: t.accessToken }]);

    location.href = "http://app.test/cb#rxferrix_error=access_denied&rxferrix_error_description=No%20thanks";
    const failed = await auth.handleRedirectCallback();
    expect(failed).toMatchObject({ type: "error", error: { code: "access_denied", message: "No thanks" } });
    expect(replaceState).toHaveBeenLastCalledWith(null, "", "http://app.test/cb");

    location.href = "http://app.test/reset#rxferrix_reset_token=rt";
    expect(await auth.handleRedirectCallback()).toEqual({ type: "passwordReset", token: "rt" });

    location.href = "http://app.test/#rxferrix_magic_token=mt";
    expect(await auth.handleRedirectCallback()).toEqual({ type: "signedIn" });
    expect(calls).toEqual([{ path: "/api/auth/magic/verify", body: { token: "mt" } }]);

    // An account with two-factor authentication: OAuth returns the second-factor token in the fragment.
    location.href = "http://app.test/cb#rxferrix_mfa_token=mfa-9";
    expect(await auth.handleRedirectCallback()).toEqual({ type: "mfaRequired", mfaToken: "mfa-9" });
    expect(replaceState).toHaveBeenLastCalledWith(null, "", "http://app.test/cb");

    location.href = "http://app.test/#nothing=here";
    expect(await auth.handleRedirectCallback()).toBeNull();

    expect(auth.oauthUrl("github", { redirectTo: "http://app.test/cb?a=b" })).toBe(
      `${URL_BASE}/api/auth/oauth/github?redirect_to=${encodeURIComponent("http://app.test/cb?a=b")}`,
    );
    location.href = "http://app.test/page#frag";
    auth.signInWithOAuth("google");
    expect(location.assign).toHaveBeenCalledWith(`${URL_BASE}/api/auth/oauth/google?redirect_to=${encodeURIComponent("http://app.test/page")}`);
  });

  it("follows sign-in and sign-out in other tabs", async () => {
    let handler: ((e: { key: string | null; newValue: string | null }) => void) | undefined;
    vi.stubGlobal("addEventListener", (type: string, l: typeof handler) => {
      if (type === "storage") handler = l;
    });
    vi.stubGlobal("removeEventListener", () => {});
    const { fetch } = mockFetch({});
    const auth = new RxFerrixAuth(client, { storage, fetch });
    await openAndDrain();
    expect(handler).toBeDefined();

    const other = stored("tab2", Date.now() + 600_000);
    handler!({ key: KEY, newValue: other });
    storage.setItem(KEY, other);
    await settle();
    expect(server.drain()).toEqual([{ type: "Authenticate", baseVersion: 0, token: JSON.parse(other).accessToken }]);
    server.transition({ identity: 1, ts: 1 });
    expect(auth.state().isAuthenticated).toBe(true);

    handler!({ key: KEY, newValue: null });
    await settle();
    expect(server.drain()).toEqual([{ type: "Authenticate", baseVersion: 1, token: null }]);
    expect(auth.state().isAuthenticated).toBe(false);
    handler!({ key: "unrelated", newValue: "x" });
    auth.dispose();
  });

  it("getAuth returns one instance per client, and systemApi points at the built-in functions", () => {
    const { fetch } = mockFetch({});
    const a = getAuth(client, { storage, fetch, syncTabs: false });
    expect(getAuth(client)).toBe(a);
    expect(systemApi.auth.me._path).toBe("_system/auth:me");
    expect(systemApi.auth.updateProfile._path).toBe("_system/auth:update_profile");
  });
});

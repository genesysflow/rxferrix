import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { RxFerrixClient, MemoryAuthStorage, getAuth } from "@rxferrix/client";
import { FakeServer, flushMicrotasks } from "@rxferrix/client/testing";
import { useEffect, useState } from "react";
import {
  RxFerrixProvider,
  useAuthActions,
  useCurrentUser,
  useRxFerrixAuth,
  type AuthProviders,
  type AuthTokens,
  type CodeProof,
} from "../src/index.js";

let server: FakeServer;
let client: RxFerrixClient;

const tokens = { accessToken: "a.b.c", refreshToken: "r1", expiresIn: 900, userId: "u1" };

beforeEach(() => {
  server = new FakeServer();
  client = new RxFerrixClient("http://localhost:3210", { webSocketConstructor: server.WebSocket, unsavedChangesWarning: false });
  const fetch = vi.fn(async () => new Response(JSON.stringify(tokens), { status: 200 }));
  getAuth(client, { storage: new MemoryAuthStorage(), fetch: fetch as unknown as typeof globalThis.fetch, syncTabs: false });
});

afterEach(() => {
  cleanup();
  client.close();
});

async function settle() {
  await act(async () => {
    for (let i = 0; i < 5; i++) await new Promise((r) => setTimeout(r, 0));
    await flushMicrotasks();
  });
}

function Status() {
  const { isLoading, isAuthenticated } = useRxFerrixAuth();
  const user = useCurrentUser();
  const { signIn, signOut } = useAuthActions();
  return (
    <div>
      <p data-testid="status">{isLoading ? "loading" : isAuthenticated ? "in" : "out"}</p>
      <p data-testid="user">{user === undefined ? "?" : user === null ? "none" : user.email}</p>
      <button onClick={() => void signIn({ email: "a@b.co", password: "pw" })}>in</button>
      <button onClick={() => void signOut()}>out</button>
    </div>
  );
}

describe("auth hooks", () => {
  it("track sign-in, the current user and sign-out", async () => {
    render(
      <RxFerrixProvider client={client}>
        <Status />
      </RxFerrixProvider>,
    );
    server.open();
    await settle();
    server.drain();
    expect(screen.getByTestId("status").textContent).toBe("out");
    expect(screen.getByTestId("user").textContent).toBe("none");

    fireEvent.click(screen.getByText("in"));
    await settle();
    expect(screen.getByTestId("status").textContent).toBe("loading");
    expect(server.drain()).toEqual([{ type: "Authenticate", baseVersion: 0, token: "a.b.c" }]);
    act(() => server.transition({ identity: 1, ts: 1 }));
    await settle();
    expect(screen.getByTestId("status").textContent).toBe("in");
    // useCurrentUser subscribes to _system/auth:me once authenticated.
    const add = server.drain().find((m) => m.type === "ModifyQuerySet");
    expect(add).toMatchObject({ modifications: [{ type: "Add", udfPath: "_system/auth:me" }] });
    act(() =>
      server.transition({ querySet: 1, ts: 2 }, [
        { type: "QueryUpdated", queryId: 0, value: { _id: "u1", email: "a@b.co", name: null } },
      ]),
    );
    expect(screen.getByTestId("user").textContent).toBe("a@b.co");

    fireEvent.click(screen.getByText("out"));
    await settle();
    expect(screen.getByTestId("status").textContent).toBe("out");
    expect(screen.getByTestId("user").textContent).toBe("none");
  });

  it("list the server's sign-in methods", async () => {
    const providers: AuthProviders = { password: true, magicLink: true, signups: false, oauth: ["github"] };
    const other = new RxFerrixClient("http://localhost:3211", {
      webSocketConstructor: server.WebSocket,
      unsavedChangesWarning: false,
    });
    const fetch = vi.fn(async (url: string) =>
      url.endsWith("/api/auth/providers")
        ? new Response(JSON.stringify(providers), { status: 200 })
        : new Response("not found", { status: 404 }),
    );
    getAuth(other, { storage: new MemoryAuthStorage(), fetch: fetch as unknown as typeof globalThis.fetch, syncTabs: false });
    function Methods() {
      const { providers: list } = useAuthActions();
      const [methods, setMethods] = useState<AuthProviders | null>(null);
      useEffect(() => {
        void list().then(setMethods);
      }, [list]);
      return <p data-testid="methods">{methods ? methods.oauth.join(",") + (methods.magicLink ? " +link" : "") : "…"}</p>;
    }
    render(
      <RxFerrixProvider client={other}>
        <Methods />
      </RxFerrixProvider>,
    );
    await settle();
    expect(screen.getByTestId("methods").textContent).toBe("github +link");
    expect(fetch.mock.calls.map((c) => String(c[0]))).toContain("http://localhost:3211/api/auth/providers");
    other.close();
    // The auth types are re-exported from @rxferrix/react.
    const proof: CodeProof = { email: "a@b.co", code: "123456" };
    const t: AuthTokens = tokens;
    void proof;
    void t;
  });
});

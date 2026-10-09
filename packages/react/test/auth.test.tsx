import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { FerrixClient, MemoryAuthStorage, getAuth } from "@ferrix/client";
import { FakeServer, flushMicrotasks } from "@ferrix/client/testing";
import { FerrixProvider, useAuthActions, useCurrentUser, useFerrixAuth } from "../src/index.js";

let server: FakeServer;
let client: FerrixClient;

const tokens = { accessToken: "a.b.c", refreshToken: "r1", expiresIn: 900, userId: "u1" };

beforeEach(() => {
  server = new FakeServer();
  client = new FerrixClient("http://localhost:3210", { webSocketConstructor: server.WebSocket, unsavedChangesWarning: false });
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
  const { isLoading, isAuthenticated } = useFerrixAuth();
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
      <FerrixProvider client={client}>
        <Status />
      </FerrixProvider>,
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
});

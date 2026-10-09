import { flushSync, mount, unmount } from "svelte";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { FerrixClient, MemoryAuthStorage, getAuth } from "@ferrix/client";
import { FakeServer, flushMicrotasks } from "@ferrix/client/testing";
import AuthView from "./AuthView.svelte";
import Root from "./Root.svelte";

let server: FakeServer;
let client: FerrixClient;
let target: HTMLElement;
let mounted: ReturnType<typeof mount> | null = null;

const tokens = { accessToken: "a.b.c", refreshToken: "r1", expiresIn: 900, userId: "u1" };

beforeEach(() => {
  server = new FakeServer();
  client = new FerrixClient("http://localhost:3210", { webSocketConstructor: server.WebSocket, unsavedChangesWarning: false });
  const fetch = vi.fn(async () => new Response(JSON.stringify(tokens), { status: 200 }));
  getAuth(client, { storage: new MemoryAuthStorage(), fetch: fetch as unknown as typeof globalThis.fetch, syncTabs: false });
  target = document.createElement("div");
  document.body.appendChild(target);
});

afterEach(() => {
  if (mounted) unmount(mounted);
  mounted = null;
  target.remove();
  client.close();
});

async function settle() {
  for (let i = 0; i < 5; i++) await new Promise((r) => setTimeout(r, 0));
  await flushMicrotasks();
  flushSync();
}

function text() {
  return target.querySelector("p")?.textContent?.replace(/\s+/g, " ").trim();
}

describe("useAuth / useCurrentUser", () => {
  it("track sign-in, the current user and sign-out", async () => {
    mounted = mount(Root, { target, props: { client, child: AuthView } });
    server.open();
    await settle();
    server.drain();
    expect(text()).toBe("out / none");

    target.querySelectorAll("button")[0]!.click();
    await settle();
    expect(text()).toBe("loading / ?");
    expect(server.drain()).toEqual([{ type: "Authenticate", baseVersion: 0, token: "a.b.c" }]);
    server.transition({ identity: 1, ts: 1 });
    await settle();
    expect(text()).toBe("in / ?");
    const add = server.drain().find((m) => m.type === "ModifyQuerySet");
    expect(add).toMatchObject({ modifications: [{ type: "Add", udfPath: "_system/auth:me" }] });
    server.transition({ querySet: 1, ts: 2 }, [
      { type: "QueryUpdated", queryId: 0, value: { _id: "u1", email: "a@b.co", name: null } },
    ]);
    flushSync();
    expect(text()).toBe("in / a@b.co");

    target.querySelectorAll("button")[1]!.click();
    await settle();
    expect(text()).toBe("out / none");
  });
});

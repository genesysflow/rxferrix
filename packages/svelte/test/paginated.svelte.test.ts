import { flushSync, mount, unmount } from "svelte";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { RxFerrixClient, type PaginationResult } from "@rxferrix/client";
import { FakeServer, flushMicrotasks } from "@rxferrix/client/testing";
import { usePaginatedQuery } from "../src/lib/index.js";
import { api, type Message } from "./api.js";
import MessageList from "./MessageList.svelte";
import Root from "./Root.svelte";

let server: FakeServer;
let client: RxFerrixClient;
let target: HTMLElement;
let mounted: ReturnType<typeof mount> | null = null;

beforeEach(async () => {
  server = new FakeServer();
  client = new RxFerrixClient("http://localhost:3210", {
    webSocketConstructor: server.WebSocket,
    unsavedChangesWarning: false,
  });
  server.open();
  await flushMicrotasks();
  server.drain();
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
  await flushMicrotasks();
  flushSync();
  await flushMicrotasks();
  flushSync();
}

const page = (ids: string[], continueCursor: string, isDone = false): PaginationResult<Message> => ({
  page: ids.map((id) => ({ _id: id as Message["_id"], body: id })),
  isDone,
  continueCursor,
});
const update = (queryId: number, value: unknown) => ({ type: "QueryUpdated" as const, queryId, value, logLines: [] });

function sentModifications(): unknown[] {
  return server.drain().flatMap((m) => (m.type === "ModifyQuerySet" ? m.modifications : []));
}
const status = () => target.querySelector(".status")?.textContent?.replace(/\s+/g, " ").trim();
const items = () => [...target.querySelectorAll("li")].map((li) => li.textContent);
function deliver(end: { querySet?: number; ts: number }, mods: ReturnType<typeof update>[]) {
  server.transition(end, mods);
  flushSync();
}

describe("usePaginatedQuery", () => {
  it("loads pages, pins the earlier page, and resets when the args change", async () => {
    const props = $state({ channel: "a" as string | null });
    mounted = mount(Root, { target, props: { client, child: MessageList, props } });
    flushSync();
    expect(status()).toBe("LoadingFirstPage loading");
    await settle();
    expect(sentModifications()).toEqual([
      { type: "Add", queryId: 0, udfPath: "messages:list", args: { channel: "a", paginationOpts: { numItems: 2, cursor: null } } },
    ]);
    deliver({ querySet: 1, ts: 1 }, [update(0, page(["m1", "m2"], "c1"))]);
    expect(items()).toEqual(["m1", "m2"]);
    expect(status()).toBe("CanLoadMore idle");

    target.querySelector("button")!.click();
    flushSync();
    expect(status()).toBe("LoadingMore loading");
    await settle();
    expect(sentModifications()).toEqual([
      {
        type: "Add",
        queryId: 1,
        udfPath: "messages:list",
        args: { channel: "a", paginationOpts: { numItems: 2, cursor: null, endCursor: "c1" } },
      },
      { type: "Add", queryId: 2, udfPath: "messages:list", args: { channel: "a", paginationOpts: { numItems: 3, cursor: "c1" } } },
    ]);
    deliver({ querySet: 2, ts: 1 }, [update(2, page(["m3"], "end", true))]);
    expect(items()).toEqual(["m1", "m2", "m3"]);
    expect(status()).toBe("Exhausted idle");
    deliver({ ts: 2 }, [update(1, page(["m0", "m1", "m2"], "c1"))]);
    expect(items()).toEqual(["m0", "m1", "m2", "m3"]);
    await settle();
    expect(sentModifications()).toEqual([{ type: "Remove", queryId: 0 }]);

    // New args: back to the first page.
    props.channel = "b";
    await settle();
    expect(status()).toBe("LoadingFirstPage loading");
    expect(items()).toEqual([]);
    expect(sentModifications()).toEqual([
      { type: "Add", queryId: 3, udfPath: "messages:list", args: { channel: "b", paginationOpts: { numItems: 2, cursor: null } } },
      { type: "Remove", queryId: 1 },
      { type: "Remove", queryId: 2 },
    ]);
    deliver({ querySet: 4, ts: 3 }, [update(3, page(["b1"], "e", true))]);
    expect(items()).toEqual(["b1"]);

    // Skip: everything is unsubscribed.
    props.channel = null;
    await settle();
    expect(status()).toBe("LoadingFirstPage loading");
    expect(sentModifications()).toEqual([{ type: "Remove", queryId: 3 }]);
  });

  it("exposes a failed page as `error` and unsubscribes on destroy", async () => {
    mounted = mount(Root, { target, props: { client, child: MessageList, props: { channel: "a" } } });
    await settle();
    server.drain();
    server.transition({ querySet: 1, ts: 1 }, [
      { type: "QueryFailed", queryId: 0, errorMessage: "boom", errorData: null, logLines: [] },
    ]);
    flushSync();
    expect(status()).toBe("LoadingFirstPage idle error: boom");
    unmount(mounted);
    mounted = null;
    await settle();
    expect(sentModifications()).toEqual([{ type: "Remove", queryId: 0 }]);
  });

  it("works outside components with an explicit client", async () => {
    const seen: string[][] = [];
    const stop = $effect.root(() => {
      const q = usePaginatedQuery(api.messages.list, () => ({ channel: "x" }), { initialNumItems: 1, client });
      $effect(() => {
        seen.push(q.results.map((r) => r._id));
      });
    });
    await settle();
    deliver({ querySet: 1, ts: 1 }, [update(0, page(["x1"], "c1"))]);
    expect(seen).toEqual([[], ["x1"]]);
    stop();
    await settle();
    expect(server.sentOfType("ModifyQuerySet").at(-1)?.modifications).toEqual([{ type: "Remove", queryId: 0 }]);
  });
});

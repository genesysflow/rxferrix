import { describe, expect, it } from "vitest";
import { OUTCOME_UNKNOWN, RxFerrixError } from "../src/index.js";
import { RequestManager, RESEND_WINDOW_MS } from "../src/request_manager.js";

function mutation(requests: RequestManager) {
  const requestId = requests.allocateRequestId();
  const promise = requests.trackMutation({ type: "Mutation", requestId, udfPath: "todos:add", args: {} });
  return { requestId, promise };
}

describe("RequestManager resends", () => {
  it("sends an unanswered mutation again, from the same identity, within the server's dedup window", () => {
    const requests = new RequestManager();
    const { requestId } = mutation(requests);
    requests.markSent(requestId, "alice");
    requests.onSocketClosed();
    expect(requests.expireUnsafeResends("alice", Date.now() + RESEND_WINDOW_MS - 1_000)).toEqual([]);
    expect(requests.unsent().map((m) => m.requestId)).toEqual([requestId]);
  });

  it("gives up on resends the server wouldn't recognize: too late, or as someone else", async () => {
    const requests = new RequestManager();
    const late = mutation(requests);
    const other = mutation(requests);
    const fresh = mutation(requests);
    requests.markSent(late.requestId, "alice");
    requests.markSent(other.requestId, "alice");
    requests.onSocketClosed();
    // `fresh` never went out: it is sent as whoever is signed in by then.
    const now = Date.now();
    expect(requests.expireUnsafeResends("alice", now).map((s) => s.requestId)).toEqual([]);
    const expired = requests.expireUnsafeResends(null, now + RESEND_WINDOW_MS + 1);
    expect(expired.map((s) => s.requestId)).toEqual([late.requestId, other.requestId]);
    expired.forEach((s) => s.settle());
    for (const p of [late.promise, other.promise]) {
      const error = await p.catch((e: unknown) => e);
      expect(error).toBeInstanceOf(RxFerrixError);
      expect((error as RxFerrixError).code).toBe(OUTCOME_UNKNOWN);
    }
    expect(requests.unsent().map((m) => m.requestId)).toEqual([fresh.requestId]);
  });

  it("gives up on a resend after the identity changed, even right away", async () => {
    const requests = new RequestManager();
    const { requestId, promise } = mutation(requests);
    requests.markSent(requestId, "alice");
    requests.onSocketClosed();
    const [expired] = requests.expireUnsafeResends(null);
    expect(expired?.requestId).toBe(requestId);
    expired!.settle();
    await expect(promise).rejects.toMatchObject({ code: OUTCOME_UNKNOWN, message: expect.stringContaining("another user") });
  });
});

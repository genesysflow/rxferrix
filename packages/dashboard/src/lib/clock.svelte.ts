/**
 * A clock that ticks while the calling component is mounted (call it during component setup). Queries compute
 * time-derived values such as a session's `expired` when they run, and a live query reruns when its data changes,
 * not when time passes: check against `now` too so such values don't go stale while a page stays open.
 */
export function ticking(intervalMs = 30_000): { readonly now: number } {
  let now = $state(Date.now());
  $effect(() => {
    const timer = setInterval(() => (now = Date.now()), intervalMs);
    return () => clearInterval(timer);
  });
  return {
    get now() {
      return now;
    },
  };
}

/** A session is expired if the query said so or its expiry has passed since. */
export function sessionExpired(s: { expired: boolean; expiresAt: number }, now: number): boolean {
  return s.expired || s.expiresAt <= now;
}

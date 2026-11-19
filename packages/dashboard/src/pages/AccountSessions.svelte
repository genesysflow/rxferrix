<script lang="ts">
  import { RxFerrixError } from "@rxferrix/client";
  import { useMutation, useQuery } from "@rxferrix/svelte";
  import { dashboard } from "../lib/api.js";
  import { ticking, sessionExpired } from "../lib/clock.svelte.js";
  import { dashboardContext } from "../lib/context.js";
  import { describeUserAgent, formatAgo, formatTime } from "../lib/format.js";

  /** The signed-in administrator's own sessions (live), each revocable except the one this browser uses. */
  const ctx = dashboardContext();
  const sessions = useQuery(dashboard.mySessions, () => ({}));
  const revokeSession = useMutation(dashboard.revokeSession);
  // Sessions being revoked: dimmed until the live list drops them.
  let revoking = $state<Set<string>>(new Set());
  const clock = ticking();

  async function revoke(id: string) {
    if (!confirm("Revoke this session? The browser using it is signed out of the dashboard at once.")) return;
    revoking = new Set([...revoking, id]);
    try {
      await revokeSession({ session: id });
      ctx.toast("Session revoked: that browser was signed out");
    } catch (e) {
      ctx.toast(e instanceof RxFerrixError ? e.message : String(e), "error");
    } finally {
      const next = new Set(revoking);
      next.delete(id);
      revoking = next;
    }
  }
</script>

<section class="card block" data-testid="account-sessions">
  <h2>Sessions</h2>
  <p class="muted small">Where your administrator account is signed in. The list updates live.</p>
  {#if sessions.error}
    <div class="alert" role="alert">{sessions.error.message}</div>
  {:else if sessions.isLoading}
    <p class="faint">Loading…</p>
  {:else if (sessions.data ?? []).length === 0}
    <p class="faint">No sessions.</p>
  {:else}
    <ul class="sessions">
      {#each sessions.data ?? [] as s (s._id)}
        <li class:expired={sessionExpired(s, clock.now)} class:revoking={revoking.has(s._id)}>
          <div class="what">
            <div class="client" title={s.userAgent ?? ""}>
              <span class="label">{describeUserAgent(s.userAgent) ?? s.userAgent ?? "Unknown client"}</span>
              {#if s.current}<span class="badge success">this browser</span>{/if}
            </div>
            <div class="faint meta">
              {#if s.ip}<span class="mono">{s.ip}</span> ·{/if}
              <span title={formatTime(s.lastUsedAt)}>last used {formatAgo(s.lastUsedAt, clock.now)}</span> ·
              <span title={formatTime(s._creationTime)}>signed in {formatAgo(s._creationTime, clock.now)}</span> ·
              {sessionExpired(s, clock.now) ? "expired" : `expires ${formatTime(s.expiresAt)}`}
            </div>
          </div>
          {#if !s.current}
            <button class="small danger" onclick={() => revoke(s._id)} disabled={revoking.has(s._id)}
              >{revoking.has(s._id) ? "Revoking…" : "Revoke"}</button
            >
          {/if}
        </li>
      {/each}
    </ul>
    <p class="muted small note">
      Revoking ends a session at once: its refresh token stops working and its open dashboard is signed out. To end this
      browser's session, sign out.
    </p>
  {/if}
</section>

<style>
  .block {
    padding: 14px 18px;
  }
  .block h2 {
    margin: 0 0 8px;
    font-size: 15px;
  }
  .block p {
    margin: 6px 0;
  }
  .small {
    font-size: 12.5px;
  }
  .note {
    margin-top: 12px;
  }
  .sessions {
    list-style: none;
    margin: 10px 0 0;
    padding: 0;
  }
  .sessions li {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 8px 0;
    border-top: 1px solid var(--border);
  }
  .sessions li:first-child {
    border-top: none;
  }
  .what {
    flex: 1;
    min-width: 0;
  }
  .client {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 13.5px;
  }
  .label {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .meta {
    font-size: 12px;
    margin-top: 2px;
  }
  .expired {
    color: var(--faint);
  }
  .revoking {
    opacity: 0.5;
  }
</style>

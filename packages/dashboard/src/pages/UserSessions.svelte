<script lang="ts">
  import { RxFerrixError } from "@rxferrix/client";
  import { useMutation, useQuery } from "@rxferrix/svelte";
  import { dashboard } from "../lib/api.js";
  import { dashboardContext } from "../lib/context.js";
  import { formatAgo, formatTime } from "../lib/format.js";

  let { user }: { user: string } = $props();
  const ctx = dashboardContext();
  const sessions = useQuery(dashboard.userSessions, () => ({ user }));
  const me = useQuery(dashboard.me, () => ({}));
  const revokeSession = useMutation(dashboard.revokeSession);
  // Sessions being revoked: dimmed until the live list drops them.
  let revoking = $state<Set<string>>(new Set());

  async function revoke(id: string) {
    revoking = new Set([...revoking, id]);
    try {
      await revokeSession({ session: id });
      ctx.toast("Session revoked: its connections were signed out");
    } catch (e) {
      ctx.toast(e instanceof RxFerrixError ? e.message : String(e), "error");
    } finally {
      const next = new Set(revoking);
      next.delete(id);
      revoking = next;
    }
  }
</script>

<h3>Sessions</h3>
{#if sessions.isLoading}
  <p class="faint">Loading…</p>
{:else if (sessions.data ?? []).length === 0}
  <p class="faint">No sessions.</p>
{:else}
  <table class="sessions" data-testid="sessions">
    <thead><tr><th>Last used</th><th>Kind</th><th>Client</th><th>IP</th><th>Expires</th><th></th></tr></thead>
    <tbody>
      {#each sessions.data ?? [] as s (s._id)}
        <tr class:expired={s.expired} class:revoking={revoking.has(s._id)}>
          <td title={formatTime(s.lastUsedAt)}>{formatAgo(s.lastUsedAt)}</td>
          <td>
            {#if s.impersonatedBy}
              <span class="badge imp" title={`An impersonation session, opened by administrator ${s.impersonatedBy}`}
                >impersonated by {s.impersonatedBy === me.data?._id ? "you" : s.impersonatedBy}</span
              >
            {:else}
              <span class="faint">sign-in</span>
            {/if}
          </td>
          <td class="ua" title={s.userAgent ?? ""}>{s.userAgent ?? "—"}</td>
          <td class="mono">{s.ip ?? "—"}</td>
          <td>{s.expired ? "expired" : formatTime(s.expiresAt)}</td>
          <td>
            <button class="small danger" onclick={() => revoke(s._id)} disabled={revoking.has(s._id)}
              >{revoking.has(s._id) ? "Revoking…" : "Revoke"}</button
            >
          </td>
        </tr>
      {/each}
    </tbody>
  </table>
  <p class="faint note">
    Revoking ends the session at once: its refresh token stops working and its open connections are signed out
    immediately (live revocation). The list updates live.
  </p>
{/if}

<style>
  h3 {
    margin: 4px 0 6px;
  }
  .sessions {
    border-collapse: collapse;
    font-size: 13px;
  }
  .sessions th,
  .sessions td {
    text-align: left;
    padding: 3px 12px 3px 0;
  }
  .sessions th {
    color: var(--muted);
    font-weight: 500;
  }
  .ua {
    max-width: 320px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .expired {
    color: var(--faint);
  }
  .revoking {
    opacity: 0.5;
  }
  .badge.imp {
    color: var(--warn);
    background: var(--warn-soft);
    border-color: transparent;
  }
  .note {
    font-size: 12px;
    margin: 6px 0 0;
  }
</style>

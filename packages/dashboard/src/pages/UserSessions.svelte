<script lang="ts">
  import { FerrixError } from "@ferrix/client";
  import { useMutation, useQuery } from "@ferrix/svelte";
  import { dashboard } from "../lib/api.js";
  import { dashboardContext } from "../lib/context.js";
  import { formatAgo, formatTime } from "../lib/format.js";

  let { user }: { user: string } = $props();
  const ctx = dashboardContext();
  const sessions = useQuery(dashboard.userSessions, () => ({ user }));
  const revokeSession = useMutation(dashboard.revokeSession);

  async function revoke(id: string) {
    try {
      await revokeSession({ session: id });
      ctx.toast("Session revoked");
    } catch (e) {
      ctx.toast(e instanceof FerrixError ? e.message : String(e), "error");
    }
  }
</script>

<h3>Sessions</h3>
{#if sessions.isLoading}
  <p class="faint">Loading…</p>
{:else if (sessions.data ?? []).length === 0}
  <p class="faint">No sessions.</p>
{:else}
  <table class="sessions">
    <thead><tr><th>Last used</th><th>Client</th><th>IP</th><th>Expires</th><th></th></tr></thead>
    <tbody>
      {#each sessions.data ?? [] as s (s._id)}
        <tr class:expired={s.expired}>
          <td title={formatTime(s.lastUsedAt)}>{formatAgo(s.lastUsedAt)}</td>
          <td class="ua" title={s.userAgent ?? ""}>{s.userAgent ?? "—"}</td>
          <td class="mono">{s.ip ?? "—"}</td>
          <td>{s.expired ? "expired" : formatTime(s.expiresAt)}</td>
          <td><button class="small danger" onclick={() => revoke(s._id)}>Revoke</button></td>
        </tr>
      {/each}
    </tbody>
  </table>
  <p class="faint note">
    Revoking ends the session's refresh token at once; sockets already connected with it keep working until their
    access token expires (at most 15 minutes).
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
  .note {
    font-size: 12px;
    margin: 6px 0 0;
  }
</style>

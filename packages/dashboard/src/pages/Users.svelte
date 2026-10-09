<script lang="ts">
  import { FerrixError } from "@ferrix/client";
  import { useMutation, usePaginatedQuery } from "@ferrix/svelte";
  import { dashboard, type UserSummary } from "../lib/api.js";
  import { dashboardContext } from "../lib/context.js";
  import { formatTime } from "../lib/format.js";
  import { href, link } from "../lib/router.svelte.js";
  import Empty from "../components/Empty.svelte";
  import UserSessions from "./UserSessions.svelte";

  const ctx = dashboardContext();
  let searchText = $state("");
  let search = $state("");
  let open = $state<string | null>(null);
  const users = usePaginatedQuery(dashboard.users, () => ({ search: search || null }), { initialNumItems: 50 });
  const deleteUser = useMutation(dashboard.deleteUser);

  let timer: ReturnType<typeof setTimeout> | undefined;
  function onSearch(value: string) {
    searchText = value;
    clearTimeout(timer);
    timer = setTimeout(() => (search = value.trim()), 250);
  }

  async function remove(u: UserSummary) {
    if (!confirm(`Delete ${u.email ?? u._id} with their sessions, accounts and password? This can't be undone.`)) return;
    try {
      const d = await deleteUser({ user: u._id });
      ctx.toast(`Deleted user (${d.sessions} sessions, ${d.accounts} accounts)`);
      if (open === u._id) open = null;
    } catch (e) {
      ctx.toast(e instanceof FerrixError ? e.message : String(e), "error");
    }
  }
</script>

<div class="page">
  <header class="page-header">
    <h1>Users</h1>
    <span class="spacer"></span>
    <input
      class="search"
      placeholder="Search by email prefix"
      aria-label="Search users"
      value={searchText}
      oninput={(e) => onSearch(e.currentTarget.value)}
    />
  </header>
  <div class="body">
    {#if users.error}
      <div class="alert" role="alert">{users.error.message}</div>
    {:else if users.isLoading && users.results.length === 0}
      <Empty title="Loading…" />
    {:else if users.results.length === 0}
      <Empty
        title={search ? "No users match" : "No users yet"}
        hint={search ? "Search matches the start of the email address." : "Users appear here when they sign up."}
      />
    {:else}
      <table class="grid">
        <thead>
          <tr><th>Email</th><th>Name</th><th>Verified</th><th>Sign-in</th><th>Sessions</th><th>Created</th><th></th></tr>
        </thead>
        <tbody>
          {#each users.results as u (u._id)}
            <tr onclick={() => (open = open === u._id ? null : u._id)} class:selected={open === u._id} aria-expanded={open === u._id}>
              <td>{u.email ?? "—"}</td>
              <td>{u.name ?? ""}</td>
              <td>{u.emailVerified ? "✓" : ""}</td>
              <td>
                {#if u.hasPassword}<span class="badge">password</span>{/if}
                {#each u.providers as p (p)}<span class="badge">{p}</span>{/each}
              </td>
              <td>{u.sessions}</td>
              <td class="faint">{formatTime(u._creationTime)}</td>
              <td class="actions" onclick={(e) => e.stopPropagation()}>
                <a href={href("data", ["_users"], { doc: u._id })} onclick={link}>Document</a>
                <button class="small danger" onclick={() => remove(u)}>Delete</button>
              </td>
            </tr>
            {#if open === u._id}
              <tr class="detail">
                <td colspan="7"><UserSessions user={u._id} /></td>
              </tr>
            {/if}
          {/each}
        </tbody>
      </table>
      <div class="more">
        {#if users.status === "CanLoadMore"}<button onclick={() => users.loadMore(50)}>Load more</button>{/if}
      </div>
    {/if}
  </div>
</div>

<style>
  .body {
    flex: 1;
    overflow: auto;
  }
  .search {
    width: 260px;
  }
  .actions {
    display: flex;
    gap: 10px;
    align-items: center;
  }
  tr.detail {
    cursor: default;
    background: var(--panel-2);
  }
  tr.detail td {
    max-width: none;
    padding: 8px 16px 14px;
  }
  .more {
    padding: 12px 20px;
  }
</style>

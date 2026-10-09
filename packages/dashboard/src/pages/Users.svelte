<script lang="ts">
  import { FerrixError } from "@ferrix/client";
  import { useAction, useMutation, usePaginatedQuery } from "@ferrix/svelte";
  import { dashboard, settings, type Impersonation, type UserSummary } from "../lib/api.js";
  import { impersonate } from "../lib/admin.js";
  import { dashboardContext } from "../lib/context.js";
  import { formatTime } from "../lib/format.js";
  import { href, link } from "../lib/router.svelte.js";
  import Empty from "../components/Empty.svelte";
  import Modal from "../components/Modal.svelte";
  import UserSessions from "./UserSessions.svelte";

  const ctx = dashboardContext();
  let searchText = $state("");
  let search = $state("");
  let open = $state<string | null>(null);
  const users = usePaginatedQuery(dashboard.users, () => ({ search: search || null }), { initialNumItems: 50 });
  const deleteUser = useMutation(dashboard.deleteUser);
  const getSettings = useAction(settings.get);

  // "Open app as user": a short-lived impersonation session, opened in a new tab.
  let opening = $state<UserSummary | null>(null);
  let redirectUrls = $state<string[] | null>(null);
  let redirectTo = $state("");
  let ttlMinutes = $state(15);
  let impersonating = $state(false);
  let impersonateError = $state<string | null>(null);
  let opened = $state<(Impersonation & { email: string }) | null>(null);

  async function startImpersonation(u: UserSummary) {
    opening = u;
    opened = null;
    impersonateError = null;
    if (redirectUrls === null) {
      try {
        redirectUrls = (await getSettings({})).effective.redirectUrls;
      } catch (e) {
        redirectUrls = [];
        impersonateError = e instanceof Error ? e.message : String(e);
      }
    }
    if (!redirectTo) redirectTo = redirectUrls[0] ?? "";
  }

  async function openApp() {
    const u = opening;
    if (!u || !redirectTo) return;
    impersonating = true;
    impersonateError = null;
    // Open the tab now, while the click still counts as a user gesture (popup blockers), and point it at the app
    // once the session exists. It gets no reference back to the dashboard.
    const tab = window.open("about:blank", "_blank");
    try {
      const r = await impersonate(ctx.auth, { user: u._id, ttlSeconds: ttlMinutes * 60, redirectTo });
      if (r.url && tab) {
        tab.opener = null;
        tab.location.replace(r.url);
      } else if (r.url) {
        window.open(r.url, "_blank", "noopener,noreferrer");
      }
      opened = { ...r, url: null, email: u.email ?? u._id };
      opening = null;
      ctx.toast(`Opened the app as ${u.email ?? u._id} until ${formatTime(r.expiresAt).slice(11)}`);
    } catch (e) {
      tab?.close();
      impersonateError = e instanceof FerrixError ? e.message : String(e);
    } finally {
      impersonating = false;
    }
  }

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
                <button class="small" onclick={() => startImpersonation(u)} title="Open the app signed in as this user, in a new tab"
                  >Open app as user</button
                >
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

{#if opened}
  <div class="banner" role="status" data-testid="impersonation-banner">
    <span>⚠ An impersonation session for <strong>{opened.email}</strong> is open in another tab until
      <strong>{formatTime(opened.expiresAt)}</strong> (it can't be extended). Revoke it from the user's sessions to end it early.</span>
    <button class="small ghost" onclick={() => (opened = null)} aria-label="Dismiss">✕</button>
  </div>
{/if}

{#if opening}
  {@const u = opening}
  <Modal title={`Open the app as ${u.email ?? u._id}`} onclose={() => (opening = null)}>
    <div class="alert info warning">
      You will act with <strong>all of this user's rights</strong>: everything you do is done as them. The session is
      audited, marked as impersonated, listed with their sessions and revocable, and it expires on its own.
    </div>
    <p class="muted small">
      The app opens in a new tab and stores the session in that browser's app storage, replacing whoever is signed in to
      the app there. Use a private window (copy the tab's address) to keep your own app session.
    </p>
    <div class="form">
      <label for="imp-redirect">App URL</label>
      {#if redirectUrls === null}
        <span class="faint">Loading…</span>
      {:else if redirectUrls.length === 0}
        <span class="error-text">No redirect URL is allowed: add the app's URL under Settings → Redirect URLs (or
          <code>--auth-redirect-url</code>).</span>
      {:else}
        <select id="imp-redirect" bind:value={redirectTo}>
          {#each redirectUrls as r (r)}<option value={r}>{r}</option>{/each}
        </select>
      {/if}
      <label for="imp-ttl">Expires after</label>
      <select id="imp-ttl" bind:value={ttlMinutes}>
        <option value={5}>5 minutes</option>
        <option value={15}>15 minutes</option>
        <option value={30}>30 minutes</option>
        <option value={60}>1 hour (the maximum)</option>
      </select>
    </div>
    <p class="faint small">Expires at {formatTime(Date.now() + ttlMinutes * 60_000)}; refreshing never extends it.</p>
    {#if impersonateError}<div class="alert" role="alert">{impersonateError}</div>{/if}
    {#snippet actions()}
      <button onclick={() => (opening = null)}>Cancel</button>
      <button class="primary" onclick={openApp} disabled={impersonating || !redirectTo}
        >{impersonating ? "Opening…" : "Open app as user"}</button
      >
    {/snippet}
  </Modal>
{/if}

<style>
  .banner {
    position: fixed;
    left: 50%;
    bottom: 16px;
    transform: translateX(-50%);
    display: flex;
    gap: 10px;
    align-items: center;
    max-width: min(760px, calc(100vw - 260px));
    padding: 8px 12px;
    border-radius: 8px;
    background: var(--warn-soft);
    color: var(--warn);
    border: 1px solid color-mix(in srgb, var(--warn) 40%, transparent);
    box-shadow: var(--shadow);
    z-index: 50;
  }
  .warning {
    white-space: normal;
  }
  .small {
    font-size: 12.5px;
  }
  .form {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: 8px 12px;
    align-items: center;
    margin: 10px 0;
  }
  .body {
    flex: 1;
    overflow: auto;
  }
  .search {
    width: 260px;
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

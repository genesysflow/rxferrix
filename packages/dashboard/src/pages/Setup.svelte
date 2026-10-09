<script lang="ts">
  import { FerrixError } from "@ferrix/client";
  import { onMount } from "svelte";
  import { dashboardContext } from "../lib/context.js";
  import { href, link, router } from "../lib/router.svelte.js";
  import AuthCard from "./AuthCard.svelte";

  const { auth } = dashboardContext();
  // Take the one-time token out of the fragment and the address bar right away (it never reaches the server except in
  // the POST body, and shouldn't linger in history).
  const token = new URLSearchParams(location.hash.slice(1)).get("token") ?? "";
  if (location.hash) history.replaceState(null, "", location.pathname + location.search);

  let email = $state("");
  let password = $state("");
  let confirm = $state("");
  let error = $state<string | null>(null);
  let busy = $state(false);
  let needsSetup = $state<boolean | null>(null);

  onMount(() => {
    auth.needsAdminSetup().then(
      (n) => (needsSetup = n),
      () => (needsSetup = true),
    );
  });

  async function submit(e: SubmitEvent) {
    e.preventDefault();
    if (password !== confirm) {
      error = "The passwords don't match";
      return;
    }
    busy = true;
    error = null;
    try {
      await auth.setupAdmin({ token, email, password });
      router.navigate(href("overview"), { replace: true });
    } catch (err) {
      error = err instanceof FerrixError ? err.message : String(err);
    } finally {
      busy = false;
    }
  }
</script>

{#if needsSetup === false}
  <AuthCard title="Setup is complete" subtitle="An administrator already exists.">
    <a href={href("overview")} onclick={link}>Sign in →</a>
  </AuthCard>
{:else if !token}
  <AuthCard title="Setup link incomplete" subtitle="This page needs the one-time link that `serve` prints.">
    <p class="muted">
      Look for <code>/_/setup#token=…</code> in the server's output (it is valid until used or until restart), or run
      <code>admin create &lt;email&gt;</code>.
    </p>
  </AuthCard>
{:else}
  <AuthCard title="Create the first administrator" subtitle="This one-time link works once.">
    <form onsubmit={submit}>
      <label>Email <input type="email" bind:value={email} required autocomplete="username" /></label>
      <label
        >Password (8+ characters)
        <input type="password" bind:value={password} minlength="8" required autocomplete="new-password" /></label
      >
      <label
        >Confirm password
        <input type="password" bind:value={confirm} minlength="8" required autocomplete="new-password" /></label
      >
      {#if error}<div class="alert" role="alert">{error}</div>{/if}
      <button class="primary" disabled={busy}>{busy ? "Creating…" : "Create administrator"}</button>
    </form>
  </AuthCard>
{/if}

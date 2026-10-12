<script lang="ts">
  import { RxFerrixError } from "@rxferrix/client";
  import { dashboardContext } from "../lib/context.js";
  import AuthCard from "./AuthCard.svelte";

  const { auth } = dashboardContext();
  let email = $state("");
  let password = $state("");
  let error = $state<string | null>(null);
  let busy = $state(false);

  async function submit(e: SubmitEvent) {
    e.preventDefault();
    busy = true;
    error = null;
    try {
      await auth.signInAsAdmin({ email, password });
    } catch (err) {
      error = err instanceof RxFerrixError ? err.message : String(err);
    } finally {
      busy = false;
    }
  }
</script>

<AuthCard title="Sign in" subtitle="Administrator access to this RxFerrix server.">
  <form onsubmit={submit}>
    <label>Email <input type="email" bind:value={email} required autocomplete="username" /></label>
    <label>Password <input type="password" bind:value={password} required autocomplete="current-password" /></label>
    {#if error}<div class="alert" role="alert">{error}</div>{/if}
    <button class="primary" disabled={busy}>{busy ? "Signing in…" : "Sign in"}</button>
  </form>
  <p class="faint hint">
    No administrator yet? Open the one-time <code>/_/setup#token=…</code> link that <code>serve</code> prints, or run
    <code>admin create &lt;email&gt;</code>.
  </p>
</AuthCard>

<style>
  .hint {
    font-size: 12.5px;
    margin: 6px 0 0;
  }
</style>

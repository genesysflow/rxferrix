<script lang="ts">
  import { RxFerrixError } from "@rxferrix/client";
  import { dashboardContext } from "../lib/context.js";
  import AuthCard from "./AuthCard.svelte";

  const { auth } = dashboardContext();
  let email = $state("");
  let password = $state("");
  let error = $state<string | null>(null);
  let busy = $state(false);
  /** Set once the password was accepted for an administrator with two-factor authentication: the code step. */
  let mfaToken = $state<string | null>(null);
  let code = $state("");
  let codeInput = $state<HTMLInputElement | undefined>();

  async function submit(e: SubmitEvent) {
    e.preventDefault();
    busy = true;
    error = null;
    try {
      const r = await auth.signInAsAdmin({ email, password });
      if (r.mfaRequired) {
        mfaToken = r.mfaToken;
        password = "";
        setTimeout(() => codeInput?.focus(), 0);
      }
    } catch (err) {
      error = err instanceof RxFerrixError ? err.message : String(err);
    } finally {
      busy = false;
    }
  }

  async function submitCode(e: SubmitEvent) {
    e.preventDefault();
    if (!mfaToken) return;
    busy = true;
    error = null;
    try {
      await auth.verifyMfa(mfaToken, code);
    } catch (err) {
      error = err instanceof RxFerrixError ? err.message : String(err);
      // The second-factor token lasts 5 minutes: after that, start over with the password.
      if (err instanceof RxFerrixError && err.code === "invalid_mfa_token") startOver();
    } finally {
      busy = false;
    }
  }

  function startOver() {
    mfaToken = null;
    code = "";
  }
</script>

{#if mfaToken}
  <AuthCard title="Two-factor authentication" subtitle="Enter the code from your authenticator app, or a recovery code.">
    <form onsubmit={submitCode}>
      <label
        >Code
        <input
          bind:this={codeInput}
          bind:value={code}
          required
          autocomplete="one-time-code"
          inputmode="text"
          spellcheck="false"
          placeholder="123 456 or xxxxx-xxxxx"
        /></label
      >
      {#if error}<div class="alert" role="alert">{error}</div>{/if}
      <button class="primary" disabled={busy || !code.trim()}>{busy ? "Verifying…" : "Verify"}</button>
    </form>
    <p class="faint hint">
      Signed in as <span class="mono">{email}</span>. <button class="link" type="button" onclick={startOver}>Start over</button>
    </p>
  </AuthCard>
{:else}
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
{/if}

<style>
  .hint {
    font-size: 12.5px;
    margin: 6px 0 0;
  }
  .link {
    background: none;
    border: 0;
    padding: 0;
    color: var(--link);
    font-size: inherit;
    cursor: pointer;
  }
</style>

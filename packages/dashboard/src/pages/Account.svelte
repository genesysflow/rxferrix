<script lang="ts">
  import { RxFerrixError, type MfaSetup, type MfaStatus } from "@rxferrix/client";
  import { useQuery } from "@rxferrix/svelte";
  import { dashboard } from "../lib/api.js";
  import { dashboardContext } from "../lib/context.js";
  import Modal from "../components/Modal.svelte";

  /**
   * The signed-in administrator's own account: two-factor authentication with an authenticator app (TOTP) and
   * recovery codes, through the same `/api/auth/mfa/*` endpoints apps use for their users.
   */
  const ctx = dashboardContext();
  const me = useQuery(dashboard.me, () => ({}));

  let status = $state<MfaStatus | null>(null);
  let loadError = $state<string | null>(null);

  async function load() {
    loadError = null;
    try {
      status = await ctx.auth.getMfaStatus();
    } catch (e) {
      loadError = errorText(e);
    }
  }
  // On mount, and whenever `me` (live) changes: a reset from the command line shows here without a reload.
  $effect(() => {
    void me.data?.mfaEnabled;
    void load();
  });

  const errorText = (e: unknown) => (e instanceof RxFerrixError ? e.message : e instanceof Error ? e.message : String(e));

  // Enabling: password → QR code → first code → recovery codes.
  let enabling = $state<"password" | "scan" | "codes" | null>(null);
  let password = $state("");
  let setup = $state<MfaSetup | null>(null);
  let showSecret = $state(false);
  let code = $state("");
  let recoveryCodes = $state<string[] | null>(null);
  let busy = $state(false);
  let stepError = $state<string | null>(null);

  function startEnable() {
    enabling = "password";
    password = "";
    setup = null;
    code = "";
    stepError = null;
    showSecret = false;
  }

  async function beginSetup(e: SubmitEvent) {
    e.preventDefault();
    busy = true;
    stepError = null;
    try {
      setup = await ctx.auth.setupTotp({ password });
      password = "";
      enabling = "scan";
    } catch (err) {
      stepError = errorText(err);
    } finally {
      busy = false;
    }
  }

  async function confirm(e: SubmitEvent) {
    e.preventDefault();
    busy = true;
    stepError = null;
    try {
      const r = await ctx.auth.confirmTotp(code);
      recoveryCodes = r.recoveryCodes;
      code = "";
      enabling = "codes";
      await load();
      ctx.toast("Two-factor authentication is on: your next sign-in needs a code");
    } catch (err) {
      stepError = errorText(err);
    } finally {
      busy = false;
    }
  }

  async function cancelSetup() {
    const pending = enabling === "scan";
    enabling = null;
    setup = null;
    if (pending) {
      // Drop the unconfirmed factor so the status doesn't show a pending setup.
      try {
        await ctx.auth.disableMfa();
      } catch {
        // Nothing enabled yet either way.
      }
      await load();
    }
  }

  // Disabling and regenerating codes need a current code.
  let asking = $state<"disable" | "regenerate" | null>(null);
  let askCode = $state("");

  function ask(what: "disable" | "regenerate") {
    asking = what;
    askCode = "";
    stepError = null;
  }

  async function submitAsk(e: SubmitEvent) {
    e.preventDefault();
    busy = true;
    stepError = null;
    try {
      if (asking === "disable") {
        await ctx.auth.disableMfa(askCode);
        asking = null;
        ctx.toast("Two-factor authentication is off");
      } else {
        const r = await ctx.auth.regenerateRecoveryCodes(askCode);
        recoveryCodes = r.recoveryCodes;
        asking = null;
        enabling = "codes";
      }
      await load();
    } catch (err) {
      stepError = errorText(err);
    } finally {
      busy = false;
    }
  }

  async function copyCodes() {
    if (!recoveryCodes) return;
    try {
      await navigator.clipboard.writeText(recoveryCodes.join("\n"));
      ctx.toast("Recovery codes copied");
    } catch {
      ctx.toast("The browser refused to copy", "error");
    }
  }

  function downloadCodes() {
    if (!recoveryCodes) return;
    const text = `RxFerrix recovery codes for ${me.data?.email ?? "your administrator account"}\n\n${recoveryCodes.join("\n")}\n`;
    const url = URL.createObjectURL(new Blob([text], { type: "text/plain" }));
    const a = document.createElement("a");
    a.href = url;
    a.download = "rxferrix-recovery-codes.txt";
    a.click();
    URL.revokeObjectURL(url);
  }
</script>

<div class="page">
  <header class="page-header">
    <h1>Account</h1>
    <span class="muted small">Your administrator account on this server.</span>
  </header>
  <div class="page-body">
    <section class="card block">
      <h2>Signed in as</h2>
      <p class="mono">{me.data?.email ?? "…"}</p>
      <p class="muted small">
        To change the password, run <code>admin reset-password &lt;email&gt;</code> on the server (it ends every session
        of that administrator).
      </p>
    </section>

    <section class="card block" data-testid="mfa">
      <h2>Two-factor authentication</h2>
      {#if loadError}
        <div class="alert" role="alert">{loadError}</div>
        <button onclick={load}>Retry</button>
      {:else if !status}
        <p class="faint">Loading…</p>
      {:else if status.enabled}
        <p><span class="badge success">on</span> Signing in needs a code from your authenticator app, or a recovery code.</p>
        <p class="muted small">
          Recovery codes left: <strong>{status.recoveryCodesLeft}</strong>. Each one signs you in once if the
          authenticator is unavailable.
        </p>
        <div class="row">
          <button onclick={() => ask("regenerate")}>New recovery codes…</button>
          <button class="danger" onclick={() => ask("disable")}>Turn off…</button>
        </div>
      {:else}
        <p>
          <span class="badge">off</span> Add a second step to signing in: a code from an authenticator app (Google
          Authenticator, 1Password, Authy, …) on top of the password.
        </p>
        {#if status.pending}
          <p class="muted small">A setup was started but never confirmed; starting again replaces it.</p>
        {/if}
        <div class="row"><button class="primary" onclick={startEnable}>Turn on…</button></div>
      {/if}
      <p class="muted small note">
        Lost the authenticator and the recovery codes? Another administrator can't reset yours from here: run
        <code>admin disable-2fa &lt;email&gt;</code> on the server. Users' factors are reset from the Users page.
      </p>
    </section>
  </div>
</div>

{#if enabling === "password"}
  <Modal title="Turn on two-factor authentication" onclose={cancelSetup}>
    <form class="steps" onsubmit={beginSetup}>
      <p class="muted small">Confirm your password first.</p>
      <label>Password <input type="password" bind:value={password} required autocomplete="current-password" /></label>
      {#if stepError}<div class="alert" role="alert">{stepError}</div>{/if}
    </form>
    {#snippet actions()}
      <button onclick={cancelSetup}>Cancel</button>
      <button class="primary" onclick={(e) => beginSetup(e as unknown as SubmitEvent)} disabled={busy || !password}
        >{busy ? "Starting…" : "Continue"}</button
      >
    {/snippet}
  </Modal>
{:else if enabling === "scan" && setup}
  <Modal title="Scan the QR code" onclose={cancelSetup} dismissible={false}>
    <form class="steps" onsubmit={confirm}>
      <p class="muted small">
        Open your authenticator app, add an account by scanning this code, then enter the 6-digit code it shows.
      </p>
      <div class="qr" aria-label="QR code for the authenticator app">{@html setup.qrSvg}</div>
      {#if showSecret}
        <p class="small">Or enter this key by hand: <code class="secret">{setup.secret}</code></p>
      {:else}
        <button class="small ghost" type="button" onclick={() => (showSecret = true)}>Can't scan? Show the key</button>
      {/if}
      <label
        >Code from the app
        <input bind:value={code} required autocomplete="one-time-code" inputmode="numeric" placeholder="123 456" /></label
      >
      {#if stepError}<div class="alert" role="alert">{stepError}</div>{/if}
    </form>
    {#snippet actions()}
      <button onclick={cancelSetup}>Cancel</button>
      <button class="primary" onclick={(e) => confirm(e as unknown as SubmitEvent)} disabled={busy || !code.trim()}
        >{busy ? "Checking…" : "Turn on"}</button
      >
    {/snippet}
  </Modal>
{:else if enabling === "codes" && recoveryCodes}
  <Modal title="Save your recovery codes" onclose={() => (enabling = null)} dismissible={false}>
    <div class="steps">
      <div class="alert info">
        These codes are shown <strong>once</strong>. Each signs you in one time if you lose the authenticator. Keep them
        somewhere safe.
      </div>
      <ul class="codes mono" data-testid="recovery-codes">
        {#each recoveryCodes as c (c)}<li>{c}</li>{/each}
      </ul>
      <div class="row">
        <button class="small" type="button" onclick={copyCodes}>Copy</button>
        <button class="small" type="button" onclick={downloadCodes}>Download</button>
      </div>
    </div>
    {#snippet actions()}
      <button class="primary" onclick={() => (enabling = null)}>I saved them</button>
    {/snippet}
  </Modal>
{/if}

{#if asking}
  <Modal title={asking === "disable" ? "Turn off two-factor authentication" : "New recovery codes"} onclose={() => (asking = null)}>
    <form class="steps" onsubmit={submitAsk}>
      <p class="muted small">
        {asking === "disable"
          ? "Enter a current code from your authenticator app, or a recovery code. Signing in will need the password only."
          : "Enter a current code from your authenticator app, or a recovery code. The old recovery codes stop working."}
      </p>
      <label>Code <input bind:value={askCode} required autocomplete="one-time-code" placeholder="123 456 or xxxxx-xxxxx" /></label>
      {#if stepError}<div class="alert" role="alert">{stepError}</div>{/if}
    </form>
    {#snippet actions()}
      <button onclick={() => (asking = null)}>Cancel</button>
      <button
        class={asking === "disable" ? "danger" : "primary"}
        onclick={(e) => submitAsk(e as unknown as SubmitEvent)}
        disabled={busy || !askCode.trim()}>{busy ? "Checking…" : asking === "disable" ? "Turn off" : "Replace codes"}</button
      >
    {/snippet}
  </Modal>
{/if}

<style>
  .page-body {
    display: flex;
    flex-direction: column;
    gap: 14px;
    max-width: 640px;
  }
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
  .row {
    display: flex;
    gap: 8px;
    margin-top: 10px;
  }
  .steps {
    display: flex;
    flex-direction: column;
    gap: 10px;
    min-width: min(360px, 80vw);
  }
  .steps label {
    display: flex;
    flex-direction: column;
    gap: 4px;
    font-size: 13px;
    color: var(--muted);
  }
  .qr {
    width: 200px;
    height: 200px;
    margin: 4px auto;
    background: white;
    padding: 6px;
    border-radius: 6px;
  }
  .qr :global(svg) {
    width: 100%;
    height: 100%;
    display: block;
  }
  .secret {
    user-select: all;
    word-break: break-all;
  }
  .codes {
    columns: 2;
    list-style: none;
    margin: 0;
    padding: 10px 14px;
    background: var(--panel-2);
    border-radius: var(--radius);
    font-size: 14px;
    line-height: 1.8;
  }
</style>

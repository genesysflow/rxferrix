<script lang="ts">
  import { FerrixError } from "@ferrix/client";
  import { useAction } from "@ferrix/svelte";
  import { onMount } from "svelte";
  import { settings, type OAuthKind, type SettingsKey, type SettingsView } from "../lib/api.js";
  import { dashboardContext } from "../lib/context.js";
  import { formatSeconds } from "../lib/format.js";
  import {
    ACCESS_TTL_RANGE,
    buildUpdate,
    formFromView,
    isLocked,
    newProvider,
    sectionOfError,
    SESSION_TTL_RANGE,
    validate,
    type SecretField,
    type SettingsForm,
  } from "../lib/settings.js";
  import Empty from "../components/Empty.svelte";

  /**
   * Runtime settings (`_system/settings:*`, actions): sign-ups, token lifetimes, redirect URLs, CORS origins, SMTP and
   * OAuth providers. Fields the configuration fixes are read-only; secrets are write-only.
   */
  const ctx = dashboardContext();
  const get = useAction(settings.get);
  const update = useAction(settings.update);
  const sendTestEmail = useAction(settings.sendTestEmail);

  let view = $state<SettingsView | null>(null);
  let form = $state<SettingsForm | null>(null);
  let loadError = $state<string | null>(null);
  let saving = $state(false);
  let showErrors = $state(false);
  let serverError = $state<{ section: SettingsKey | "general"; message: string } | null>(null);
  let testTo = $state("");
  let testing = $state(false);
  let testResult = $state<{ ok: boolean; message: string } | null>(null);
  let newKind = $state<OAuthKind>("github");

  async function load() {
    loadError = null;
    try {
      const v = await get({});
      view = v;
      form = formFromView(v);
      showErrors = false;
      serverError = null;
    } catch (e) {
      loadError = e instanceof Error ? e.message : String(e);
    }
  }
  onMount(() => void load());

  const errors = $derived(form && view ? validate(form, view) : {});
  const pending = $derived(form && view ? buildUpdate(view, form) : null);
  const dirty = $derived((pending?.changed.length ?? 0) > 0);
  const err = (key: string) => (showErrors ? errors[key] : undefined);
  const sectionError = (section: SettingsKey) => (serverError?.section === section ? serverError.message : null);

  async function save() {
    if (!form || !view || !pending) return;
    showErrors = true;
    serverError = null;
    if (Object.keys(errors).length > 0) {
      ctx.toast("Fix the highlighted fields first", "error");
      return;
    }
    if (!dirty) return;
    saving = true;
    const { update: changes, changed } = pending;
    try {
      const v = await update(changes);
      view = v;
      form = formFromView(v);
      showErrors = false;
      ctx.toast(`Saved ${changed.join(", ")}; in effect now`);
    } catch (e) {
      const message = e instanceof Error ? e.message : String(e);
      if (e instanceof FerrixError && e.code === "invalid_settings") serverError = { section: sectionOfError(message), message };
      else serverError = { section: "general", message };
    } finally {
      saving = false;
    }
  }

  function discard() {
    if (view) form = formFromView(view);
    showErrors = false;
    serverError = null;
  }

  async function testEmail() {
    if (!form || !view || !testTo.trim()) return;
    testing = true;
    testResult = null;
    try {
      // Unsaved SMTP changes are tested as typed (a kept password means the stored one).
      const smtpChanged = pending?.changed.includes("smtp") && pending.update.smtp;
      await sendTestEmail({ to: testTo.trim(), smtp: smtpChanged ? pending!.update.smtp! : null });
      testResult = { ok: true, message: `Sent to ${testTo.trim()}${smtpChanged ? " with the unsaved SMTP settings" : ""}` };
    } catch (e) {
      testResult = { ok: false, message: e instanceof Error ? e.message : String(e) };
    } finally {
      testing = false;
    }
  }

  function addProvider() {
    if (!form) return;
    const p = newProvider(newKind);
    if (form.oauth.some((x) => x.id === p.id)) p.id = "";
    form.oauth = [...form.oauth, p];
  }

  const ttlHint = (text: string, effective: number) => {
    const n = Number(text);
    return text.trim() === "" ? `default: ${formatSeconds(effective)}` : Number.isFinite(n) && n > 0 ? `= ${formatSeconds(n)}` : "";
  };
</script>

{#snippet lockedNote(what: string, effective: string)}
  <p class="locked" data-testid="locked-note">
    <span aria-hidden="true">🔒</span>
    <span
      ><strong>{what} is set by the server's configuration</strong> (in code, a command-line flag or an environment
      variable), which takes precedence over settings, so it can't be changed here. In effect: <span class="mono">{effective}</span></span
    >
  </p>
{/snippet}

{#snippet secretInput(s: SecretField, label: string, id: string, allowNone: boolean, onchange: (s: SecretField) => void, disabled: boolean, multiline = false)}
  <div class="secret">
    {#if s.mode === "keep" && s.stored}
      <input {id} class="mono" value="••••" readonly aria-label={`${label} (stored, hidden)`} disabled />
      <span class="faint small">stored, never shown</span>
      {#if !disabled}
        <button class="small" type="button" onclick={() => onchange({ ...s, mode: "replace", value: "" })}>Replace</button>
        {#if allowNone}<button class="small ghost" type="button" onclick={() => onchange({ ...s, mode: "none", value: "" })}>Remove</button>{/if}
      {/if}
    {:else if s.mode === "none"}
      <span class="muted small">No {label.toLowerCase()} will be stored.</span>
      <button class="small" type="button" onclick={() => onchange({ ...s, mode: "keep", value: "" })}>Undo</button>
    {:else if s.mode === "keep"}
      <span class="faint small">Not set</span>
      {#if !disabled}<button class="small" type="button" onclick={() => onchange({ ...s, mode: "replace", value: "" })}>Set {label.toLowerCase()}</button>{/if}
    {:else if multiline}
      <!-- A PEM file keeps its line breaks; the text is masked with CSS where supported. -->
      <textarea
        {id}
        class="mono pem"
        rows="5"
        spellcheck="false"
        autocomplete="off"
        aria-label={label}
        placeholder="-----BEGIN PRIVATE KEY-----"
        value={s.value}
        oninput={(e) => onchange({ ...s, value: e.currentTarget.value })}
        {disabled}
      ></textarea>
      {#if s.stored}<button class="small ghost" type="button" onclick={() => onchange({ ...s, mode: "keep", value: "" })}>Keep current</button>{/if}
    {:else}
      <input
        {id}
        type="password"
        autocomplete="new-password"
        aria-label={label}
        value={s.value}
        oninput={(e) => onchange({ ...s, value: e.currentTarget.value })}
        {disabled}
      />
      {#if s.stored}<button class="small ghost" type="button" onclick={() => onchange({ ...s, mode: "keep", value: "" })}>Keep current</button>{/if}
    {/if}
  </div>
{/snippet}

{#snippet fieldError(message: string | null | undefined)}
  {#if message}<div class="field-error" role="alert">{message}</div>{/if}
{/snippet}

<div class="page">
  <header class="page-header">
    <h1>Settings</h1>
    <span class="muted small">Applied without a restart; the server's configuration wins where it sets a value.</span>
    <span class="spacer"></span>
    {#if form}
      {#if dirty}<span class="badge userError" data-testid="unsaved">unsaved changes</span>{/if}
      <button onclick={discard} disabled={!dirty || saving}>Discard</button>
      <button class="primary" onclick={save} disabled={!dirty || saving}>{saving ? "Saving…" : "Save changes"}</button>
    {/if}
  </header>
  <div class="page-body">
    {#if loadError}
      <div class="alert" role="alert">{loadError}</div>
      <button onclick={load}>Retry</button>
    {:else if !form || !view}
      <Empty title="Loading settings…" />
    {:else}
      {@const v = view}
      {#if serverError?.section === "general"}<div class="alert" role="alert">{serverError.message}</div>{/if}

      <section class="card section" aria-labelledby="s-signups">
        <h2 id="s-signups">Sign-ups</h2>
        {#if isLocked(v, "signups")}
          {@render lockedNote("Sign-ups", v.effective.signups ? "allowed" : "disabled")}
        {:else}
          <div class="field">
            <label for="signups">New accounts</label>
            <select id="signups" bind:value={form.signups}>
              <option value="default">Default{v.signups === null ? ` (${v.effective.signups ? "allowed" : "disabled"})` : ""}</option>
              <option value="on">Allowed</option>
              <option value="off">Disabled: only existing users can sign in</option>
            </select>
          </div>
          {@render fieldError(sectionError("signups"))}
        {/if}
      </section>

      <section class="card section" aria-labelledby="s-ttl">
        <h2 id="s-ttl">Token lifetimes</h2>
        <p class="muted small">Apply to tokens and sessions issued from now on.</p>
        {#each [{ key: "accessTokenTtlSecs", label: "Access token", range: ACCESS_TTL_RANGE, effective: v.effective.accessTokenTtlSecs }, { key: "sessionTtlSecs", label: "Session", range: SESSION_TTL_RANGE, effective: v.effective.sessionTtlSecs }] as const as t (t.key)}
          {#if isLocked(v, t.key)}
            {@render lockedNote(`The ${t.label.toLowerCase()} lifetime`, formatSeconds(t.effective))}
          {:else}
            <div class="field">
              <label for={t.key}>{t.label} lifetime (seconds)</label>
              <div>
                <input
                  id={t.key}
                  class="num"
                  inputmode="numeric"
                  placeholder={String(t.effective)}
                  bind:value={form[t.key]}
                  aria-invalid={err(t.key) ? "true" : undefined}
                />
                <span class="faint small">{ttlHint(form[t.key], t.effective)} · {formatSeconds(t.range[0])} to {formatSeconds(t.range[1])}</span>
                {@render fieldError(err(t.key) ?? sectionError(t.key))}
              </div>
            </div>
          {/if}
        {/each}
      </section>

      <section class="card section" aria-labelledby="s-redirects">
        <h2 id="s-redirects">Redirect URLs</h2>
        <p class="muted small">
          Where OAuth, magic links, email verification and "open app as user" may send people: one URL (or prefix) per line.
        </p>
        {#if isLocked(v, "redirectUrls")}
          {@render lockedNote("The redirect allowlist", v.effective.redirectUrls.join(", ") || "(none)")}
        {:else}
          <textarea
            rows="3"
            aria-label="Redirect URLs"
            placeholder="https://app.example.com/"
            bind:value={form.redirectUrls}
            aria-invalid={err("redirectUrls") ? "true" : undefined}
          ></textarea>
          {@render fieldError(err("redirectUrls") ?? sectionError("redirectUrls"))}
        {/if}
      </section>

      <section class="card section" aria-labelledby="s-cors">
        <h2 id="s-cors">CORS origins</h2>
        <p class="muted small">
          Browser origins allowed to call the API from another site, one per line (<code>*</code> for any). Empty: any
          origin{v.effective.corsOrigins ? "" : " (in effect now)"}.
        </p>
        <textarea
          rows="3"
          aria-label="CORS origins"
          placeholder="https://app.example.com"
          bind:value={form.corsOrigins}
          aria-invalid={err("corsOrigins") ? "true" : undefined}
        ></textarea>
        {@render fieldError(err("corsOrigins") ?? sectionError("corsOrigins"))}
      </section>

      <section class="card section" aria-labelledby="s-smtp">
        <h2 id="s-smtp">Email (SMTP)</h2>
        <p class="muted small">
          Used for verification codes, magic links and password resets. {v.effective.emailConfigured ? "Email is configured." : "Email is not configured."}
        </p>
        {#if isLocked(v, "smtp")}
          {@render lockedNote("Email delivery", "a mailer configured on the server")}
        {:else}
          <div class="field">
            <label for="smtp-url">SMTP URL</label>
            <div>
              <input
                id="smtp-url"
                class="mono wide"
                placeholder="smtps://user@smtp.example.com:465"
                bind:value={form.smtp.url}
                aria-invalid={err("smtp.url") ? "true" : undefined}
              />
              {@render fieldError(err("smtp.url"))}
            </div>
            <label for="smtp-from">From</label>
            <div>
              <input
                id="smtp-from"
                class="wide"
                placeholder="My App <noreply@example.com>"
                bind:value={form.smtp.from}
                aria-invalid={err("smtp.from") ? "true" : undefined}
              />
              {@render fieldError(err("smtp.from"))}
            </div>
            <label for="smtp-password">Password</label>
            <div>
              {@render secretInput(form.smtp.password, "SMTP password", "smtp-password", true, (s) => (form!.smtp.password = s), false)}
              {@render fieldError(err("smtp.password"))}
            </div>
          </div>
          {@render fieldError(sectionError("smtp"))}
        {/if}
        <div class="test row">
          <input type="email" placeholder="you@example.com" aria-label="Test email recipient" bind:value={testTo} />
          <button onclick={testEmail} disabled={testing || !testTo.trim()}>{testing ? "Sending…" : "Send test email"}</button>
          {#if testResult}
            <span class:error-text={!testResult.ok} class:ok-text={testResult.ok} role="status">{testResult.message}</span>
          {/if}
        </div>
      </section>

      <section class="card section" aria-labelledby="s-oauth">
        <h2 id="s-oauth">OAuth providers</h2>
        <p class="muted small">
          In effect: {v.effective.oauthProviders.length ? v.effective.oauthProviders.join(", ") : "none"}. Client secrets are
          encrypted at rest and never shown again.
        </p>
        {#each form.oauth as p, i (p.key)}
          <fieldset class="provider" class:locked-provider={p.locked}>
            <legend>
              <span class="mono">{p.id || "new provider"}</span>
              <span class="badge">{p.kind}</span>
              {#if p.locked}<span class="badge" title="A provider with this id is configured in code: the code wins and this entry is ignored">🔒 configured in code</span>{/if}
            </legend>
            <div class="field">
              <label for={`p${p.key}-id`}>Id</label>
              <div>
                <input id={`p${p.key}-id`} class="mono" bind:value={p.id} aria-invalid={err(`oauth.${i}.id`) ? "true" : undefined} />
                {@render fieldError(err(`oauth.${i}.id`))}
              </div>
              <label for={`p${p.key}-kind`}>Kind</label>
              <select id={`p${p.key}-kind`} bind:value={p.kind}>
                <option value="github">GitHub</option>
                <option value="google">Google</option>
                <option value="discord">Discord</option>
                <option value="apple">Sign in with Apple</option>
                <option value="oidc">OpenID Connect</option>
              </select>
              {#if p.kind === "apple"}
                <label for={`p${p.key}-team`}>Team id</label>
                <div>
                  <input id={`p${p.key}-team`} class="mono" placeholder="ABCDE12345" bind:value={p.teamId} />
                  {@render fieldError(err(`oauth.${i}.teamId`))}
                </div>
                <label for={`p${p.key}-keyid`}>Key id</label>
                <div>
                  <input id={`p${p.key}-keyid`} class="mono" placeholder="the Sign in with Apple key's id" bind:value={p.keyId} />
                  {@render fieldError(err(`oauth.${i}.keyId`))}
                </div>
              {/if}
              {#if p.kind === "oidc"}
                <label for={`p${p.key}-issuer`}>Issuer</label>
                <div>
                  <input id={`p${p.key}-issuer`} class="mono wide" placeholder="https://accounts.example.com" bind:value={p.issuer} />
                  {@render fieldError(err(`oauth.${i}.issuer`))}
                </div>
              {/if}
              <label for={`p${p.key}-client`}>{p.kind === "apple" ? "Services ID" : "Client id"}</label>
              <div>
                <input id={`p${p.key}-client`} class="mono wide" placeholder={p.kind === "apple" ? "com.example.web" : ""} bind:value={p.clientId} />
                {@render fieldError(err(`oauth.${i}.clientId`))}
              </div>
              <label for={`p${p.key}-secret`}>{p.kind === "apple" ? "Private key (.p8)" : "Client secret"}</label>
              <div>
                {#if p.kind === "apple"}
                  {@render secretInput(p.secret, "Private key", `p${p.key}-secret`, false, (s) => (p.secret = s), false, true)}
                  <div class="faint small">It signs a short-lived client secret; stored encrypted, never shown again.</div>
                {:else}
                  {@render secretInput(p.secret, "Client secret", `p${p.key}-secret`, false, (s) => (p.secret = s), false)}
                {/if}
                {@render fieldError(err(`oauth.${i}.secret`))}
              </div>
              <label for={`p${p.key}-scopes`}>Scopes</label>
              <input id={`p${p.key}-scopes`} class="mono wide" placeholder="the preset's defaults" bind:value={p.scopes} />
              <span></span>
              <div class="row">
                <label class="inline"><input type="checkbox" bind:checked={p.enabled} /> enabled</label>
                <span class="spacer"></span>
                <button class="small danger" type="button" onclick={() => (form!.oauth = form!.oauth.filter((x) => x.key !== p.key))}>Remove</button>
              </div>
            </div>
          </fieldset>
        {/each}
        {@render fieldError(err("oauth") ?? sectionError("oauth"))}
        <div class="row">
          <select aria-label="New provider kind" bind:value={newKind}>
            <option value="github">GitHub</option>
            <option value="google">Google</option>
            <option value="discord">Discord</option>
            <option value="apple">Sign in with Apple</option>
            <option value="oidc">OpenID Connect</option>
          </select>
          <button onclick={addProvider}>+ Add provider</button>
        </div>
      </section>
    {/if}
  </div>
</div>

<style>
  textarea.pem {
    width: min(100%, 520px);
    font-size: 12px;
    -webkit-text-security: disc;
  }
  .small {
    font-size: 12.5px;
  }
  .page-body {
    display: flex;
    flex-direction: column;
    gap: 14px;
    max-width: 900px;
  }
  .section {
    padding: 14px 16px;
  }
  .section h2 {
    margin-bottom: 4px;
  }
  .section > p {
    margin: 0 0 10px;
  }
  .field {
    display: grid;
    grid-template-columns: 170px 1fr;
    gap: 8px 12px;
    align-items: start;
    margin: 6px 0;
  }
  .field > label {
    padding-top: 6px;
    color: var(--muted);
    font-size: 13px;
  }
  .field select {
    justify-self: start;
  }
  input.num {
    width: 120px;
    margin-right: 8px;
  }
  input.wide {
    width: min(100%, 440px);
  }
  textarea {
    width: 100%;
    font-family: var(--mono);
  }
  [aria-invalid="true"] {
    border-color: var(--err);
  }
  .field-error {
    color: var(--err);
    font-size: 12.5px;
    margin-top: 3px;
  }
  .secret {
    display: flex;
    gap: 8px;
    align-items: center;
    flex-wrap: wrap;
  }
  .secret input {
    width: 220px;
  }
  .locked {
    display: flex;
    gap: 8px;
    margin: 4px 0;
    padding: 8px 10px;
    background: var(--panel-2);
    border-radius: var(--radius);
    font-size: 13px;
    color: var(--muted);
  }
  .locked strong {
    color: var(--text);
  }
  .test {
    margin-top: 10px;
    padding-top: 10px;
    border-top: 1px dashed var(--border);
  }
  .test input {
    width: 240px;
  }
  .ok-text {
    color: var(--ok);
  }
  .provider {
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 6px 12px 10px;
    margin: 0 0 10px;
  }
  .provider legend {
    display: flex;
    gap: 6px;
    align-items: center;
    padding: 0 4px;
  }
  .locked-provider {
    background: var(--panel-2);
  }
  @media (max-width: 760px) {
    .field {
      grid-template-columns: 1fr;
      gap: 4px;
    }
    .secret,
    .test {
      width: 100%;
    }
  }
</style>

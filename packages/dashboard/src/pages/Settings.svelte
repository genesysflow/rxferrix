<script lang="ts">
  import { RxFerrixError } from "@rxferrix/client";
  import { useAction } from "@rxferrix/svelte";
  import { onMount } from "svelte";
  import { fetchClientIp } from "../lib/admin.js";
  import { settings, type ClientIp, type EmailKind, type OAuthKind, type SettingsKey, type SettingsView } from "../lib/api.js";
  import { dashboardContext } from "../lib/context.js";
  import { formatSeconds } from "../lib/format.js";
  import {
    ACCESS_TTL_RANGE,
    addHeader,
    buildUpdate,
    COMMON_PROXY_HEADERS,
    EMAIL_KINDS,
    EMAIL_PLACEHOLDERS,
    formFromView,
    isLocked,
    newProvider,
    newRateLimitRule,
    previewTemplate,
    sectionOfError,
    SESSION_TTL_RANGE,
    validate,
    type SecretField,
    type SettingsForm,
  } from "../lib/settings.js";
  import Empty from "../components/Empty.svelte";

  /**
   * Runtime settings (`_system/settings:*`, actions): sign-ups, token lifetimes, redirect URLs, CORS origins, trusted
   * proxy headers, rate limits, SMTP, the auth email templates and OAuth providers. Fields the configuration fixes are
   * read-only; secrets are write-only.
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
  let clientIp = $state<ClientIp | null>(null);
  let emailKind = $state<EmailKind>("verifyEmail");
  let htmlArea = $state<HTMLTextAreaElement | null>(null);

  // Put `{name}` where the cursor is in the HTML (at the end if it never had the focus).
  function insertPlaceholder(name: string) {
    const t = form?.emailTemplates[emailKind];
    if (!t) return;
    const at = htmlArea?.selectionStart ?? t.html.length;
    const end = htmlArea?.selectionEnd ?? at;
    t.html = `${t.html.slice(0, at)}{${name}}${t.html.slice(end)}`;
    const caret = at + name.length + 2;
    requestAnimationFrame(() => {
      htmlArea?.focus();
      htmlArea?.setSelectionRange(caret, caret);
    });
  }

  const preview = $derived.by(() => {
    const t = form?.emailTemplates[emailKind];
    if (!t || !form || !view) return null;
    return previewTemplate(t, emailKind, form.emailTemplates.appName.trim() || view.effective.appName);
  });

  // How the server sees this browser's IP with the trusted proxy headers in effect (refreshed after a save).
  async function loadClientIp() {
    try {
      clientIp = await fetchClientIp(ctx.auth);
    } catch {
      clientIp = null;
    }
  }

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
  onMount(() => {
    void load();
    void loadClientIp();
  });

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
      if (changed.includes("trustedProxy")) void loadClientIp();
    } catch (e) {
      const message = e instanceof Error ? e.message : String(e);
      if (e instanceof RxFerrixError && e.code === "invalid_settings") serverError = { section: sectionOfError(message), message };
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

  function addRateLimitRule() {
    if (!form) return;
    const first = form.rateLimits.rules.length === 0;
    form.rateLimits.rules = [...form.rateLimits.rules, newRateLimitRule(first)];
    if (first) form.rateLimits.enabled = true;
  }

  // Headers to offer: those on this very request first (they reached the server), then the common ones.
  const headerSuggestions = $derived.by(() => {
    const seen = (clientIp?.proxyHeaders ?? []).map((h) => COMMON_PROXY_HEADERS.find((c) => c.toLowerCase() === h.name) ?? h.name);
    return [...new Set([...seen, ...COMMON_PROXY_HEADERS])];
  });

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

      <section class="card section" aria-labelledby="s-proxy">
        <h2 id="s-proxy">IP proxy headers</h2>
        <p class="muted small">
          Behind a reverse proxy, load balancer or CDN (NGINX, Caddy, Cloudflare, Fly, …) every request reaches the server
          from the proxy's address. To see the real client IP (for rate limits, sessions and the audit log), list the
          headers your proxy puts it in. The first header present wins.
        </p>
        <div class="resolved" data-testid="resolved-ip" role="status">
          {#if clientIp}
            <div>Your IP, as the server sees it: <strong class="mono">{clientIp.ip ?? "unknown"}</strong></div>
            <div>
              Taken from:
              {#if clientIp.header}<strong class="mono">{clientIp.header}</strong>
              {:else}<strong>the connection</strong> <span class="faint">(no trusted header)</span>{/if}
            </div>
            {#if clientIp.proxyHeaders.length}
              <div class="faint">
                Proxy headers on this request: {#each clientIp.proxyHeaders as h, i (h.name)}{i ? ", " : ""}<span class="mono">{h.name}: {h.value}</span>{/each}
              </div>
            {/if}
          {:else}
            <div class="faint">Checking your IP…</div>
          {/if}
        </div>
        <p class="muted small">
          If that isn't your real IP, set the header your proxy uses. To keep clients from forging their IP, trust only
          headers the proxy always sets (overwriting what the client sent), and either list the proxy's addresses or
          make sure the server can only be reached through the proxy. Leave the field empty when there is no proxy.
        </p>
        {#if isLocked(v, "trustedProxy")}
          {@render lockedNote("The trusted proxy header", `${v.effective.trustedProxy.headers.join(", ")} (rightmost IP)${v.effective.trustedProxy.proxies?.length ? `, from ${v.effective.trustedProxy.proxies.join(", ")}` : ""}`)}
        {:else}
          <div class="field">
            <label for="proxy-headers">Trusted headers</label>
            <div>
              <div class="row">
                <input
                  id="proxy-headers"
                  class="mono wide"
                  placeholder="none: not behind a proxy"
                  bind:value={form.proxyHeaders}
                  aria-invalid={err("trustedProxy") ? "true" : undefined}
                />
                <button class="small ghost" type="button" onclick={() => (form!.proxyHeaders = "")} disabled={form.proxyHeaders === ""}>Clear</button>
              </div>
              <div class="chips faint small">
                Comma-separated, such as:
                {#each headerSuggestions as h (h)}
                  <button class="chip mono" type="button" onclick={() => (form!.proxyHeaders = addHeader(form!.proxyHeaders, h))}>{h}</button>
                {/each}
              </div>
              {@render fieldError(err("trustedProxy") ?? sectionError("trustedProxy"))}
            </div>
            <label for="proxy-priority">IP priority</label>
            <div>
              <select id="proxy-priority" bind:value={form.useLeftmostIp}>
                <option value={false}>Use rightmost IP</option>
                <option value={true}>Use leftmost IP</option>
              </select>
              <div class="faint small">
                For lists such as <span class="mono">X-Forwarded-For: client, proxy1, proxy2</span>. The rightmost entry was
                added by your proxy and can't be forged; the leftmost is the original client only if every proxy on the
                way can be trusted.
              </div>
            </div>
            {#if isLocked(v, "trustedProxy.proxies")}
              <span>Proxy addresses</span>
              <div>{@render lockedNote("The proxy address list", (v.effective.trustedProxy.proxies ?? []).join(", "))}</div>
            {:else}
            <label for="proxy-addresses">Proxy addresses</label>
            <div>
              <input
                id="proxy-addresses"
                class="mono wide"
                placeholder="any: the server is reachable only through the proxy"
                bind:value={form.proxyAddresses}
                aria-invalid={err("proxyAddresses") ? "true" : undefined}
              />
              <div class="faint small">
                Comma-separated IPs or CIDR ranges, such as <span class="mono">10.0.0.0/8, 127.0.0.1</span>. The headers then
                count only on requests from these addresses; a client that reaches the server directly is seen at its own
                address.
              </div>
              {@render fieldError(err("proxyAddresses"))}
            </div>
            {/if}
          </div>
        {/if}
      </section>

      <section class="card section" aria-labelledby="s-rate">
        <div class="section-head">
          <h2 id="s-rate">Rate limiting</h2>
          <span class="spacer"></span>
          {#if v.rateLimits?.enabled && v.rateLimits.rules.length}<span class="badge success" data-testid="rate-limits-on">Enabled</span>{/if}
        </div>
        <p class="muted small">
          Limit how many requests one client IP can make (the IP the proxy headers above resolve). Each rule allows a
          burst of up to <em>max requests</em>, refilled evenly over the interval; past it, requests get
          <span class="mono">429 rate_limited</span> with <span class="mono">Retry-After</span>.
        </p>
        <label class="inline toggle">
          <input type="checkbox" role="switch" aria-label="Enable rate limiting" bind:checked={form.rateLimits.enabled} />
          Enable
        </label>
        {#if form.rateLimits.rules.length}
          <table class="rules" aria-label="Rate limit rules">
            <thead>
              <tr>
                <th>Rate limit label</th>
                <th class="n">Max requests <span class="faint">(per IP)</span></th>
                <th class="n">Interval <span class="faint">(seconds)</span></th>
                <th>Targeted users</th>
                <th aria-label="Remove"></th>
              </tr>
            </thead>
            <tbody>
              {#each form.rateLimits.rules as r, i (r.key)}
                <tr>
                  <td>
                    <input
                      class="mono"
                      aria-label={`Rule ${i + 1} label`}
                      placeholder="/api/ or messages:send"
                      bind:value={r.label}
                      aria-invalid={err(`rateLimits.${i}.label`) ? "true" : undefined}
                    />
                    {@render fieldError(err(`rateLimits.${i}.label`))}
                  </td>
                  <td>
                    <input
                      class="num"
                      inputmode="numeric"
                      aria-label={`Rule ${i + 1} max requests`}
                      bind:value={r.maxRequests}
                      aria-invalid={err(`rateLimits.${i}.maxRequests`) ? "true" : undefined}
                    />
                    {@render fieldError(err(`rateLimits.${i}.maxRequests`))}
                  </td>
                  <td>
                    <input
                      class="num"
                      inputmode="numeric"
                      aria-label={`Rule ${i + 1} interval in seconds`}
                      bind:value={r.intervalSecs}
                      aria-invalid={err(`rateLimits.${i}.intervalSecs`) ? "true" : undefined}
                    />
                    {@render fieldError(err(`rateLimits.${i}.intervalSecs`))}
                  </td>
                  <td>
                    <select aria-label={`Rule ${i + 1} targeted users`} bind:value={r.audience}>
                      <option value="all">All</option>
                      <option value="guest">Guests</option>
                      <option value="signedIn">Signed-in users</option>
                    </select>
                  </td>
                  <td>
                    <button
                      class="small ghost"
                      type="button"
                      aria-label={`Remove rule ${i + 1}`}
                      onclick={() => (form!.rateLimits.rules = form!.rateLimits.rules.filter((x) => x.key !== r.key))}>×</button
                    >
                  </td>
                </tr>
              {/each}
            </tbody>
          </table>
        {/if}
        <button class="add-rule" type="button" onclick={addRateLimitRule} disabled={form.rateLimits.rules.length >= 50}>+ Add rate limit rule</button>
        {@render fieldError(err("rateLimits") ?? sectionError("rateLimits"))}
        <details class="small muted rules-help">
          <summary>How rules match</summary>
          <ul>
            <li>
              <span class="mono">/api/</span>, <span class="mono">/api/storage/upload</span>, <span class="mono">/webhooks/</span>: HTTP
              requests whose path starts with it. The longest matching prefix decides. WebSocket connections
              (<span class="mono">/api/sync</span>) and session refreshes (<span class="mono">/api/auth/refresh</span>) only count
              against a rule naming them, not against <span class="mono">/api/</span> or <span class="mono">/</span>, so the dashboard
              stays reachable.
            </li>
            <li>
              <span class="mono">messages:send</span>, <span class="mono">messages:*</span>, <span class="mono">*:create</span>,
              <span class="mono">*</span>: your functions, called over HTTP or the WebSocket (mutations and actions; live query
              subscriptions aren't counted). The most specific match decides.
            </li>
            <li>A rule for guests or signed-in users goes before an <em>All</em> rule with the same label.</li>
            <li>
              Never limited: the dashboard and its API, <span class="mono">/api/health</span>, <span class="mono">/metrics</span>,
              system functions, and administrators.
            </li>
          </ul>
        </details>
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

      <section class="card section" aria-labelledby="s-templates">
        <h2 id="s-templates">Email templates</h2>
        <p class="muted small">
          The verification, password reset and sign-in emails. Without a template, the built-in plain-text email is sent;
          with one, your HTML (and a plain-text version made from it).
        </p>
        <div class="field">
          <label for="app-name">App name</label>
          <div>
            <input
              id="app-name"
              class="wide"
              placeholder={v.effective.appName ? `${v.effective.appName} (the SMTP sender's name)` : "e.g. Acme"}
              bind:value={form.emailTemplates.appName}
              aria-invalid={err("emailTemplates.appName") ? "true" : undefined}
            />
            <div class="faint small">What <span class="mono">{"{app_name}"}</span> becomes.</div>
            {@render fieldError(err("emailTemplates.appName"))}
          </div>
        </div>
        <div class="tabs" role="tablist" aria-label="Email">
          {#each EMAIL_KINDS as k (k.kind)}
            <button
              type="button"
              role="tab"
              class="tab"
              aria-selected={emailKind === k.kind}
              onclick={() => (emailKind = k.kind)}
            >
              {k.label}{#if form.emailTemplates[k.kind]}<span class="badge">custom</span>{/if}
            </button>
          {/each}
        </div>
        {#each [emailKind] as kind (kind)}
        {@const builtIn = v.emailTemplateDefaults[kind]!}
        <div role="tabpanel" class="template">
          {#if !form.emailTemplates[kind]}
            <p class="muted small">
              Built-in plain-text email, subject <strong>{builtIn.subject}</strong>.
            </p>
            <button type="button" onclick={() => (form!.emailTemplates[kind] = { ...builtIn })}>Customize</button>
          {:else}
            <div class="field">
              <label for="template-subject">Subject</label>
              <div>
                <input
                  id="template-subject"
                  class="wide"
                  bind:value={form.emailTemplates[kind]!.subject}
                  aria-invalid={err(`emailTemplates.${kind}.subject`) ? "true" : undefined}
                />
                {@render fieldError(err(`emailTemplates.${kind}.subject`))}
              </div>
              <label for="template-html">HTML</label>
              <div>
                <textarea
                  id="template-html"
                  rows="10"
                  spellcheck="false"
                  bind:this={htmlArea}
                  bind:value={form.emailTemplates[kind]!.html}
                  aria-invalid={err(`emailTemplates.${kind}.html`) ? "true" : undefined}
                ></textarea>
                <div class="chips faint small">
                  Insert:
                  {#each EMAIL_PLACEHOLDERS as p (p.name)}
                    <button class="chip mono" type="button" title={p.description} onclick={() => insertPlaceholder(p.name)}>{`{${p.name}}`}</button>
                  {/each}
                </div>
                {@render fieldError(err(`emailTemplates.${kind}.html`))}
                {#if !v.effective.redirectUrls.length}
                  <div class="faint small">No redirect URL is set, so <span class="mono">{"{link}"}</span> is empty: rely on <span class="mono">{"{code}"}</span>.</div>
                {/if}
              </div>
              <span class="muted small">Preview</span>
              <div class="preview">
                <div class="preview-subject" data-testid="template-preview-subject"><span class="faint">Subject:</span> {preview?.subject}</div>
                <iframe title="Email preview" sandbox="" srcdoc={preview?.html ?? ""}></iframe>
              </div>
            </div>
            <div class="row">
              <span class="spacer"></span>
              <button class="small ghost" type="button" onclick={() => (form!.emailTemplates[kind] = { ...builtIn })}>Start over from the built-in text</button>
              <button class="small danger" type="button" onclick={() => (form!.emailTemplates[kind] = null)}>Use the built-in email</button>
            </div>
          {/if}
        </div>
        {/each}
        {@render fieldError(sectionError("emailTemplates"))}
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
  .resolved {
    margin: 0 0 10px;
    padding: 8px 12px;
    border: 1px solid var(--border);
    border-radius: var(--radius);
    background: var(--panel-2);
    font-size: 13px;
    display: flex;
    flex-direction: column;
    gap: 2px;
    overflow-wrap: anywhere;
  }
  .chips {
    display: flex;
    flex-wrap: wrap;
    gap: 4px;
    align-items: center;
    margin-top: 4px;
  }
  .chip {
    padding: 1px 6px;
    font-size: 11.5px;
  }
  .tabs {
    display: flex;
    gap: 4px;
    margin: 10px 0 0;
    border-bottom: 1px solid var(--border);
  }
  .tab {
    display: flex;
    gap: 6px;
    align-items: center;
    border: 1px solid transparent;
    border-bottom: none;
    border-radius: var(--radius) var(--radius) 0 0;
    background: none;
    margin-bottom: -1px;
  }
  .tab[aria-selected="true"] {
    border-color: var(--border);
    background: var(--panel);
    font-weight: 600;
  }
  .template {
    padding-top: 10px;
  }
  .preview {
    display: flex;
    flex-direction: column;
    border: 1px solid var(--border);
    border-radius: var(--radius);
    overflow: hidden;
  }
  .preview-subject {
    padding: 6px 10px;
    font-size: 13px;
    border-bottom: 1px solid var(--border);
    background: var(--panel-2);
    overflow-wrap: anywhere;
  }
  .preview iframe {
    width: 100%;
    height: 260px;
    border: 0;
    background: #fff;
  }
  .ok-text {
    color: var(--ok);
  }
  .section-head {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .toggle {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    margin: 2px 0 10px;
    font-size: 13px;
  }
  table.rules {
    width: 100%;
    border-collapse: collapse;
    margin-bottom: 6px;
    font-size: 13px;
  }
  table.rules th {
    text-align: left;
    font-weight: 500;
    color: var(--muted);
    font-size: 12.5px;
    padding: 4px 6px;
  }
  table.rules td {
    padding: 4px 6px;
    vertical-align: top;
    border-top: 1px solid var(--border);
  }
  table.rules td:first-child input {
    width: 100%;
  }
  table.rules input.num {
    width: 100px;
    margin-right: 0;
  }
  .add-rule {
    width: 100%;
  }
  .rules-help {
    margin-top: 8px;
  }
  .rules-help ul {
    margin: 6px 0 0;
    padding-left: 18px;
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

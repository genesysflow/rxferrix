/**
 * The Settings page's form state (unit-tested in test/settings.test.ts): an editable copy of a `SettingsView`, the
 * client-side checks (the server validates again, `invalid_settings`), and the partial `update` that sends only the
 * sections that changed.
 *
 * **Secrets.** The server never returns a stored secret, only `"••••"`. A secret field starts in `keep` mode (sent
 * back as `"••••"`, which keeps it); the admin can switch to `replace` and type a new one, or, for the SMTP
 * password, to `none`. Nothing on the page ever holds the stored value.
 */
import {
  MASK,
  type EmailKind,
  type EmailTemplate,
  type EmailTemplates,
  type OAuthKind,
  type OAuthProviderInput,
  type RateLimitAudience,
  type RateLimitSettings,
  type SettingsKey,
  type SettingsUpdate,
  type SettingsView,
  type SmtpInput,
} from "./api.js";

/** Bounds enforced by the server (crates/rxferrix/src/auth/effective.rs). */
export const ACCESS_TTL_RANGE = [30, 24 * 3600] as const;
export const SESSION_TTL_RANGE = [60, 400 * 24 * 3600] as const;
export const MAX_PROVIDERS = 20;
export const MAX_LIST = 50;
export const MAX_PROXY_HEADERS = 10;
/** Rate limit bounds (crates/rxferrix/src/settings/rate_limits.rs). */
export const MAX_RATE_LIMIT_RULES = 50;
export const MAX_REQUESTS = 1_000_000;
export const MAX_INTERVAL_SECS = 24 * 3600;
/** Headers proxies and CDNs commonly put the client IP in, offered as suggestions. */
export const COMMON_PROXY_HEADERS = ["CF-Connecting-IP", "X-Forwarded-For", "Fly-Client-IP", "X-Real-IP", "True-Client-IP"];

/** Email template limits (crates/rxferrix/src/settings/mod.rs). */
export const MAX_APP_NAME = 100;
export const MAX_SUBJECT = 200;
export const MAX_TEMPLATE_HTML = 64 * 1024;

/** The auth emails a template can replace, in the order shown. `what` names it in the server's messages. */
export const EMAIL_KINDS: { kind: EmailKind; label: string; what: string; expires: string }[] = [
  { kind: "verifyEmail", label: "Verify email", what: "verification", expires: "24 hours" },
  { kind: "resetPassword", label: "Reset password", what: "password reset", expires: "30 minutes" },
  { kind: "signIn", label: "Sign-in link", what: "sign-in", expires: "15 minutes" },
];

/** The placeholders of email templates (crates/rxferrix/src/auth/email.rs), with what they become. */
export const EMAIL_PLACEHOLDERS: { name: string; description: string }[] = [
  { name: "link", description: "the link to your app that completes the action (empty without a redirect URL)" },
  { name: "code", description: "the one-time code, to type in instead of following the link" },
  { name: "expires", description: "how long the link and code stay valid, e.g. 15 minutes" },
  { name: "app_name", description: "the app name set here" },
  { name: "email", description: "the recipient's address" },
];

export interface SecretField {
  /** `keep` the stored secret (if any), `replace` it with `value`, or (SMTP only) store `none`. */
  mode: "keep" | "replace" | "none";
  value: string;
  /** A secret is stored on the server. */
  stored: boolean;
}

export interface ProviderForm {
  /** A stable key for the list (not sent). */
  key: number;
  id: string;
  kind: OAuthKind;
  issuer: string;
  /** Apple: the developer team id and the private key's id. */
  teamId: string;
  keyId: string;
  clientId: string;
  secret: SecretField;
  enabled: boolean;
  /** Space- or comma-separated; empty for the preset's default scopes. */
  scopes: string;
  /** Configured in code: the code wins and this entry is ignored. */
  locked: boolean;
}

export interface RateLimitRuleForm {
  /** A stable key for the list (not sent). */
  key: number;
  label: string;
  /** As typed. */
  maxRequests: string;
  intervalSecs: string;
  audience: RateLimitAudience;
}

export interface SettingsForm {
  signups: "default" | "on" | "off";
  /** Seconds as typed; empty for the default. */
  accessTokenTtlSecs: string;
  sessionTtlSecs: string;
  /** One per line. */
  redirectUrls: string;
  corsOrigins: string;
  smtp: { url: string; from: string; password: SecretField };
  oauth: ProviderForm[];
  /** Comma-separated header names; empty: not behind a proxy (the peer address). */
  proxyHeaders: string;
  useLeftmostIp: boolean;
  rateLimits: { enabled: boolean; rules: RateLimitRuleForm[] };
  /** A template left null: the built-in plain-text email. */
  emailTemplates: { appName: string } & Record<EmailKind, EmailTemplate | null>;
}

let nextKey = 1;

function secret(stored: boolean): SecretField {
  return { mode: "keep", value: "", stored };
}

export function newProvider(kind: OAuthKind = "github"): ProviderForm {
  return {
    key: nextKey++,
    id: kind === "oidc" ? "" : kind,
    kind,
    issuer: "",
    teamId: "",
    keyId: "",
    clientId: "",
    secret: { mode: "replace", value: "", stored: false },
    enabled: true,
    scopes: "",
    locked: false,
  };
}

/** A blank rate limit rule; suggests `/api/` for the first one. */
export function newRateLimitRule(first = false): RateLimitRuleForm {
  return { key: nextKey++, label: first ? "/api/" : "", maxRequests: first ? "300" : "", intervalSecs: first ? "10" : "", audience: "all" };
}

export function formFromView(view: SettingsView): SettingsForm {
  return {
    signups: view.signups === null ? "default" : view.signups ? "on" : "off",
    accessTokenTtlSecs: view.accessTokenTtlSecs === null ? "" : String(view.accessTokenTtlSecs),
    sessionTtlSecs: view.sessionTtlSecs === null ? "" : String(view.sessionTtlSecs),
    redirectUrls: (view.redirectUrls ?? []).join("\n"),
    corsOrigins: (view.corsOrigins ?? []).join("\n"),
    smtp: {
      url: view.smtp?.url ?? "",
      from: view.smtp?.from ?? "",
      password: secret(view.smtp?.password != null),
    },
    oauth: view.oauth.map((p) => ({
      key: nextKey++,
      id: p.id,
      kind: p.kind,
      issuer: p.issuer ?? "",
      teamId: p.teamId ?? "",
      keyId: p.keyId ?? "",
      clientId: p.clientId,
      secret: secret(p.clientSecret != null),
      enabled: p.enabled,
      scopes: (p.scopes ?? []).join(" "),
      locked: p.locked,
    })),
    proxyHeaders: (view.trustedProxy?.headers ?? []).join(", "),
    useLeftmostIp: view.trustedProxy?.useLeftmostIp ?? false,
    rateLimits: {
      enabled: view.rateLimits?.enabled ?? false,
      rules: (view.rateLimits?.rules ?? []).map((r) => ({
        key: nextKey++,
        label: r.label,
        maxRequests: String(r.maxRequests),
        intervalSecs: String(r.intervalSecs),
        audience: r.audience,
      })),
    },
    emailTemplates: {
      appName: view.emailTemplates?.appName ?? "",
      verifyEmail: copyTemplate(view.emailTemplates?.verifyEmail),
      resetPassword: copyTemplate(view.emailTemplates?.resetPassword),
      signIn: copyTemplate(view.emailTemplates?.signIn),
    },
  };
}

/** Why `label` is not a rate limit label (mirrors the server's check), or null. */
export function rateLimitLabelError(label: string): string | null {
  const l = label.trim();
  if (l === "") return "A path such as /api/ or a function such as messages:send";
  if (l.startsWith("/")) {
    if (l.includes("*")) return `Paths match by prefix: write ${l.replace(/\*+$/, "")}`;
    if (!/^[!-~]+$/.test(l) || /[?#]/.test(l)) return "A path, without spaces, query or fragment";
    return null;
  }
  if (l === "*") return null;
  const [, module = "", name = ""] = /^([^:]+):([^:]+)$/.exec(l) ?? [];
  const moduleOk = module === "*" || (/^[A-Za-z0-9_./-]+$/.test(module) && !module.startsWith("_system"));
  const nameOk = name === "*" || /^[A-Za-z0-9_]+$/.test(name);
  return moduleOk && nameOk ? null : "A path (/api/) or a function: messages:send, messages:*, *:send or *";
}

function rateLimitsInput(form: SettingsForm): RateLimitSettings | null {
  const rules = form.rateLimits.rules.map((r) => ({
    label: r.label.trim(),
    maxRequests: Number(r.maxRequests),
    intervalSecs: Number(r.intervalSecs),
    audience: r.audience,
  }));
  return form.rateLimits.enabled || rules.length ? { enabled: form.rateLimits.enabled, rules } : null;
}

function copyTemplate(t: EmailTemplate | null | undefined): EmailTemplate | null {
  return t ? { subject: t.subject, html: t.html } : null;
}

/**
 * `template` with each known `{name}` replaced by its value (HTML-escaped with `html`), in one pass, as the server
 * does it: other braces (CSS, unknown names) stay as they are.
 */
export function fillTemplate(template: string, vars: Record<string, string>, html: boolean): string {
  return template.replace(/\{([a-z_]+)\}/g, (whole, name: string) => {
    const v = Object.hasOwn(vars, name) ? vars[name] : undefined;
    if (v === undefined) return whole;
    return html ? v.replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`) : v;
  });
}

/** The first `{name}` in `text` that isn't a placeholder. */
export function unknownPlaceholder(text: string): string | null {
  for (const m of text.matchAll(/\{([a-z_]+)\}/g)) {
    const name = m[1]!;
    if (!EMAIL_PLACEHOLDERS.some((p) => p.name === name)) return name;
  }
  return null;
}

/** A template filled with sample values, for the preview. */
export function previewTemplate(t: EmailTemplate, kind: EmailKind, appName: string): EmailTemplate {
  const expires = EMAIL_KINDS.find((k) => k.kind === kind)!.expires;
  const vars = { link: "https://app.example.com/#preview", code: "123456", expires, app_name: appName, email: "ana@example.com" };
  return { subject: fillTemplate(t.subject, vars, false), html: fillTemplate(t.html, vars, true) };
}

/** `headers` (comma-separated) with `name` added, unless already listed (case-insensitively). */
export function addHeader(headers: string, name: string): string {
  const list = lines(headers);
  if (list.some((h) => h.toLowerCase() === name.toLowerCase())) return headers;
  return [...list, name].join(", ");
}

/** Whether the configuration (code, command line or environment) fixes this field: shown read-only. */
export function isLocked(view: SettingsView, field: string): boolean {
  return view.locked.includes(field);
}

export function lines(text: string): string[] {
  return text
    .split(/[\n,]/)
    .map((s) => s.trim())
    .filter(Boolean);
}

function words(text: string): string[] {
  return text
    .split(/[\s,]+/)
    .map((s) => s.trim())
    .filter(Boolean);
}

function httpUrl(s: string): URL | null {
  try {
    const u = new URL(s);
    return u.protocol === "http:" || u.protocol === "https:" ? u : null;
  } catch {
    return null;
  }
}

/** Field errors, keyed `accessTokenTtlSecs`, `smtp.url`, `oauth.<index>.clientId`, …; empty when the form is valid. */
export function validate(form: SettingsForm, view: SettingsView): Record<string, string> {
  const errors: Record<string, string> = {};
  const ttl = (key: "accessTokenTtlSecs" | "sessionTtlSecs", [min, max]: readonly [number, number]) => {
    const text = form[key].trim();
    if (text === "" || isLocked(view, key)) return;
    const n = Number(text);
    if (!Number.isInteger(n) || n < min || n > max) errors[key] = `A whole number of seconds from ${min} to ${max}`;
  };
  ttl("accessTokenTtlSecs", ACCESS_TTL_RANGE);
  ttl("sessionTtlSecs", SESSION_TTL_RANGE);

  if (!isLocked(view, "redirectUrls")) {
    const urls = lines(form.redirectUrls);
    const bad = urls.find((u) => !httpUrl(u));
    if (bad) errors.redirectUrls = `\`${bad}\` is not an absolute http(s) URL`;
    else if (urls.length > MAX_LIST) errors.redirectUrls = `At most ${MAX_LIST} URLs`;
  }
  const origins = lines(form.corsOrigins);
  const badOrigin = origins.find((o) => o !== "*" && httpUrl(o)?.origin !== o.replace(/\/$/, ""));
  if (badOrigin) errors.corsOrigins = `\`${badOrigin}\` is not an origin like https://app.example.com (or *)`;
  else if (origins.length > MAX_LIST) errors.corsOrigins = `At most ${MAX_LIST} origins`;

  if (!isLocked(view, "smtp")) {
    const { url, from, password } = form.smtp;
    const any = url.trim() !== "" || from.trim() !== "";
    if (any) {
      if (!/^smtps?:\/\/[^/\s]+/i.test(url.trim())) errors["smtp.url"] = "Use smtp://user@host:587 or smtps://user@host:465";
      if (!/^[^@\s<>]+@[^@\s<>]+$|<[^@\s<>]+@[^@\s<>]+>\s*$/.test(from.trim())) errors["smtp.from"] = "A mailbox such as App <noreply@example.com>";
    }
    if (password.mode === "replace" && password.value === "") errors["smtp.password"] = "Type the new password, or keep the current one";
  }

  if (!isLocked(view, "trustedProxy")) {
    const headers = lines(form.proxyHeaders);
    const bad = headers.find((h) => !/^[!#$%&'*+.^_`|~0-9A-Za-z-]+$/.test(h));
    if (bad) errors.trustedProxy = `\`${bad}\` is not a valid header name`;
    else if (headers.length > MAX_PROXY_HEADERS) errors.trustedProxy = `At most ${MAX_PROXY_HEADERS} headers`;
  }

  const rules = form.rateLimits.rules;
  if (rules.length > MAX_RATE_LIMIT_RULES) errors.rateLimits = `At most ${MAX_RATE_LIMIT_RULES} rules`;
  const seenRules = new Set<string>();
  rules.forEach((r, i) => {
    const at = (field: string, message: string) => (errors[`rateLimits.${i}.${field}`] = message);
    const labelError = rateLimitLabelError(r.label);
    if (labelError) at("label", labelError);
    const whole = (text: string, max: number) => {
      const n = Number(text.trim());
      return text.trim() !== "" && Number.isInteger(n) && n >= 1 && n <= max;
    };
    if (!whole(r.maxRequests, MAX_REQUESTS)) at("maxRequests", `1 to ${MAX_REQUESTS.toLocaleString("en-US")}`);
    if (!whole(r.intervalSecs, MAX_INTERVAL_SECS)) at("intervalSecs", `1 to ${MAX_INTERVAL_SECS} seconds`);
    const id = `${r.label.trim()}\u0000${r.audience}`;
    if (!labelError && seenRules.has(id)) at("label", "Listed twice for the same users");
    seenRules.add(id);
  });
  {
    const { appName } = form.emailTemplates;
    if (appName.trim().length > MAX_APP_NAME || /[\n\r\t]/.test(appName.trim()))
      errors["emailTemplates.appName"] = `At most ${MAX_APP_NAME} characters on one line`;
    for (const { kind } of EMAIL_KINDS) {
      const t = form.emailTemplates[kind];
      if (!t) continue;
      const at = (field: "subject" | "html", message: string) => (errors[`emailTemplates.${kind}.${field}`] = message);
      const subject = t.subject.trim();
      const html = t.html.trim();
      const unknown = unknownPlaceholder(subject) ?? unknownPlaceholder(html);
      if (subject === "") at("subject", "Required");
      else if (subject.length > MAX_SUBJECT || /[\n\r\t]/.test(subject)) at("subject", `At most ${MAX_SUBJECT} characters on one line`);
      if (html === "") at("html", "Required");
      else if (new TextEncoder().encode(html).length > MAX_TEMPLATE_HTML) at("html", `At most ${MAX_TEMPLATE_HTML / 1024} KiB`);
      else if (!html.includes("{code}") && !html.includes("{link}")) at("html", "Include {code} or {link}, or nobody can use the email");
      if (unknown && !errors[`emailTemplates.${kind}.subject`] && !errors[`emailTemplates.${kind}.html`])
        at(unknownPlaceholder(subject) ? "subject" : "html", `Unknown placeholder {${unknown}}`);
    }
  }

  if (form.oauth.length > MAX_PROVIDERS) errors.oauth = `At most ${MAX_PROVIDERS} providers`;
  const seen = new Set<string>();
  form.oauth.forEach((p, i) => {
    const at = (field: string, message: string) => (errors[`oauth.${i}.${field}`] = message);
    if (!/^[a-z0-9_-]{1,40}$/.test(p.id)) at("id", "1 to 40 of a-z, 0-9, - and _");
    else if (seen.has(p.id)) at("id", "Already used by another provider");
    seen.add(p.id);
    if (p.clientId.trim() === "") at("clientId", "Required");
    if (p.kind === "oidc" && !httpUrl(p.issuer.trim())) at("issuer", "OIDC needs the issuer URL (https)");
    if (p.kind === "apple") {
      if (!/^[A-Za-z0-9]{1,64}$/.test(p.teamId.trim())) at("teamId", "The Apple developer team id (letters and digits)");
      if (!/^[A-Za-z0-9]{1,64}$/.test(p.keyId.trim())) at("keyId", "The id of the Sign in with Apple key");
      if (p.secret.mode === "replace" && p.secret.value !== "" && !p.secret.value.includes("PRIVATE KEY"))
        at("secret", "Paste the whole .p8 file (-----BEGIN PRIVATE KEY----- …)");
    }
    const hasSecret = p.secret.mode === "replace" ? p.secret.value !== "" : p.secret.stored;
    const secretName = p.kind === "apple" ? "The private key" : "A client secret";
    if (p.enabled && !hasSecret) at("secret", `${secretName} is required to enable the provider`);
    if (p.secret.mode === "replace" && p.secret.value === "" && p.secret.stored) at("secret", "Type the new secret, or keep the current one");
  });
  return errors;
}

function sameList(a: readonly string[] | null, b: readonly string[] | null): boolean {
  const x = a && a.length ? a : null;
  const y = b && b.length ? b : null;
  return JSON.stringify(x) === JSON.stringify(y);
}

function secretInput(s: SecretField, allowNone: boolean): string | null {
  if (s.mode === "replace") return s.value;
  if (s.mode === "none" && allowNone) return "";
  return s.stored ? MASK : null;
}

function providerInput(p: ProviderForm): OAuthProviderInput {
  const scopes = words(p.scopes);
  return {
    id: p.id.trim(),
    kind: p.kind,
    issuer: p.issuer.trim() || null,
    clientId: p.clientId.trim(),
    clientSecret: secretInput(p.secret, false),
    enabled: p.enabled,
    scopes: scopes.length ? scopes : null,
    teamId: p.kind === "apple" ? p.teamId.trim() || null : null,
    keyId: p.kind === "apple" ? p.keyId.trim() || null : null,
  };
}

/**
 * The `update` for what changed between `view` (as loaded) and `form`: set sections, `clear` for sections emptied
 * back to their default. Locked sections are never sent. `changed` lists the sections the update touches.
 */
export function buildUpdate(view: SettingsView, form: SettingsForm): { update: SettingsUpdate; changed: SettingsKey[] } {
  const update: SettingsUpdate = {};
  const clear: SettingsKey[] = [];
  const changed: SettingsKey[] = [];
  const set = <K extends SettingsKey>(key: K, value: SettingsUpdate[K] | null) => {
    changed.push(key);
    if (value === null) clear.push(key);
    else (update as Record<string, unknown>)[key] = value;
  };

  if (!isLocked(view, "signups")) {
    const want = form.signups === "default" ? null : form.signups === "on";
    if (want !== view.signups) set("signups", want);
  }
  for (const key of ["accessTokenTtlSecs", "sessionTtlSecs"] as const) {
    if (isLocked(view, key)) continue;
    const text = form[key].trim();
    const want = text === "" ? null : Number(text);
    if (want !== view[key]) set(key, want);
  }
  if (!isLocked(view, "redirectUrls")) {
    const want = lines(form.redirectUrls);
    if (!sameList(want, view.redirectUrls)) set("redirectUrls", want.length ? want : null);
  }
  {
    const want = lines(form.corsOrigins);
    if (!sameList(want, view.corsOrigins)) set("corsOrigins", want.length ? want : null);
  }
  if (!isLocked(view, "trustedProxy")) {
    const headers = lines(form.proxyHeaders);
    const want = headers.length ? { headers, useLeftmostIp: form.useLeftmostIp } : null;
    if (JSON.stringify(want) !== JSON.stringify(view.trustedProxy)) set("trustedProxy", want);
  }
  {
    const want = rateLimitsInput(form);
    // Rebuilt in the same key order: the server's JSON has its keys sorted.
    const have: RateLimitSettings | null = view.rateLimits && {
      enabled: view.rateLimits.enabled,
      rules: view.rateLimits.rules.map((r) => ({ label: r.label, maxRequests: r.maxRequests, intervalSecs: r.intervalSecs, audience: r.audience })),
    };
    if (JSON.stringify(want) !== JSON.stringify(have)) set("rateLimits", want);
  }
  {
    const want = emailTemplatesInput(form.emailTemplates);
    if (JSON.stringify(want) !== JSON.stringify(normalTemplates(view.emailTemplates))) set("emailTemplates", want);
  }
  if (!isLocked(view, "smtp")) {
    const { url, from, password } = form.smtp;
    const empty = url.trim() === "" && from.trim() === "";
    if (empty) {
      if (view.smtp !== null) set("smtp", null);
    } else {
      const differs =
        view.smtp === null ||
        url.trim() !== view.smtp.url ||
        from.trim() !== view.smtp.from ||
        password.mode !== "keep";
      if (differs) {
        const input: SmtpInput = { url: url.trim(), from: from.trim(), password: secretInput(password, true) };
        set("smtp", input);
      }
    }
  }
  {
    const want = form.oauth.map(providerInput);
    const before = view.oauth.map((p) => ({
      id: p.id,
      kind: p.kind,
      issuer: p.issuer,
      clientId: p.clientId,
      clientSecret: p.clientSecret === null ? null : MASK,
      enabled: p.enabled,
      scopes: p.scopes && p.scopes.length ? p.scopes : null,
      teamId: p.teamId ?? null,
      keyId: p.keyId ?? null,
    }));
    if (JSON.stringify(want) !== JSON.stringify(before)) set("oauth", want.length ? want : null);
  }
  if (clear.length) update.clear = clear;
  return { update, changed };
}

/** The templates as `update` sends them (trimmed); null when nothing is set. */
function emailTemplatesInput(t: SettingsForm["emailTemplates"]): EmailTemplates | null {
  const trim = (x: EmailTemplate | null) => (x ? { subject: x.subject.trim(), html: x.html.trim() } : null);
  return normalTemplates({
    appName: t.appName.trim() || null,
    verifyEmail: trim(t.verifyEmail),
    resetPassword: trim(t.resetPassword),
    signIn: trim(t.signIn),
  });
}

/** `t` with its keys in one order, or null when nothing is set (to compare a form with the view). */
function normalTemplates(t: EmailTemplates | null): EmailTemplates | null {
  if (!t) return null;
  const out: EmailTemplates = {
    appName: t.appName ?? null,
    verifyEmail: copyTemplate(t.verifyEmail),
    resetPassword: copyTemplate(t.resetPassword),
    signIn: copyTemplate(t.signIn),
  };
  return Object.values(out).some((v) => v !== null) ? out : null;
}

/** The form section a server `invalid_settings` message is about, to show it there. */
export function sectionOfError(message: string): SettingsKey | "general" {
  const m = message.toLowerCase();
  if (m.includes("rate limit")) return "rateLimits";
  if (m.includes("email template")) return "emailTemplates";
  if (m.includes("proxy")) return "trustedProxy";
  if (m.includes("oauth") || m.includes("provider")) return "oauth";
  if (m.includes("smtp") || m.includes("mailbox") || m.includes("from address")) return "smtp";
  if (m.includes("redirect")) return "redirectUrls";
  if (m.includes("cors") || m.includes("origin")) return "corsOrigins";
  if (m.includes("accesstokenttl") || m.includes("access token")) return "accessTokenTtlSecs";
  if (m.includes("sessionttl") || m.includes("session")) return "sessionTtlSecs";
  if (m.includes("signup") || m.includes("sign-up")) return "signups";
  return "general";
}

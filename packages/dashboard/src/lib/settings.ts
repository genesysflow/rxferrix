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
  type OAuthKind,
  type OAuthProviderInput,
  type SettingsKey,
  type SettingsUpdate,
  type SettingsView,
  type SmtpInput,
} from "./api.js";

/** Bounds enforced by the server (crates/ferrix/src/auth/effective.rs). */
export const ACCESS_TTL_RANGE = [30, 24 * 3600] as const;
export const SESSION_TTL_RANGE = [60, 400 * 24 * 3600] as const;
export const MAX_PROVIDERS = 20;
export const MAX_LIST = 50;

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
  clientId: string;
  secret: SecretField;
  enabled: boolean;
  /** Space- or comma-separated; empty for the preset's default scopes. */
  scopes: string;
  /** Configured in code: the code wins and this entry is ignored. */
  locked: boolean;
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
    clientId: "",
    secret: { mode: "replace", value: "", stored: false },
    enabled: true,
    scopes: "",
    locked: false,
  };
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
      clientId: p.clientId,
      secret: secret(p.clientSecret != null),
      enabled: p.enabled,
      scopes: (p.scopes ?? []).join(" "),
      locked: p.locked,
    })),
  };
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

  if (form.oauth.length > MAX_PROVIDERS) errors.oauth = `At most ${MAX_PROVIDERS} providers`;
  const seen = new Set<string>();
  form.oauth.forEach((p, i) => {
    const at = (field: string, message: string) => (errors[`oauth.${i}.${field}`] = message);
    if (!/^[a-z0-9_-]{1,40}$/.test(p.id)) at("id", "1 to 40 of a-z, 0-9, - and _");
    else if (seen.has(p.id)) at("id", "Already used by another provider");
    seen.add(p.id);
    if (p.clientId.trim() === "") at("clientId", "Required");
    if (p.kind === "oidc" && !httpUrl(p.issuer.trim())) at("issuer", "OIDC needs the issuer URL (https)");
    const hasSecret = p.secret.mode === "replace" ? p.secret.value !== "" : p.secret.stored;
    if (p.enabled && !hasSecret) at("secret", "A client secret is required to enable the provider");
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
    }));
    if (JSON.stringify(want) !== JSON.stringify(before)) set("oauth", want.length ? want : null);
  }
  if (clear.length) update.clear = clear;
  return { update, changed };
}

/** The form section a server `invalid_settings` message is about, to show it there. */
export function sectionOfError(message: string): SettingsKey | "general" {
  const m = message.toLowerCase();
  if (m.includes("oauth") || m.includes("provider")) return "oauth";
  if (m.includes("smtp") || m.includes("mailbox") || m.includes("from address")) return "smtp";
  if (m.includes("redirect")) return "redirectUrls";
  if (m.includes("cors") || m.includes("origin")) return "corsOrigins";
  if (m.includes("accesstokenttl") || m.includes("access token")) return "accessTokenTtlSecs";
  if (m.includes("sessionttl") || m.includes("session")) return "sessionTtlSecs";
  if (m.includes("signup") || m.includes("sign-up")) return "signups";
  return "general";
}

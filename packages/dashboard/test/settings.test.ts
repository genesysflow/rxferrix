import { describe, expect, it } from "vitest";
import { MASK, type SettingsView } from "../src/lib/api.js";
import {
  addHeader,
  buildUpdate,
  fillTemplate,
  formFromView,
  newProvider,
  newRateLimitRule,
  previewTemplate,
  rateLimitLabelError,
  sectionOfError,
  unknownPlaceholder,
  validate,
} from "../src/lib/settings.js";

function view(over: Partial<SettingsView> = {}): SettingsView {
  return {
    oauth: [],
    smtp: null,
    redirectUrls: null,
    signups: null,
    accessTokenTtlSecs: null,
    sessionTtlSecs: null,
    corsOrigins: null,
    backups: null,
    trustedProxy: null,
    rateLimits: null,
    emailTemplates: null,
    emailTemplateDefaults: {
      appName: null,
      verifyEmail: { subject: "Verify your email address", html: "<p>Please confirm your email address.</p>\n<p>Your code: <strong>{code}</strong></p>" },
      resetPassword: { subject: "Reset your password", html: "<p>Your code: <strong>{code}</strong></p>" },
      signIn: { subject: "Your sign-in link", html: "<p><a href=\"{link}\">Sign in</a></p>\n<p>Your code: <strong>{code}</strong></p>" },
    },
    locked: [],
    effective: {
      signups: true,
      accessTokenTtlSecs: 900,
      sessionTtlSecs: 2_592_000,
      redirectUrls: [],
      oauthProviders: [],
      emailConfigured: false,
      corsOrigins: null,
      backups: { target: "/data/backups", everySecs: null, keep: 7, includeAux: false, includeFiles: false, s3Available: true },
      trustedProxy: { headers: [], useLeftmostIp: false },
      appName: "",
    },
    ...over,
  };
}

const stored = view({
  smtp: { url: "smtps://me@mail.example.com:465", from: "App <a@example.com>", password: MASK },
  oauth: [
    { id: "github", kind: "github", issuer: null, clientId: "abc", clientSecret: MASK, enabled: true, scopes: null, teamId: null, keyId: null, locked: false },
  ],
  redirectUrls: ["https://app.example.com/"],
  sessionTtlSecs: 3600,
});

describe("settings form", () => {
  it("round-trips a view without changes", () => {
    for (const v of [view(), stored]) {
      const form = formFromView(v);
      expect(buildUpdate(v, form)).toEqual({ update: {}, changed: [] });
      expect(validate(form, v)).toEqual({});
    }
  });

  it("sends only the changed sections, and clears emptied ones", () => {
    const form = formFromView(stored);
    form.signups = "off";
    form.accessTokenTtlSecs = "600";
    form.sessionTtlSecs = ""; // back to the default
    form.redirectUrls = "https://app.example.com/\n https://admin.example.com/ \n";
    const { update, changed } = buildUpdate(stored, form);
    expect(changed.sort()).toEqual(["accessTokenTtlSecs", "redirectUrls", "sessionTtlSecs", "signups"]);
    expect(update).toEqual({
      signups: false,
      accessTokenTtlSecs: 600,
      redirectUrls: ["https://app.example.com/", "https://admin.example.com/"],
      clear: ["sessionTtlSecs"],
    });
  });

  it("never reveals or resends a stored secret: it is kept as the mask", () => {
    const form = formFromView(stored);
    expect(form.smtp.password).toEqual({ mode: "keep", value: "", stored: true });
    form.smtp.from = "App <b@example.com>";
    expect(buildUpdate(stored, form).update.smtp).toEqual({
      url: "smtps://me@mail.example.com:465",
      from: "App <b@example.com>",
      password: MASK,
    });
    // Changing another provider field keeps the stored client secret.
    form.oauth[0]!.clientId = "xyz";
    expect(buildUpdate(stored, form).update.oauth).toEqual([
      { id: "github", kind: "github", issuer: null, clientId: "xyz", clientSecret: MASK, enabled: true, scopes: null, teamId: null, keyId: null },
    ]);
  });

  it("replaces or removes the SMTP password only when asked", () => {
    const form = formFromView(stored);
    form.smtp.password = { ...form.smtp.password, mode: "replace", value: "new secret" };
    expect(buildUpdate(stored, form).update.smtp?.password).toBe("new secret");
    form.smtp.password = { ...form.smtp.password, mode: "none", value: "" };
    expect(buildUpdate(stored, form).update.smtp?.password).toBe("");
    // A replace with nothing typed is an error, not a silent removal.
    form.smtp.password = { ...form.smtp.password, mode: "replace", value: "" };
    expect(validate(form, stored)["smtp.password"]).toMatch(/new password/);
  });

  it("clears SMTP when its fields are emptied", () => {
    const form = formFromView(stored);
    form.smtp.url = "";
    form.smtp.from = " ";
    expect(buildUpdate(stored, form)).toMatchObject({ update: { clear: ["smtp"] }, changed: ["smtp"] });
  });

  it("adds and removes OAuth providers", () => {
    const form = formFromView(stored);
    const google = newProvider("google");
    google.clientId = "g-client";
    google.secret.value = "g-secret";
    google.scopes = "openid, email profile";
    form.oauth.push(google);
    expect(buildUpdate(stored, form).update.oauth?.[1]).toEqual({
      id: "google",
      kind: "google",
      issuer: null,
      clientId: "g-client",
      clientSecret: "g-secret",
      enabled: true,
      scopes: ["openid", "email", "profile"],
      teamId: null,
      keyId: null,
    });
    form.oauth = [];
    expect(buildUpdate(stored, form)).toMatchObject({ update: { clear: ["oauth"] }, changed: ["oauth"] });
  });

  it("configures Sign in with Apple with its team id, key id and private key", () => {
    const form = formFromView(stored);
    const apple = newProvider("apple");
    expect(apple.id).toBe("apple");
    apple.clientId = "com.example.web";
    apple.secret.value = "not a pem";
    form.oauth.push(apple);
    expect(Object.keys(validate(form, stored)).sort()).toEqual(["oauth.1.keyId", "oauth.1.secret", "oauth.1.teamId"]);
    apple.teamId = " TEAMID1234 ";
    apple.keyId = "KEYID12345";
    apple.secret.value = "-----BEGIN PRIVATE KEY-----\nMIGT…\n-----END PRIVATE KEY-----";
    expect(validate(form, stored)).toEqual({});
    expect(buildUpdate(stored, form).update.oauth?.[1]).toMatchObject({
      kind: "apple",
      clientSecret: apple.secret.value,
      teamId: "TEAMID1234",
      keyId: "KEYID12345",
    });
    // A stored Apple provider round-trips without changes (the key stays masked).
    const withApple = view({
      oauth: [{ id: "apple", kind: "apple", issuer: null, clientId: "c", clientSecret: MASK, enabled: true, scopes: null, teamId: "T1", keyId: "K1", locked: false }],
    });
    expect(buildUpdate(withApple, formFromView(withApple))).toEqual({ update: {}, changed: [] });
  });

  it("never sends locked fields", () => {
    const locked = view({ ...stored, locked: ["signups", "sessionTtlSecs", "redirectUrls", "smtp"] });
    const form = formFromView(locked);
    form.signups = "off";
    form.sessionTtlSecs = "5"; // invalid, but locked: neither checked nor sent
    form.redirectUrls = "not a url";
    form.smtp.url = "";
    expect(buildUpdate(locked, form)).toEqual({ update: {}, changed: [] });
    expect(validate(form, locked)).toEqual({});
  });

  it("validates like the server", () => {
    const form = formFromView(view());
    form.accessTokenTtlSecs = "10";
    form.sessionTtlSecs = "1.5";
    form.redirectUrls = "https://ok.example.com/\nftp://nope";
    form.corsOrigins = "https://app.example.com\nhttps://app.example.com/path";
    form.smtp.url = "http://mail";
    form.smtp.from = "nobody";
    const p = newProvider("oidc");
    p.id = "Bad Id";
    form.oauth = [p, { ...newProvider("github"), clientId: "c", secret: { mode: "keep", value: "", stored: false } }];
    const errors = validate(form, view());
    expect(Object.keys(errors).sort()).toEqual([
      "accessTokenTtlSecs",
      "corsOrigins",
      "oauth.0.clientId",
      "oauth.0.id",
      "oauth.0.issuer",
      "oauth.0.secret",
      "oauth.1.secret",
      "redirectUrls",
      "sessionTtlSecs",
      "smtp.from",
      "smtp.url",
    ]);
    expect(errors.accessTokenTtlSecs).toBe("A whole number of seconds from 30 to 86400");
    expect(errors.redirectUrls).toContain("ftp://nope");
    // `*` and bare origins (a trailing slash is fine) pass.
    form.corsOrigins = "*\nhttps://app.example.com/\nhttp://localhost:5173";
    expect(validate(form, view()).corsOrigins).toBeUndefined();
  });

  it("edits the trusted proxy headers", () => {
    const form = formFromView(view());
    expect(form.proxyHeaders).toBe("");
    form.proxyHeaders = addHeader(addHeader(form.proxyHeaders, "CF-Connecting-IP"), "X-Forwarded-For");
    expect(addHeader(form.proxyHeaders, "cf-connecting-ip")).toBe(form.proxyHeaders);
    expect(buildUpdate(view(), form)).toEqual({
      update: { trustedProxy: { headers: ["CF-Connecting-IP", "X-Forwarded-For"], useLeftmostIp: false } },
      changed: ["trustedProxy"],
    });

    const saved = view({ trustedProxy: { headers: ["CF-Connecting-IP"], useLeftmostIp: false } });
    const edit = formFromView(saved);
    expect(edit.proxyHeaders).toBe("CF-Connecting-IP");
    expect(buildUpdate(saved, edit).changed).toEqual([]);
    edit.useLeftmostIp = true;
    expect(buildUpdate(saved, edit).update).toEqual({ trustedProxy: { headers: ["CF-Connecting-IP"], useLeftmostIp: true } });
    edit.proxyHeaders = " ";
    expect(buildUpdate(saved, edit)).toEqual({ update: { clear: ["trustedProxy"] }, changed: ["trustedProxy"] });

    edit.proxyHeaders = "X Forwarded For";
    expect(validate(edit, saved).trustedProxy).toContain("not a valid header name");

    // The proxy's addresses go with the headers; checked, and needing a header.
    const addr = formFromView(saved);
    addr.proxyAddresses = "10.0.0.0/8, 127.0.0.1";
    expect(validate(addr, saved)).toEqual({});
    expect(buildUpdate(saved, addr).update).toEqual({
      trustedProxy: { headers: ["CF-Connecting-IP"], useLeftmostIp: false, proxies: ["10.0.0.0/8", "127.0.0.1"] },
    });
    const withAddresses = view({ trustedProxy: { headers: ["CF-Connecting-IP"], useLeftmostIp: false, proxies: ["10.0.0.0/8"] } });
    expect(formFromView(withAddresses).proxyAddresses).toBe("10.0.0.0/8");
    expect(buildUpdate(withAddresses, formFromView(withAddresses)).changed).toEqual([]);
    addr.proxyAddresses = "proxy.internal";
    expect(validate(addr, saved).proxyAddresses).toContain("not an IP address or CIDR range");
    addr.proxyAddresses = "10.0.0.1";
    addr.proxyHeaders = "";
    expect(validate(addr, saved).proxyAddresses).toBe("Set the trusted headers first");
    // Fixed by the configuration: the stored addresses are kept, whatever the field says.
    const lockedAddresses = view({ ...withAddresses, locked: ["trustedProxy.proxies"] });
    const keep = formFromView(lockedAddresses);
    keep.proxyAddresses = "garbage";
    keep.useLeftmostIp = true;
    expect(validate(keep, lockedAddresses)).toEqual({});
    expect(buildUpdate(lockedAddresses, keep).update).toEqual({
      trustedProxy: { headers: ["CF-Connecting-IP"], useLeftmostIp: true, proxies: ["10.0.0.0/8"] },
    });
    // Fixed by the configuration: neither validated nor sent.
    const locked = view({ locked: ["trustedProxy"] });
    expect(validate(edit, locked)).toEqual({});
    expect(buildUpdate(locked, edit).changed).toEqual([]);
  });

  it("edits the rate limit rules", () => {
    const form = formFromView(view());
    expect(form.rateLimits).toEqual({ enabled: false, rules: [] });
    expect(buildUpdate(view(), form).changed).toEqual([]);

    form.rateLimits.enabled = true;
    form.rateLimits.rules = [newRateLimitRule(true), { ...newRateLimitRule(), label: " *:create ", maxRequests: "20", intervalSecs: "5", audience: "guest" }];
    expect(validate(form, view())).toEqual({});
    expect(buildUpdate(view(), form)).toEqual({
      update: {
        rateLimits: {
          enabled: true,
          rules: [
            { label: "/api/", maxRequests: 300, intervalSecs: 10, audience: "all" },
            { label: "*:create", maxRequests: 20, intervalSecs: 5, audience: "guest" },
          ],
        },
      },
      changed: ["rateLimits"],
    });

    // As the server sends it: keys sorted.
    const saved = view({ rateLimits: { enabled: true, rules: [{ audience: "all", intervalSecs: 10, label: "/api/", maxRequests: 300 }] } });
    const edit = formFromView(saved);
    expect(buildUpdate(saved, edit).changed).toEqual([]);
    // Turning it off keeps the rules; removing them all while off clears the setting.
    edit.rateLimits.enabled = false;
    expect(buildUpdate(saved, edit).update).toEqual({
      rateLimits: { enabled: false, rules: [{ label: "/api/", maxRequests: 300, intervalSecs: 10, audience: "all" }] },
    });
    edit.rateLimits.rules = [];
    expect(buildUpdate(saved, edit)).toEqual({ update: { clear: ["rateLimits"] }, changed: ["rateLimits"] });

    edit.rateLimits.rules = [
      { ...newRateLimitRule(), label: "/api/*", maxRequests: "0", intervalSecs: "86401" },
      { ...newRateLimitRule(), label: "*", maxRequests: "1", intervalSecs: "1" },
      { ...newRateLimitRule(), label: "*", maxRequests: "2", intervalSecs: "1" },
    ];
    const errors = validate(edit, saved);
    expect(errors["rateLimits.0.label"]).toContain("write /api/");
    expect(errors["rateLimits.0.maxRequests"]).toBeDefined();
    expect(errors["rateLimits.0.intervalSecs"]).toBeDefined();
    expect(errors["rateLimits.2.label"]).toContain("twice");
  });

  it("checks rate limit labels like the server", () => {
    for (const ok of ["/api/", "/", "*", "messages:send", "chat/messages:*", "*:create", "*:*"]) expect(rateLimitLabelError(ok)).toBeNull();
    for (const bad of ["", "messages", ":send", "a b:c", "_system/settings:update", "/a b", "/a?b", "m:se-nd"])
      expect(rateLimitLabelError(bad)).not.toBeNull();
  });

  it("edits the email templates", () => {
    const form = formFromView(view());
    expect(form.emailTemplates).toEqual({ appName: "", verifyEmail: null, resetPassword: null, signIn: null });
    expect(buildUpdate(view(), form).changed).toEqual([]);
    form.emailTemplates.resetPassword = { subject: " Reset your {app_name} password ", html: "<p>{code}</p>\n" };
    expect(buildUpdate(view(), form)).toEqual({
      update: {
        emailTemplates: {
          appName: null,
          verifyEmail: null,
          resetPassword: { subject: "Reset your {app_name} password", html: "<p>{code}</p>" },
          signIn: null,
        },
      },
      changed: ["emailTemplates"],
    });

    const saved = view({ emailTemplates: { appName: "Acme", verifyEmail: null, resetPassword: null, signIn: { subject: "Hi", html: "{link}" } } });
    const edit = formFromView(saved);
    expect(buildUpdate(saved, edit).changed).toEqual([]);
    edit.emailTemplates.signIn = null;
    edit.emailTemplates.appName = " ";
    expect(buildUpdate(saved, edit)).toEqual({ update: { clear: ["emailTemplates"] }, changed: ["emailTemplates"] });

    const errorsOf = (subject: string, html: string, appName = "") => {
      const f = formFromView(view());
      f.emailTemplates.appName = appName;
      f.emailTemplates.signIn = { subject, html };
      return validate(f, view());
    };
    expect(errorsOf("Hi", "<p>{code}</p>")).toEqual({});
    expect(errorsOf("", "")).toEqual({ "emailTemplates.signIn.subject": "Required", "emailTemplates.signIn.html": "Required" });
    expect(errorsOf("Hi", "<p>none</p>")["emailTemplates.signIn.html"]).toContain("{code} or {link}");
    expect(errorsOf("Hi {name}", "{code}")["emailTemplates.signIn.subject"]).toBe("Unknown placeholder {name}");
    expect(errorsOf("Two\nlines", "{code}")["emailTemplates.signIn.subject"]).toContain("one line");
    expect(errorsOf("Hi", "{code}", "x".repeat(101))["emailTemplates.appName"]).toContain("100 characters");
  });

  it("fills placeholders like the server", () => {
    const vars = { code: "1", app_name: "A & {code}" };
    expect(fillTemplate("{app_name}: {code} {nope} p { color: red }", vars, false)).toBe("A & {code}: 1 {nope} p { color: red }");
    expect(fillTemplate("<b>{app_name}</b>", vars, true)).toBe("<b>A &#38; {code}</b>");
    expect(fillTemplate("{constructor}", vars, false)).toBe("{constructor}");
    expect(unknownPlaceholder("{link} {code} {expires} {app_name} {email} { x } {Code}")).toBeNull();
    expect(unknownPlaceholder("{appname}")).toBe("appname");
    expect(previewTemplate({ subject: "{code} for {email}", html: "{expires}" }, "resetPassword", "Acme")).toEqual({
      subject: "123456 for ana@example.com",
      html: "30 minutes",
    });
  });

  it("maps server messages to the section they concern", () => {
    expect(sectionOfError("SMTP URL must look like smtp://user@host:587")).toBe("smtp");
    expect(sectionOfError("OAuth provider `x`: a client id is required")).toBe("oauth");
    expect(sectionOfError("redirect URL `x` must be an absolute http(s) URL")).toBe("redirectUrls");
    expect(sectionOfError("CORS origin `x` must be an origin like https://app.example.com")).toBe("corsOrigins");
    expect(sectionOfError("accessTokenTtlSecs must be between 30 and 86400 seconds")).toBe("accessTokenTtlSecs");
    expect(sectionOfError("sessionTtlSecs must be between 60 and 34560000 seconds")).toBe("sessionTtlSecs");
    expect(sectionOfError("trusted proxy header `a b` is not a valid header name")).toBe("trustedProxy");
    expect(sectionOfError("rate limit `/api/`: max requests must be between 1 and 1000000")).toBe("rateLimits");
    expect(sectionOfError("rate limit `x:y` is listed twice for the same users")).toBe("rateLimits");
    expect(sectionOfError("password reset email template: unknown placeholder `{x}`")).toBe("emailTemplates");
    expect(sectionOfError("something else")).toBe("general");
  });
});

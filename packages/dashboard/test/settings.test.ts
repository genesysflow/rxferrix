import { describe, expect, it } from "vitest";
import { MASK, type SettingsView } from "../src/lib/api.js";
import { buildUpdate, formFromView, newProvider, sectionOfError, validate } from "../src/lib/settings.js";

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

  it("maps server messages to the section they concern", () => {
    expect(sectionOfError("SMTP URL must look like smtp://user@host:587")).toBe("smtp");
    expect(sectionOfError("OAuth provider `x`: a client id is required")).toBe("oauth");
    expect(sectionOfError("redirect URL `x` must be an absolute http(s) URL")).toBe("redirectUrls");
    expect(sectionOfError("CORS origin `x` must be an origin like https://app.example.com")).toBe("corsOrigins");
    expect(sectionOfError("accessTokenTtlSecs must be between 30 and 86400 seconds")).toBe("accessTokenTtlSecs");
    expect(sectionOfError("sessionTtlSecs must be between 60 and 34560000 seconds")).toBe("sessionTtlSecs");
    expect(sectionOfError("something else")).toBe("general");
  });
});

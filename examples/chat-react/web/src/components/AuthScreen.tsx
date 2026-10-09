import { useEffect, useState, type FormEvent } from "react";
import { getAuth, useAuthActions, useFerrix } from "@ferrix/react";
// Not re-exported by @ferrix/react.
import type { AuthProviders } from "@ferrix/client";
import { errorMessage } from "../lib";

type Mode = "signin" | "signup" | "code";

export function AuthScreen({ initialError }: { initialError: string | null }) {
  const client = useFerrix();
  const auth = useAuthActions();
  const [mode, setMode] = useState<Mode>("signin");
  const [providers, setProviders] = useState<AuthProviders | null>(null);
  const [name, setName] = useState("");
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [code, setCode] = useState("");
  const [codeSent, setCodeSent] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(initialError);

  useEffect(() => setError(initialError), [initialError]);
  useEffect(() => {
    // `providers()` lives on FerrixAuth, not on useAuthActions().
    getAuth(client).providers().then(setProviders, () => setProviders(null));
  }, [client]);

  const run = async (f: () => Promise<unknown>) => {
    setBusy(true);
    setError(null);
    try {
      await f();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  const submit = (e: FormEvent) => {
    e.preventDefault();
    if (mode === "signin") return run(() => auth.signIn({ email, password }));
    if (mode === "signup") {
      return run(async () => {
        const { verificationRequired } = await auth.signUp({ email, password, name: name.trim() || undefined });
        if (verificationRequired) setError("Check your email to verify your address, then sign in.");
      });
    }
    if (!codeSent) {
      return run(async () => {
        await auth.requestMagicLink({ email, redirectTo: window.location.origin + "/" });
        setCodeSent(true);
      });
    }
    return run(() => auth.verifyCode({ email, code: code.trim() }));
  };

  const switchTo = (next: Mode) => {
    setMode(next);
    setError(null);
    setCodeSent(false);
  };

  return (
    <div className="auth">
      <div className="auth-card">
        <div className="auth-brand">
          <span className="logo">💬</span>
          <div>
            <h1>Ferrix Chat</h1>
            <p>A Slack-lite on a single Rust binary.</p>
          </div>
        </div>
        <div className="tabs" role="tablist">
          {(
            [
              ["signin", "Sign in"],
              ["signup", "Sign up"],
              ["code", "Email code"],
            ] as const
          ).map(([m, label]) => (
            <button key={m} role="tab" aria-selected={mode === m} className={mode === m ? "active" : ""} onClick={() => switchTo(m)}>
              {label}
            </button>
          ))}
        </div>
        <form onSubmit={submit} className="auth-form">
          {mode === "signup" && (
            <label>
              Name
              <input value={name} onChange={(e) => setName(e.target.value)} autoComplete="name" placeholder="Ada Lovelace" />
            </label>
          )}
          <label>
            Email
            <input
              type="email"
              required
              value={email}
              onChange={(e) => setEmail(e.target.value)}
              autoComplete="email"
              placeholder="you@example.com"
              disabled={mode === "code" && codeSent}
            />
          </label>
          {mode !== "code" && (
            <label>
              Password
              <input
                type="password"
                required
                minLength={8}
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                autoComplete={mode === "signup" ? "new-password" : "current-password"}
              />
            </label>
          )}
          {mode === "code" && codeSent && (
            <>
              <p className="hint">We emailed a 6-digit code and a sign-in link to {email}.</p>
              <label>
                Code
                <input
                  inputMode="numeric"
                  autoComplete="one-time-code"
                  required
                  value={code}
                  onChange={(e) => setCode(e.target.value)}
                  placeholder="123456"
                  autoFocus
                />
              </label>
            </>
          )}
          {error && (
            <p className="form-error" role="alert">
              {error}
            </p>
          )}
          <button className="btn primary" type="submit" disabled={busy}>
            {mode === "signin" ? "Sign in" : mode === "signup" ? "Create account" : codeSent ? "Verify code" : "Email me a code"}
          </button>
          {mode === "code" && codeSent && (
            <button type="button" className="link" onClick={() => setCodeSent(false)}>
              Use another email
            </button>
          )}
        </form>
        {providers?.oauth.includes("github") && (
          <>
            <div className="divider">or</div>
            <button className="btn github" onClick={() => auth.signInWithOAuth("github")}>
              Continue with GitHub
            </button>
          </>
        )}
      </div>
    </div>
  );
}

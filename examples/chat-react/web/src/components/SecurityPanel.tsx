import { useCallback, useEffect, useState, type FormEvent } from "react";
import { useAuthActions } from "@rxferrix/react";
import type { MfaSetup, MfaStatus } from "@rxferrix/client";
import { errorMessage } from "../lib";

/** Account security: turn two-factor authentication (an authenticator app) on and off, and manage recovery codes. */
export function SecurityPanel({ onClose }: { onClose: () => void }) {
  const auth = useAuthActions();
  const [status, setStatus] = useState<MfaStatus | null>(null);
  const [setup, setSetup] = useState<MfaSetup | null>(null);
  const [recoveryCodes, setRecoveryCodes] = useState<string[] | null>(null);
  const [password, setPassword] = useState("");
  const [code, setCode] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(() => auth.getMfaStatus().then(setStatus, (e) => setError(errorMessage(e))), [auth]);
  useEffect(() => {
    void refresh();
  }, [refresh]);

  const run = async (fn: () => Promise<void>) => {
    setBusy(true);
    setError(null);
    try {
      await fn();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  const start = (e: FormEvent) => {
    e.preventDefault();
    return run(async () => {
      setSetup(await auth.setupTotp(password ? { password } : {}));
      setPassword("");
    });
  };
  const confirm = (e: FormEvent) => {
    e.preventDefault();
    return run(async () => {
      setRecoveryCodes((await auth.confirmTotp(code.trim())).recoveryCodes);
      setSetup(null);
      setCode("");
      await refresh();
    });
  };
  const disable = (e: FormEvent) => {
    e.preventDefault();
    return run(async () => {
      await auth.disableMfa(code.trim());
      setCode("");
      await refresh();
    });
  };
  const regenerate = () =>
    run(async () => {
      setRecoveryCodes((await auth.regenerateRecoveryCodes(code.trim())).recoveryCodes);
      setCode("");
      await refresh();
    });

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal" role="dialog" aria-label="Account security" onClick={(e) => e.stopPropagation()}>
        <div className="modal-head">
          <h2>Two-factor authentication</h2>
          <button className="link" onClick={onClose}>
            Close
          </button>
        </div>

        {!status && !error && <p className="hint">Loading…</p>}

        {recoveryCodes ? (
          <div data-testid="recovery-codes">
            <p className="hint">Two-factor authentication is on. Keep these recovery codes somewhere safe; each works once in place of a code.</p>
            <pre className="codes">{recoveryCodes.join("\n")}</pre>
            <button className="btn primary" onClick={() => setRecoveryCodes(null)}>
              I saved them
            </button>
          </div>
        ) : setup ? (
          <form onSubmit={confirm} className="auth-form">
            <p className="hint">Scan the QR code with your authenticator app, or enter the secret by hand, then type its first code.</p>
            <div className="qr" dangerouslySetInnerHTML={{ __html: setup.qrSvg }} />
            <code className="secret">{setup.secret}</code>
            <label>
              Code
              <input autoComplete="one-time-code" required value={code} onChange={(e) => setCode(e.target.value)} placeholder="123456" autoFocus />
            </label>
            <button className="btn primary" type="submit" disabled={busy}>
              Turn on
            </button>
            <button type="button" className="link" onClick={() => void run(async () => (await auth.disableMfa(), setSetup(null), refresh()))}>
              Cancel
            </button>
          </form>
        ) : status?.enabled ? (
          <form onSubmit={disable} className="auth-form">
            <p className="hint">
              On. {status.recoveryCodesLeft} recovery code{status.recoveryCodesLeft === 1 ? "" : "s"} left. Enter a current code to turn it off or to
              replace the recovery codes.
            </p>
            <label>
              Code
              <input autoComplete="one-time-code" required value={code} onChange={(e) => setCode(e.target.value)} placeholder="123456 or xxxxx-xxxxx" />
            </label>
            <button className="btn primary" type="submit" disabled={busy}>
              Turn off
            </button>
            <button type="button" className="btn" disabled={busy || !code.trim()} onClick={() => void regenerate()}>
              New recovery codes
            </button>
          </form>
        ) : status ? (
          <form onSubmit={start} className="auth-form">
            <p className="hint">Off. Add an authenticator app (Google Authenticator, 1Password, Aegis…) so signing in also needs a code from it.</p>
            <label>
              Password <span className="hint">(leave empty if you sign in with email codes)</span>
              <input type="password" autoComplete="current-password" value={password} onChange={(e) => setPassword(e.target.value)} />
            </label>
            <button className="btn primary" type="submit" disabled={busy}>
              Set up
            </button>
          </form>
        ) : null}

        {error && (
          <p className="form-error" role="alert">
            {error}
          </p>
        )}
      </div>
    </div>
  );
}

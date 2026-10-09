import { errorCode, errorMessage } from "../lib";

const TITLES: Record<string, string> = {
  not_a_member: "Private channel",
  not_found: "Channel not found",
  unauthenticated: "Signed out",
};

/** A failed channel (a `UserError` from the server), e.g. `not_a_member` for someone else's private channel. */
export function AccessError({ error, onBack }: { error: unknown; onBack: () => void }) {
  const code = errorCode(error);
  return (
    <div className="access-error" role="alert" data-code={code}>
      <div className="access-icon">{code === "not_a_member" ? "🔒" : "⚠️"}</div>
      <h2>{(code && TITLES[code]) ?? "Something went wrong"}</h2>
      <p>{errorMessage(error)}</p>
      {code && <code className="error-code">{code}</code>}
      <button className="btn" onClick={onBack}>
        Back to your channels
      </button>
    </div>
  );
}

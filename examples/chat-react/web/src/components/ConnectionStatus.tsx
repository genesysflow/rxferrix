import { useConnectionState } from "@rxferrix/react";

/** A dot and a word: live, connecting, or reconnecting (with pending writes). */
export function ConnectionStatus() {
  const state = useConnectionState();
  const status = state.isWebSocketConnected ? "live" : state.hasEverConnected ? "reconnecting" : "connecting";
  const label = { live: "Live", connecting: "Connecting…", reconnecting: "Reconnecting…" }[status];
  const pending = state.inflightMutations + state.inflightActions;
  return (
    <span
      className={`conn ${status}`}
      data-testid="connection"
      data-status={status}
      title={pending ? `${pending} change(s) waiting to be saved` : undefined}
    >
      <span className="dot" />
      {label}
      {status !== "live" && pending > 0 && <span className="pending">· {pending} pending</span>}
    </span>
  );
}

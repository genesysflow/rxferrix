import { useEffect, useState } from "react";
import { useAuthActions, useRxFerrixAuth } from "@rxferrix/react";
import { AuthScreen } from "./components/AuthScreen";
import { Chat } from "./components/Chat";
import { ErrorBoundary } from "./components/ErrorBoundary";
import { errorMessage } from "./lib";

export function App() {
  const { isLoading, isAuthenticated } = useRxFerrixAuth();
  const { handleRedirectCallback } = useAuthActions();
  const [redirectError, setRedirectError] = useState<string | null>(null);

  // Magic links and OAuth return here with `#rxferrix_…` in the URL.
  useEffect(() => {
    void handleRedirectCallback().then((result) => {
      if (result?.type === "error") setRedirectError(errorMessage(result.error));
    });
  }, [handleRedirectCallback]);

  if (isLoading) {
    return (
      <div className="splash">
        <div className="logo">💬</div>
        <p>Connecting…</p>
      </div>
    );
  }
  if (!isAuthenticated) return <AuthScreen initialError={redirectError} />;
  return (
    <ErrorBoundary
      fallback={(error, reset) => (
        <div className="splash">
          <p role="alert">{errorMessage(error)}</p>
          <button className="btn" onClick={reset}>
            Try again
          </button>
        </div>
      )}
    >
      <Chat />
    </ErrorBoundary>
  );
}

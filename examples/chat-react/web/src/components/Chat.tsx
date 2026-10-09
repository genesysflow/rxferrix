import { useEffect, useState } from "react";
import { useMutation, useQuery } from "@ferrix/react";
import { api } from "../ferrix";
import { errorMessage, useChannelRoute } from "../lib";
import { ChannelView } from "./ChannelView";
import { ErrorBoundary } from "./ErrorBoundary";
import { AccessError } from "./AccessError";
import { Sidebar } from "./Sidebar";

/** The signed-in app: sidebar plus the open channel. */
export function Chat() {
  const ensure = useMutation(api.profiles.ensure);
  const [setupError, setSetupError] = useState<string | null>(null);
  // Creates the profile on first sign-in (and joins #general); a no-op afterwards.
  useEffect(() => {
    ensure().catch((e) => setSetupError(errorMessage(e)));
  }, [ensure]);

  const me = useQuery(api.profiles.me);
  const channels = useQuery(api.channels.list);
  const [channelId, navigate] = useChannelRoute();

  // Nothing open yet: go to #general (or the first joined channel).
  useEffect(() => {
    if (channelId || !channels) return;
    const first = channels.find((c) => c.isMember && c.name === "general") ?? channels.find((c) => c.isMember);
    if (first) navigate(first._id, true);
  }, [channelId, channels, navigate]);

  if (setupError) {
    return (
      <div className="splash">
        <p role="alert">{setupError}</p>
      </div>
    );
  }
  if (!me || !channels) {
    return (
      <div className="splash">
        <div className="logo">💬</div>
        <p>Setting things up…</p>
      </div>
    );
  }

  return (
    <div className="app">
      <Sidebar me={me} channels={channels} activeId={channelId} onSelect={navigate} />
      <main className="main">
        {channelId ? (
          <ErrorBoundary key={channelId} fallback={(error) => <AccessError error={error} onBack={() => navigate(null)} />}>
            <ChannelView channelId={channelId} me={me} onLeave={() => navigate(null)} />
          </ErrorBoundary>
        ) : (
          <div className="empty-main">Pick a channel on the left, or create one.</div>
        )}
      </main>
    </div>
  );
}

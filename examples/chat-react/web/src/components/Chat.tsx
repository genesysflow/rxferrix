import { useEffect, useState } from "react";
import { useMutation, useQuery } from "@rxferrix/react";
import { api } from "../rxferrix";
import { errorMessage, useChannelRoute } from "../lib";
import { ChannelView } from "./ChannelView";
import { ErrorBoundary } from "./ErrorBoundary";
import { AccessError } from "./AccessError";
import { InsightsView } from "./InsightsView";
import { Sidebar } from "./Sidebar";

/** The `CHAT_ANNOUNCEMENT` environment variable, live (`announcement:get`): edit it on the dashboard's Environment page. */
function Announcement() {
  const text = useQuery(api.announcement.get);
  if (!text) return null;
  return (
    <div className="announcement" role="status" data-testid="announcement">
      <span aria-hidden="true">📣</span> {text}
    </div>
  );
}

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
  const insights = useInsightsRoute();

  // Nothing open yet: go to #general (or the first joined channel). The hash is read again because the two route
  // hooks hear `hashchange` separately: opening Insights can render once with neither set.
  useEffect(() => {
    if (channelId || insights || isInsightsHash() || !channels) return;
    const first = channels.find((c) => c.isMember && c.name === "general") ?? channels.find((c) => c.isMember);
    if (first) navigate(first._id, true);
  }, [channelId, insights, channels, navigate]);

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
      <Sidebar
        me={me}
        channels={channels}
        activeId={insights ? null : channelId}
        insights={insights}
        onSelect={navigate}
        onInsights={() => (window.location.hash = "#/insights")}
      />
      <main className="main">
        <Announcement />
        {insights ? (
          <InsightsView channels={channels} />
        ) : channelId ? (
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

/** Whether the URL is `#/insights`. */
function useInsightsRoute(): boolean {
  const [on, setOn] = useState(isInsightsHash);
  useEffect(() => {
    const onChange = () => setOn(isInsightsHash());
    window.addEventListener("hashchange", onChange);
    return () => window.removeEventListener("hashchange", onChange);
  }, []);
  return on;
}

function isInsightsHash(): boolean {
  return window.location.hash.startsWith("#/insights");
}

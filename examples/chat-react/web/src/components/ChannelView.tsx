import { useState, type FormEvent } from "react";
import { useMutation, useQueryWithStatus } from "@ferrix/react";
import { api, type Id } from "../ferrix";
import { errorMessage, initials, type ChannelInfo, type Profile } from "../lib";
import { AccessError } from "./AccessError";
import { Composer } from "./Composer";
import { ErrorBoundary } from "./ErrorBoundary";
import { MessageList } from "./MessageList";
import { SearchPanel } from "./SearchPanel";
import { TypingIndicator } from "./TypingIndicator";

interface Props {
  channelId: Id<"channels">;
  me: Profile;
  onLeave: () => void;
}

export function ChannelView({ channelId, me, onLeave }: Props) {
  // useQueryWithStatus instead of useQuery: a private channel we're not in is an expected `not_a_member` error.
  const info = useQueryWithStatus(api.channels.get, { channel: channelId });
  const [panel, setPanel] = useState<"none" | "search" | "members">("none");

  if (info.error) return <AccessError error={info.error} onBack={onLeave} />;
  if (!info.data) return <div className="loading-channel">Loading…</div>;
  const channel = info.data;

  return (
    <section className="channel">
      <header className="channel-head">
        <div className="channel-title">
          <h2>
            <span className="hash">{channel.private ? "🔒" : "#"}</span>
            {channel.name}
          </h2>
          {channel.topic && <p className="topic">{channel.topic}</p>}
        </div>
        <div className="head-actions">
          <button
            className={`chip${panel === "members" ? " on" : ""}`}
            onClick={() => setPanel(panel === "members" ? "none" : "members")}
            aria-label="Members"
          >
            <span className="stack">
              {channel.members.slice(0, 3).map((m) => (
                <span key={m.user} className="avatar tiny" style={{ background: m.color }}>
                  {initials(m.name)}
                </span>
              ))}
            </span>
            {channel.members.length}
          </button>
          <button
            className={`chip${panel === "search" ? " on" : ""}`}
            onClick={() => setPanel(panel === "search" ? "none" : "search")}
            aria-label="Search messages"
          >
            🔍 Search
          </button>
        </div>
      </header>

      <div className="channel-body">
        <div className="timeline">
          <ErrorBoundary key={channelId} fallback={(error) => <AccessError error={error} onBack={onLeave} />}>
            <MessageList channel={channel} me={me} />
          </ErrorBoundary>
          {channel.isMember ? (
            <>
              <TypingIndicator channel={channelId} />
              <Composer channel={channel} me={me} />
            </>
          ) : (
            <JoinBar channel={channel} />
          )}
        </div>
        {panel === "search" && <SearchPanel channel={channel} onClose={() => setPanel("none")} />}
        {panel === "members" && <MembersPanel channel={channel} onClose={() => setPanel("none")} onLeave={onLeave} />}
      </div>
    </section>
  );
}

function JoinBar({ channel }: { channel: ChannelInfo }) {
  const join = useMutation(api.channels.join);
  const [error, setError] = useState<string | null>(null);
  return (
    <div className="join-bar">
      <p>
        You're previewing <strong>#{channel.name}</strong>.
      </p>
      <button className="btn primary" onClick={() => join({ channel: channel._id }).catch((e) => setError(errorMessage(e)))}>
        Join #{channel.name}
      </button>
      {error && (
        <p className="form-error" role="alert">
          {error}
        </p>
      )}
    </div>
  );
}

function MembersPanel({ channel, onClose, onLeave }: { channel: ChannelInfo; onClose: () => void; onLeave: () => void }) {
  const invite = useMutation(api.channels.invite);
  const leave = useMutation(api.channels.leave);
  const [email, setEmail] = useState("");
  const [status, setStatus] = useState<{ ok: boolean; text: string } | null>(null);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    try {
      await invite({ channel: channel._id, email });
      setStatus({ ok: true, text: `Added ${email}` });
      setEmail("");
    } catch (err) {
      setStatus({ ok: false, text: errorMessage(err) });
    }
  };

  return (
    <aside className="side-panel" aria-label="Members of the channel">
      <div className="panel-head">
        <h3>Members</h3>
        <button className="icon-btn dark" onClick={onClose} aria-label="Close members">
          ✕
        </button>
      </div>
      <ul className="member-list">
        {channel.members.map((m) => (
          <li key={m.user}>
            <span className="avatar small" style={{ background: m.color }}>
              {initials(m.name)}
            </span>
            {m.name}
          </li>
        ))}
      </ul>
      {channel.isMember && (
        <>
          <form className="invite" onSubmit={submit}>
            <label>
              Invite by email
              <input type="email" required value={email} onChange={(e) => setEmail(e.target.value)} placeholder="teammate@example.com" />
            </label>
            <button className="btn small" type="submit">
              Invite
            </button>
          </form>
          {status && (
            <p className={status.ok ? "form-ok" : "form-error"} role={status.ok ? "status" : "alert"}>
              {status.text}
            </p>
          )}
          <button
            className="link danger"
            onClick={() => {
              void leave({ channel: channel._id }).then(onLeave);
            }}
          >
            Leave #{channel.name}
          </button>
        </>
      )}
    </aside>
  );
}

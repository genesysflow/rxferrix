import { useMemo, useState, type FormEvent } from "react";
import { useAuthActions, useMutation, useQuery } from "@rxferrix/react";
import { api, type Id } from "../rxferrix";
import { errorMessage, initials, type ChannelSummary, type Profile } from "../lib";
import { ConnectionStatus } from "./ConnectionStatus";
import { SecurityPanel } from "./SecurityPanel";

interface Props {
  me: Profile;
  channels: ChannelSummary[];
  activeId: Id<"channels"> | null;
  /** The Insights view is open. */
  insights: boolean;
  onSelect: (id: Id<"channels">) => void;
  onInsights: () => void;
}

/** "1.2k" for counts past a thousand. */
function compact(n: number): string {
  return n < 1000 ? String(n) : `${(n / 1000).toFixed(n < 10_000 ? 1 : 0)}k`;
}

export function Sidebar({ me, channels, activeId, insights, onSelect, onInsights }: Props) {
  const unread = useQuery(api.channels.unread);
  const counts = useMemo(() => new Map((unread ?? []).map((u) => [u.channel, u.count])), [unread]);
  const joined = channels.filter((c) => c.isMember);
  const browse = channels.filter((c) => !c.isMember);
  const { signOut } = useAuthActions();
  const [creating, setCreating] = useState(false);
  const [security, setSecurity] = useState(false);

  return (
    <aside className="sidebar">
      <div className="sidebar-head">
        <span className="workspace">💬 RxFerrix Chat</span>
        <ConnectionStatus />
      </div>

      <nav className="sidebar-scroll" aria-label="Channels">
        <button className={`channel-link insights-link${insights ? " active" : ""}`} onClick={onInsights} data-testid="open-insights">
          <span className="hash">📊</span>
          <span className="name">Insights</span>
        </button>
        <div className="section-title">
          <span>Channels</span>
          <button className="icon-btn" aria-label="New channel" title="New channel" onClick={() => setCreating((v) => !v)}>
            +
          </button>
        </div>
        {creating && (
          <NewChannel
            onCreated={(id) => {
              setCreating(false);
              onSelect(id);
            }}
          />
        )}
        <ul className="channel-list">
          {joined.map((c) => {
            // The open channel is marked read as messages arrive, so don't flash a badge for it.
            const count = c._id === activeId ? 0 : (counts.get(c._id) ?? 0);
            return (
              <li key={c._id}>
                <button
                  className={`channel-link${c._id === activeId ? " active" : ""}${count ? " unread" : ""}`}
                  onClick={() => onSelect(c._id)}
                  data-channel={c.name}
                >
                  <span className="hash">{c.private ? "🔒" : "#"}</span>
                  <span className="name">{c.name}</span>
                  {count === 0 && c.messageCount > 0 && (
                    <span className="msg-count" data-testid={`count-${c.name}`} title={`${c.messageCount} messages`}>
                      {compact(c.messageCount)}
                    </span>
                  )}
                  {count > 0 && (
                    <span className="badge" data-testid={`unread-${c.name}`} aria-label={`${count} unread`}>
                      {count >= 100 ? "99+" : count}
                    </span>
                  )}
                </button>
              </li>
            );
          })}
        </ul>

        {browse.length > 0 && (
          <>
            <div className="section-title">
              <span>Browse</span>
            </div>
            <ul className="channel-list browse">
              {browse.map((c) => (
                <li key={c._id}>
                  <button
                    className={`channel-link${c._id === activeId ? " active" : ""}`}
                    onClick={() => onSelect(c._id)}
                    data-channel={c.name}
                  >
                    <span className="hash">#</span>
                    <span className="name">{c.name}</span>
                  </button>
                </li>
              ))}
            </ul>
          </>
        )}
      </nav>

      <div className="me">
        <span className="avatar" style={{ background: me.color }}>
          {initials(me.name)}
        </span>
        <span className="me-name" data-testid="me">
          {me.name}
        </span>
        <button className="link light" onClick={() => setSecurity(true)} data-testid="open-security">
          Security
        </button>
        <button className="link light" onClick={() => void signOut()}>
          Sign out
        </button>
        {security && <SecurityPanel onClose={() => setSecurity(false)} />}
      </div>
    </aside>
  );
}

function NewChannel({ onCreated }: { onCreated: (id: Id<"channels">) => void }) {
  const create = useMutation(api.channels.create);
  const [name, setName] = useState("");
  const [topic, setTopic] = useState("");
  const [isPrivate, setPrivate] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setError(null);
    try {
      onCreated(await create({ name, topic: topic || null, private: isPrivate }));
    } catch (err) {
      setError(errorMessage(err));
    }
  };

  return (
    <form className="new-channel" onSubmit={submit}>
      <input aria-label="Channel name" placeholder="channel-name" value={name} onChange={(e) => setName(e.target.value)} autoFocus />
      <input aria-label="Topic" placeholder="What's it about? (optional)" value={topic} onChange={(e) => setTopic(e.target.value)} />
      <label className="check">
        <input type="checkbox" checked={isPrivate} onChange={(e) => setPrivate(e.target.checked)} /> Private
      </label>
      {error && (
        <p className="form-error small" role="alert">
          {error}
        </p>
      )}
      <button className="btn primary small" type="submit">
        Create
      </button>
    </form>
  );
}

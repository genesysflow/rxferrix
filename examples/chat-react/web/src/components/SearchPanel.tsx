import { useDeferredValue, useState, type ReactNode } from "react";
import { useQueryWithStatus } from "@rxferrix/react";
import { api } from "../rxferrix";
import { errorMessage, formatTime, initials, type ChannelInfo } from "../lib";

/** Full-text search in the channel (live: new matching messages show up as they're sent). */
export function SearchPanel({ channel, onClose }: { channel: ChannelInfo; onClose: () => void }) {
  const [text, setText] = useState("");
  const query = useDeferredValue(text.trim());
  const results = useQueryWithStatus(api.messages.search, query ? { channel: channel._id, text: query } : "skip");

  return (
    <aside className="side-panel search" aria-label="Search results">
      <div className="panel-head">
        <h3>Search #{channel.name}</h3>
        <button className="icon-btn dark" onClick={onClose} aria-label="Close search">
          ✕
        </button>
      </div>
      <input
        className="search-input"
        type="search"
        aria-label="Search this channel"
        placeholder="Search messages…"
        value={text}
        onChange={(e) => setText(e.target.value)}
        autoFocus
      />
      {results.error && (
        <p className="form-error" role="alert">
          {errorMessage(results.error)}
        </p>
      )}
      {query && results.data?.length === 0 && <p className="muted center">No messages match “{query}”.</p>}
      <ul className="search-results">
        {results.data?.map((m) => (
          <li key={m._id} data-testid="search-result">
            <div className="meta">
              <span className="avatar tiny" style={{ background: m.authorColor }}>
                {initials(m.authorName)}
              </span>
              <span className="author">{m.authorName}</span>
              <time>{formatTime(m._creationTime)}</time>
            </div>
            <p>{highlight(m.body, query)}</p>
          </li>
        ))}
      </ul>
    </aside>
  );
}

/** Marks the words of `query` in `text` (prefix matches, like the server's search). */
function highlight(text: string, query: string): ReactNode {
  const words = query
    .split(/[^\p{L}\p{N}]+/u)
    .filter(Boolean)
    .map((w) => w.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"));
  if (words.length === 0) return text;
  const re = new RegExp(`(${words.join("|")})`, "giu");
  return text.split(re).map((part, i) => (i % 2 === 1 ? <mark key={i}>{part}</mark> : part));
}

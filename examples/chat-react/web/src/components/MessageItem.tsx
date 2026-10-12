import { memo, useState, type KeyboardEvent } from "react";
import { useMutation, type ReactMutation } from "@rxferrix/react";
import { api, type MessageView } from "../rxferrix";
import { errorMessage, formatTime, initials, thumbUrl } from "../lib";

export const EMOJIS = ["👍", "❤️", "😂", "🎉", "👀", "🚀"];

interface Props {
  message: MessageView;
  /** Same author as the message above: no avatar and name. */
  grouped: boolean;
  mine: boolean;
  canInteract: boolean;
  onReact: ReactMutation<typeof api.reactions.toggle>;
}

export const MessageItem = memo(function MessageItem({ message: m, grouped, mine, canInteract, onReact }: Props) {
  const edit = useMutation(api.messages.edit);
  const remove = useMutation(api.messages.remove);
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(m.body);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [picker, setPicker] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const pending = m._id.startsWith("optimistic:");
  const reminder = m.kind === "reminder";

  const react = (emoji: string) => {
    setPicker(false);
    onReact({ message: m._id, emoji }).catch((e) => setError(errorMessage(e)));
  };
  const save = async () => {
    try {
      await edit({ message: m._id, body: draft });
      setEditing(false);
      setError(null);
    } catch (e) {
      setError(errorMessage(e));
    }
  };
  const onKey = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      void save();
    } else if (e.key === "Escape") {
      setEditing(false);
      setDraft(m.body);
    }
  };

  return (
    <article
      className={`message${grouped ? " grouped" : ""}${pending ? " pending" : ""}${reminder ? " reminder" : ""}`}
      data-testid="message"
      data-id={m._id}
      aria-label={`${m.authorName}: ${m.body}`}
    >
      <div className="gutter">
        {grouped ? (
          <time className="hover-time">{formatTime(m._creationTime).split(", ").pop()}</time>
        ) : (
          <span className="avatar" style={{ background: m.authorColor }}>
            {reminder ? "⏰" : initials(m.authorName)}
          </span>
        )}
      </div>
      <div className="content">
        {!grouped && (
          <div className="meta">
            <span className="author">{reminder ? `Reminder for ${m.authorName}` : m.authorName}</span>
            <time dateTime={new Date(m._creationTime).toISOString()}>{formatTime(m._creationTime)}</time>
            {pending && <span className="sending">sending…</span>}
          </div>
        )}
        {editing ? (
          <div className="edit-box">
            <textarea aria-label="Edit message" value={draft} onChange={(e) => setDraft(e.target.value)} onKeyDown={onKey} autoFocus rows={2} />
            <div className="edit-actions">
              <span className="muted">Enter to save · Esc to cancel</span>
              <button className="btn small" onClick={() => setEditing(false)}>
                Cancel
              </button>
              <button className="btn primary small" onClick={() => void save()}>
                Save
              </button>
            </div>
          </div>
        ) : (
          m.body && (
            <p className="body">
              {m.body}
              {m.edited && <span className="edited"> (edited)</span>}
            </p>
          )
        )}
        {m.file && <AttachmentImage attachment={m.file} />}
        {m.reactions.length > 0 && (
          <div className="reactions">
            {m.reactions.map((g) => (
              <button
                key={g.emoji}
                className={`reaction${g.mine ? " mine" : ""}`}
                aria-pressed={g.mine}
                aria-label={`${g.emoji} ${g.count}`}
                title={g.names.join(", ")}
                disabled={!canInteract || pending}
                onClick={() => react(g.emoji)}
              >
                {g.emoji} <span className="count">{g.count}</span>
              </button>
            ))}
          </div>
        )}
        {error && (
          <p className="form-error small" role="alert">
            {error}
          </p>
        )}
      </div>
      {canInteract && !pending && !editing && (
        <div className={`toolbar${picker || confirmDelete ? " open" : ""}`}>
          {picker ? (
            EMOJIS.map((e) => (
              <button key={e} className="tool" aria-label={`React with ${e}`} onClick={() => react(e)}>
                {e}
              </button>
            ))
          ) : confirmDelete ? (
            <>
              <button
                className="tool danger"
                onClick={() => remove({ message: m._id }).catch((e) => setError(errorMessage(e)))}
              >
                Delete for everyone
              </button>
              <button className="tool" onClick={() => setConfirmDelete(false)}>
                Keep
              </button>
            </>
          ) : (
            <>
              <button className="tool" aria-label="Add reaction" title="Add reaction" onClick={() => setPicker(true)}>
                ☺︎+
              </button>
              {mine && !reminder && (
                <button
                  className="tool"
                  onClick={() => {
                    setDraft(m.body);
                    setEditing(true);
                  }}
                >
                  Edit
                </button>
              )}
              {mine && (
                <button className="tool" onClick={() => setConfirmDelete(true)}>
                  Delete
                </button>
              )}
            </>
          )}
          {(picker || confirmDelete) && (
            <button
              className="tool"
              aria-label="Close"
              onClick={() => {
                setPicker(false);
                setConfirmDelete(false);
              }}
            >
              ✕
            </button>
          )}
        </div>
      )}
    </article>
  );
});

/** An image attachment: asks for a thumbnail (`?thumb=`) and falls back to the original if that fails. */
function AttachmentImage({ attachment }: { attachment: NonNullable<MessageView["file"]> }) {
  const [fallback, setFallback] = useState(false);
  const isImage = attachment.contentType?.startsWith("image/") ?? false;
  if (!isImage) {
    return (
      <a className="file" href={attachment.url} target="_blank" rel="noreferrer">
        📎 {attachment.filename ?? "attachment"}
      </a>
    );
  }
  // blob: URLs (the optimistic preview) take no parameters.
  const src = fallback || attachment.url.startsWith("blob:") ? attachment.url : thumbUrl(attachment.url, 480);
  return (
    <a className="attachment" href={attachment.url} target="_blank" rel="noreferrer">
      <img src={src} alt={attachment.filename ?? "image"} loading="lazy" onError={() => !fallback && setFallback(true)} />
    </a>
  );
}

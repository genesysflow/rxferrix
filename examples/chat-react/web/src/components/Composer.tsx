import { useEffect, useRef, useState, type FormEvent, type KeyboardEvent } from "react";
import { insertAtTop, uploadFile, useAction, useMutation } from "@ferrix/react";
import { api, type Id } from "../ferrix";
import { errorMessage, type ChannelInfo, type Profile } from "../lib";

/** An image being uploaded (or uploaded) for the next message. */
interface Upload {
  file: File;
  preview: string;
  progress: number;
  storageId: Id<"_storage"> | null;
  error: string | null;
}

/** Report typing at most this often (the server keeps it for 4 s). */
const TYPING_EVERY_MS = 2000;

export function Composer({ channel, me }: { channel: ChannelInfo; me: Profile }) {
  const [text, setText] = useState("");
  const [upload, setUpload] = useState<Upload | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const send = useMutation(api.messages.send);
  const generateUploadUrl = useMutation(api.messages.generate_upload_url);
  const seed = useAction(api.messages.seed);
  const startTyping = useMutation(api.typing.start);
  const stopTyping = useMutation(api.typing.stop);
  const lastTyping = useRef(0);
  const fileInput = useRef<HTMLInputElement>(null);
  const textarea = useRef<HTMLTextAreaElement>(null);

  useEffect(() => {
    if (!notice) return;
    const t = setTimeout(() => setNotice(null), 8000);
    return () => clearTimeout(t);
  }, [notice]);

  const onChange = (value: string) => {
    setText(value);
    const typing = value.trim() !== "" && !value.startsWith("/");
    if (typing && Date.now() - lastTyping.current > TYPING_EVERY_MS) {
      lastTyping.current = Date.now();
      startTyping({ channel: channel._id }).catch(() => {});
    } else if (!typing && lastTyping.current) {
      lastTyping.current = 0;
      stopTyping({ channel: channel._id }).catch(() => {});
    }
  };

  const attach = async (file: File) => {
    setError(null);
    if (!file.type.startsWith("image/")) {
      setError("Only images can be attached.");
      return;
    }
    const preview = URL.createObjectURL(file);
    const update = (patch: Partial<Upload>) => setUpload((u) => (u && u.file === file ? { ...u, ...patch } : u));
    setUpload({ file, preview, progress: 0, storageId: null, error: null });
    try {
      const url = await generateUploadUrl();
      const storageId = await uploadFile(url, file, {
        onProgress: ({ loaded, total }) => update({ progress: total ? loaded / total : 0 }),
      });
      update({ progress: 1, storageId });
    } catch (e) {
      update({ error: errorMessage(e) });
    }
  };

  const submit = async (e?: FormEvent) => {
    e?.preventDefault();
    const body = text.trim();
    setError(null);

    const seedCommand = /^\/seed(?:\s+(\d+))?$/.exec(body);
    if (seedCommand) {
      setText("");
      try {
        const n = await seed({ channel: channel._id, count: Number(seedCommand[1] ?? 200) });
        setNotice(`Added ${n} sample messages to #${channel.name}. Scroll up!`);
      } catch (err) {
        setError(errorMessage(err));
      }
      return;
    }
    if (upload && !upload.storageId) return; // still uploading (or failed)
    if (!body && !upload) return;

    const attachment = upload?.storageId ?? null;
    const preview = upload?.preview;
    const file = upload?.file;
    // Created once, outside the update: the update is replayed on every server change until the send completes.
    const optimisticId = `optimistic:${crypto.randomUUID()}` as Id<"messages">;
    const creationTime = Date.now();
    const mutate = body.startsWith("/")
      ? send // slash commands post nothing (or post later)
      : send.withOptimisticUpdate((store, args) => {
          insertAtTop({
            paginatedQuery: api.messages.list,
            argsToMatch: { channel: args.channel },
            localQueryStore: store,
            item: {
              _id: optimisticId,
              channel: args.channel,
              author: me.user,
              authorName: me.name,
              authorColor: me.color,
              body: args.body.trim(),
              kind: "message",
              edited: false,
              _creationTime: creationTime,
              attachment: args.attachment ?? null,
              file:
                preview && file
                  ? { url: preview, contentType: file.type, filename: file.name, size: file.size }
                  : null,
              reactions: [],
            },
          });
        });

    setText("");
    setUpload(null);
    lastTyping.current = 0;
    textarea.current?.focus();
    try {
      const result = await mutate({ channel: channel._id, body, attachment });
      if (result.notice) setNotice(result.notice);
    } catch (err) {
      setError(errorMessage(err));
      setText((current) => current || body);
    } finally {
      if (preview) URL.revokeObjectURL(preview);
    }
  };

  const onKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
      e.preventDefault();
      void submit();
    }
  };

  const uploading = upload !== null && upload.storageId === null && upload.error === null;

  return (
    <form className="composer" onSubmit={submit}>
      {notice && (
        <div className="notice" role="status" data-testid="notice">
          <span>🤖 {notice}</span>
          <button type="button" className="icon-btn dark" aria-label="Dismiss" onClick={() => setNotice(null)}>
            ✕
          </button>
        </div>
      )}
      {upload && (
        <div className="upload">
          <img src={upload.preview} alt="" />
          <div className="upload-info">
            <span className="filename">{upload.file.name}</span>
            {upload.error ? (
              <span className="form-error small">{upload.error}</span>
            ) : (
              <progress aria-label="Upload progress" value={upload.progress} max={1} />
            )}
          </div>
          <button
            type="button"
            className="icon-btn dark"
            aria-label="Remove attachment"
            onClick={() => {
              URL.revokeObjectURL(upload.preview);
              setUpload(null);
            }}
          >
            ✕
          </button>
        </div>
      )}
      <div className="composer-box">
        <button type="button" className="attach" aria-label="Attach an image" title="Attach an image" onClick={() => fileInput.current?.click()}>
          📎
        </button>
        <input
          ref={fileInput}
          type="file"
          accept="image/*"
          hidden
          data-testid="file-input"
          onChange={(e) => {
            const file = e.target.files?.[0];
            e.target.value = "";
            if (file) void attach(file);
          }}
        />
        <textarea
          ref={textarea}
          aria-label="Message"
          placeholder={`Message #${channel.name}   (try /remind me in 10 seconds to stretch)`}
          value={text}
          rows={1}
          onChange={(e) => onChange(e.target.value)}
          onKeyDown={onKeyDown}
          maxLength={4000}
        />
        <button className="btn primary send" type="submit" disabled={uploading || (!text.trim() && !upload?.storageId)}>
          {uploading ? `${Math.round((upload?.progress ?? 0) * 100)}%` : "Send"}
        </button>
      </div>
      {error && (
        <p className="form-error small" role="alert">
          {error}
        </p>
      )}
    </form>
  );
}

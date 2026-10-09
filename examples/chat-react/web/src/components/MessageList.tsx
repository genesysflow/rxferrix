import { useEffect, useMemo, useRef } from "react";
import { getAllPages, useMutation, usePaginatedQuery } from "@ferrix/react";
import { api, type MessageView } from "../ferrix";
import type { ChannelInfo, Profile } from "../lib";
import { MessageItem } from "./MessageItem";

const PAGE = 30;
/** Messages from the same author within this many ms share one header. */
const GROUP_MS = 5 * 60_000;
/** Start loading older messages this far before the top is reached. */
const PRELOAD_PX = 300;

export function MessageList({ channel, me }: { channel: ChannelInfo; me: Profile }) {
  const { results, status, loadMore } = usePaginatedQuery(api.messages.list, { channel: channel._id }, { initialNumItems: PAGE });
  const scrollRef = useRef<HTMLDivElement>(null);
  const sentinelRef = useRef<HTMLDivElement>(null);

  // Infinite scroll: the list is `column-reverse` (newest at the bottom, scroll anchored there), so older pages are
  // added at the top without moving what's on screen. Load more while the sentinel at the top is near the viewport.
  const statusRef = useRef(status);
  const loadMoreRef = useRef(loadMore);
  useEffect(() => {
    statusRef.current = status;
    loadMoreRef.current = loadMore;
  });
  useEffect(() => {
    const root = scrollRef.current;
    const sentinel = sentinelRef.current;
    if (!root || !sentinel) return;
    const observer = new IntersectionObserver(
      ([entry]) => {
        if (entry?.isIntersecting && statusRef.current === "CanLoadMore") loadMoreRef.current(PAGE);
      },
      { root, rootMargin: `${PRELOAD_PX}px 0px 0px 0px` },
    );
    observer.observe(sentinel);
    return () => observer.disconnect();
  }, []);
  // A page that didn't fill the screen leaves the sentinel in view, which fires no new intersection event: measure.
  useEffect(() => {
    const root = scrollRef.current;
    const sentinel = sentinelRef.current;
    if (status !== "CanLoadMore" || !root || !sentinel) return;
    if (sentinel.getBoundingClientRect().bottom >= root.getBoundingClientRect().top - PRELOAD_PX) loadMoreRef.current(PAGE);
  }, [status, results.length]);

  // Mark the channel read whenever a new message shows up while it is open.
  const markRead = useMutation(api.channels.mark_read);
  const newest = results.find((m) => !m._id.startsWith("optimistic:"))?._id;
  useEffect(() => {
    if (!channel.isMember || !newest) return;
    const mark = () => {
      if (document.visibilityState !== "hidden") markRead({ channel: channel._id }).catch(() => {});
    };
    mark();
    document.addEventListener("visibilitychange", mark);
    return () => document.removeEventListener("visibilitychange", mark);
  }, [newest, channel._id, channel.isMember, markRead]);

  // Reactions flip instantly: the optimistic update edits every loaded page that shows the message.
  const toggle = useMutation(api.reactions.toggle);
  const toggleReaction = useMemo(
    () =>
      toggle.withOptimisticUpdate((store, { message, emoji }) => {
        for (const { args, value } of getAllPages(store, api.messages.list, { channel: channel._id })) {
          if (!value.page.some((m) => m._id === message)) continue;
          const page = value.page.map((m) => (m._id === message ? { ...m, reactions: toggledReactions(m, emoji, me.name) } : m));
          store.setQuery(api.messages.list, args, { ...value, page });
        }
      }),
    [toggle, channel._id, me.name],
  );

  return (
    <div className="messages" ref={scrollRef} data-testid="messages">
      {results.map((m, i) => {
        const older = results[i + 1];
        const grouped =
          older !== undefined &&
          older.author === m.author &&
          older.kind === "message" &&
          m.kind === "message" &&
          m.sentAt - older.sentAt < GROUP_MS;
        return (
          <MessageItem
            key={m._id}
            message={m}
            grouped={grouped}
            mine={m.author === me.user}
            canInteract={channel.isMember}
            onReact={toggleReaction}
          />
        );
      })}
      <div ref={sentinelRef} className="history-edge">
        {status === "LoadingFirstPage" || status === "LoadingMore" ? (
          <span className="muted">Loading messages…</span>
        ) : status === "Exhausted" ? (
          <div className="channel-start" data-testid="channel-start">
            <h3>
              {channel.private ? "🔒 " : "#"}
              {channel.name}
            </h3>
            <p>This is the very beginning of #{channel.name}.</p>
          </div>
        ) : (
          <button className="link" onClick={() => loadMore(PAGE)}>
            Load older messages
          </button>
        )}
      </div>
    </div>
  );
}

/** `m`'s reaction groups with the signed-in user's `emoji` toggled (for the optimistic update). */
function toggledReactions(m: MessageView, emoji: string, myName: string): MessageView["reactions"] {
  const groups = m.reactions.map((g) => ({ ...g, names: [...g.names] }));
  const group = groups.find((g) => g.emoji === emoji);
  if (!group) return [...groups, { emoji, count: 1, mine: true, names: [myName] }];
  if (group.mine) {
    group.count -= 1;
    group.mine = false;
    group.names = group.names.filter((n) => n !== myName);
    return groups.filter((g) => g.count > 0);
  }
  group.count += 1;
  group.mine = true;
  group.names.push(myName);
  return groups;
}

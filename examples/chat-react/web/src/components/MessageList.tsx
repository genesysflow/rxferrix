import { useCallback, useEffect, useLayoutEffect, useMemo, useRef } from "react";
import { getAllPages, useMutation, usePaginatedQueryWithStatus } from "@rxferrix/react";
import { api, type MessageView } from "../rxferrix";
import type { ChannelInfo, Profile } from "../lib";
import { AccessError } from "./AccessError";
import { MessageItem } from "./MessageItem";

const PAGE = 30;
/** Messages from the same author within this many ms share one header. */
const GROUP_MS = 5 * 60_000;
/** Start loading older messages this far before the top is reached. */
const PRELOAD_PX = 300;

export function MessageList({ channel, me, onBack }: { channel: ChannelInfo; me: Profile; onBack: () => void }) {
  // The non-throwing variant: an access error (e.g. `not_a_member` after being removed) is shown in place.
  const { results, status, error, loadMore } = usePaginatedQueryWithStatus(
    api.messages.list,
    { channel: channel._id },
    { initialNumItems: PAGE },
  );
  const scrollRef = useRef<HTMLDivElement>(null);
  // Infinite scroll: the list is `column-reverse` (newest at the bottom, scroll anchored there), so older pages are
  // added at the top without moving what's on screen. scrollTop is zero at the bottom and negative towards the top.
  // Check the remaining distance on every scroll and page arrival: an intersection observer can miss a transition
  // while a page is loading, then leave the reader at the top with no further event to load the next page.
  const loadOlderIfNearTop = useCallback(() => {
    const root = scrollRef.current;
    if (status !== "CanLoadMore" || !root) return;
    const remaining = root.scrollHeight - root.clientHeight + root.scrollTop;
    if (remaining <= PRELOAD_PX) loadMore(PAGE);
  }, [status, loadMore]);
  useLayoutEffect(() => {
    loadOlderIfNearTop();
  }, [loadOlderIfNearTop, results.length, error]);
  // Resizing can also bring the top within reach, or leave a short page unable to fill the viewport.
  useEffect(() => {
    const root = scrollRef.current;
    if (!root) return;
    const observer = new ResizeObserver(loadOlderIfNearTop);
    observer.observe(root);
    return () => observer.disconnect();
  }, [loadOlderIfNearTop, error]);

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

  if (error) return <AccessError error={error} onBack={onBack} />;
  return (
    <div className="messages" ref={scrollRef} onScroll={loadOlderIfNearTop} data-testid="messages">
      {results.map((m, i) => {
        const older = results[i + 1];
        const grouped =
          older !== undefined &&
          older.author === m.author &&
          older.kind === "message" &&
          m.kind === "message" &&
          m._creationTime - older._creationTime < GROUP_MS;
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
      <div className="history-edge">
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

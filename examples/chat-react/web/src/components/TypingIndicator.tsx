import { useQuery } from "@ferrix/react";
import { api, type Id } from "../ferrix";

export function TypingIndicator({ channel }: { channel: Id<"channels"> }) {
  const typing = useQuery(api.typing.list, { channel }) ?? [];
  const names = typing.map((t) => t.name);
  const text =
    names.length === 0
      ? ""
      : names.length === 1
        ? `${names[0]} is typing`
        : names.length === 2
          ? `${names[0]} and ${names[1]} are typing`
          : "Several people are typing";
  return (
    <div className="typing" data-testid="typing" aria-live="polite">
      {text && (
        <>
          <span className="dots" aria-hidden>
            <i />
            <i />
            <i />
          </span>
          {text}…
        </>
      )}
    </div>
  );
}

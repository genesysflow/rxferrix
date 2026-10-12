import { useCallback, useEffect, useState } from "react";
import { RxFerrixError } from "@rxferrix/react";
import type { Doc, Id } from "./rxferrix";

// The server's view structs (`#[object]`) are declared by codegen under their own names.
export type { ChannelInfo, ChannelSummary, MessageView } from "./rxferrix";
export type Profile = Doc<"profiles">;

const FRIENDLY: Record<string, string> = {
  invalid_credentials: "Wrong email or password.",
  email_in_use: "That email already has an account. Sign in instead.",
  weak_password: "Use a password of at least 8 characters.",
  invalid_email: "That doesn't look like an email address.",
  invalid_code: "That code is wrong or has expired.",
  rate_limited: "Too many attempts. Wait a moment and try again.",
  network_error: "Can't reach the server.",
};

/** A message for the UI: the server's `UserError` message, or a friendlier one for well-known codes. */
export function errorMessage(e: unknown): string {
  if (e instanceof RxFerrixError) return (e.code && FRIENDLY[e.code]) || e.message;
  if (e instanceof Error) return e.message;
  return String(e);
}

export function errorCode(e: unknown): string | undefined {
  return e instanceof RxFerrixError ? e.code : undefined;
}

export function formatTime(ms: number): string {
  const d = new Date(ms);
  const today = new Date();
  const time = d.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
  if (d.toDateString() === today.toDateString()) return time;
  return `${d.toLocaleDateString([], { month: "short", day: "numeric" })}, ${time}`;
}

export function initials(name: string): string {
  const parts = name.trim().split(/\s+/);
  return ((parts[0]?.[0] ?? "?") + (parts.length > 1 ? (parts[parts.length - 1]?.[0] ?? "") : "")).toUpperCase();
}

/** The open channel lives in the URL (`#/c/<id>`), so it survives reloads and can be linked to. */
export function useChannelRoute(): [Id<"channels"> | null, (id: Id<"channels"> | null, replace?: boolean) => void] {
  const read = () => {
    const m = /^#\/c\/([0-9a-z]+)/.exec(window.location.hash);
    return m ? (m[1] as Id<"channels">) : null;
  };
  const [route, setRoute] = useState(read);
  useEffect(() => {
    const onChange = () => setRoute(read());
    window.addEventListener("hashchange", onChange);
    return () => window.removeEventListener("hashchange", onChange);
  }, []);
  const navigate = useCallback((id: Id<"channels"> | null, replace = false) => {
    const hash = id ? `#/c/${id}` : "#/";
    if (replace) history.replaceState(history.state, "", hash);
    else window.location.hash = hash;
    setRoute(id);
  }, []);
  return [route, navigate];
}

/** `url` with a `thumb` parameter, for servers that resize images (others ignore it). */
export function thumbUrl(url: string, size: number): string {
  return `${url}${url.includes("?") ? "&" : "?"}thumb=${size}`;
}

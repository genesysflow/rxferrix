/**
 * A tiny path router under `/_/` (the server answers every `/_/…` path with index.html).
 *
 * Routes: `/_/` overview, `/_/data[/<table>][?doc=<id>]`, `/_/functions[?fn=<path>]`, `/_/logs`, `/_/users`,
 * `/_/files[?file=<id>]`, `/_/scheduler[?state=<state>]`, `/_/setup#token=…`.
 */
export const BASE = "/_/";

export type Page = "overview" | "data" | "functions" | "logs" | "users" | "files" | "scheduler" | "setup";

export interface Route {
  page: Page;
  /** Path segments after the page (e.g. the table name). */
  rest: string[];
  params: URLSearchParams;
}

export function parseRoute(pathname: string, search: string): Route {
  const path = pathname.startsWith(BASE) ? pathname.slice(BASE.length) : pathname.replace(/^\/+/, "");
  const [first = "", ...rest] = path.split("/").filter(Boolean).map(decodeURIComponent);
  const pages: Page[] = ["data", "functions", "logs", "users", "files", "scheduler", "setup"];
  const page = (pages as string[]).includes(first) ? (first as Page) : "overview";
  return { page, rest, params: new URLSearchParams(search) };
}

export function href(page: Page, rest: string[] = [], params: Record<string, string | null | undefined> = {}): string {
  const path = page === "overview" ? "" : [page, ...rest].map(encodeURIComponent).join("/");
  const p = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) if (v) p.set(k, v);
  const qs = p.toString();
  return `${BASE}${path}${qs ? `?${qs}` : ""}`;
}

class Router {
  url = $state({ pathname: location.pathname, search: location.search });
  route = $derived(parseRoute(this.url.pathname, this.url.search));

  constructor() {
    addEventListener("popstate", () => this.sync());
  }

  private sync() {
    this.url = { pathname: location.pathname, search: location.search };
  }

  navigate(to: string, options: { replace?: boolean } = {}) {
    if (to === location.pathname + location.search) return;
    if (options.replace) history.replaceState(null, "", to);
    else history.pushState(null, "", to);
    this.sync();
  }
}

export const router = new Router();

/** onclick for `<a href>`: client-side navigation unless a modifier key asks for a new tab. */
export function link(e: MouseEvent) {
  const a = e.currentTarget as HTMLAnchorElement;
  if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
  e.preventDefault();
  router.navigate(a.getAttribute("href")!);
}

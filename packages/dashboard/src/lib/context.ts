/** Shared dashboard state: the table list (for following ids) and the auth instance. */
import { getContext, setContext } from "svelte";
import type { FerrixAuth } from "@ferrix/client";
import type { TableSummary } from "./api.js";

/** The context key (exported for component tests). */
export const DASHBOARD_CONTEXT = Symbol("ferrix-dashboard");

export interface DashboardContext {
  auth: FerrixAuth;
  /** The live table list (undefined while loading). */
  readonly tables: TableSummary[] | undefined;
  /** Table name for a table number (from an id). */
  tableByNumber(n: number): TableSummary | undefined;
  /** Show a transient message. */
  toast(message: string, kind?: "info" | "error"): void;
}

export function setDashboard(ctx: DashboardContext): void {
  setContext(DASHBOARD_CONTEXT, ctx);
}

export function dashboardContext(): DashboardContext {
  return getContext<DashboardContext>(DASHBOARD_CONTEXT);
}

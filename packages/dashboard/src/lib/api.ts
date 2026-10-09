/**
 * Hand-written references to the admin-only system functions (`_system/dashboard:*`, defined in
 * crates/ferrix/src/dashboard/mod.rs, and `_system/storage:*`) and the types they return. Codegen leaves system functions out of app APIs.
 */
import { makeFunctionReference, type PaginationOpts, type PaginationResult } from "@ferrix/client";

export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
export type JsonObject = { [key: string]: Json };

export type TableAccess = "readWrite" | "limited" | "readOnly";

export interface IndexSummary {
  name: string;
  fields: string[];
  builtin: boolean;
  usable: boolean;
}

export interface TableSummary {
  name: string;
  number: number;
  system: boolean;
  defined: boolean;
  documents: number;
  indexes: IndexSummary[];
  ts: string | null;
  secretFields: string[];
  access: TableAccess;
}

export type FilterOp = "eq" | "gt" | "gte" | "lt" | "lte";
export interface IndexFilter {
  field: string;
  op: FilterOp;
  value: Json;
}

export interface DocumentsArgs {
  table: string;
  index?: string | null;
  filters?: IndexFilter[] | null;
  order?: "asc" | "desc" | null;
  paginationOpts: PaginationOpts;
}

export interface TableDocument {
  table: string;
  document: JsonObject;
}

export interface FunctionSummary {
  path: string;
  kind: "query" | "mutation" | "action";
  visibility: "public" | "internal";
  system: boolean;
  argsTs: string;
  outputTs: string;
}

export interface RunResult {
  kind: string;
  success: boolean;
  value: Json;
  error: { message: string; code: string | null; data: Json | null } | null;
  logLines: string[];
  durationMs: number;
  ts: number | null;
  identity: string | null;
}

export interface UserSummary {
  _id: string;
  _creationTime: number;
  email: string | null;
  emailVerified: boolean;
  name: string | null;
  image: string | null;
  providers: string[];
  hasPassword: boolean;
  sessions: number;
}

export interface SessionSummary {
  _id: string;
  _creationTime: number;
  expiresAt: number;
  lastUsedAt: number;
  userAgent: string | null;
  ip: string | null;
  expired: boolean;
}

export interface DeletedUser {
  sessions: number;
  accounts: number;
  credentials: number;
  verificationCodes: number;
}

const q = <Args, Out>(name: string) => makeFunctionReference<"query", Args, Out>("query", `_system/dashboard:${name}`);
const m = <Args, Out>(name: string) =>
  makeFunctionReference<"mutation", Args, Out>("mutation", `_system/dashboard:${name}`);

export const dashboard = {
  tables: q<Record<string, never>, TableSummary[]>("tables"),
  documents: q<DocumentsArgs, PaginationResult<JsonObject>>("documents"),
  document: q<{ id: string }, TableDocument | null>("document"),
  insertDocument: m<{ table: string; document: JsonObject }, string>("insert_document"),
  patchDocument: m<{ id: string; patch: JsonObject }, null>("patch_document"),
  replaceDocument: m<{ id: string; document: JsonObject }, null>("replace_document"),
  deleteDocuments: m<{ ids: string[] }, number>("delete_documents"),
  functions: q<Record<string, never>, FunctionSummary[]>("functions"),
  runFunction: makeFunctionReference<"action", { path: string; args?: Json; asUser?: string | null }, RunResult>(
    "action",
    "_system/dashboard:run_function",
  ),
  users: q<{ search?: string | null; paginationOpts: PaginationOpts }, PaginationResult<UserSummary>>("users"),
  userSessions: q<{ user: string }, SessionSummary[]>("user_sessions"),
  revokeSession: m<{ session: string }, null>("revoke_session"),
  deleteUser: m<{ user: string }, DeletedUser>("delete_user"),
  me: q<Record<string, never>, { _id: string; email: string } | null>("me"),
} as const;

/** A stored file (`FileSummary` in crates/ferrix/src/storage/mod.rs). */
export interface FileSummary {
  _id: string;
  _creationTime: number;
  sha256: string;
  size: number;
  contentType: string | null;
  createdBy: string | null;
  filename: string | null;
  /** The serving URL (`ctx.storage.get_url`), absolute when the server has a public URL. */
  url: string;
}

/** The Files page's admin-only system functions (`_system/storage:*`, crates/ferrix/src/storage/mod.rs). */
export const storage = {
  files: makeFunctionReference<"query", { paginationOpts: PaginationOpts }, PaginationResult<FileSummary>>(
    "query",
    "_system/storage:files",
  ),
  file: makeFunctionReference<"query", { id: string }, FileSummary | null>("query", "_system/storage:file"),
  deleteFile: makeFunctionReference<"mutation", { id: string }, null>("mutation", "_system/storage:delete_file"),
  generateUploadUrl: makeFunctionReference<"mutation", Record<string, never>, string>(
    "mutation",
    "_system/storage:generate_upload_url",
  ),
};

/** Scheduled job states (`JobState` in crates/ferrix/src/scheduler/tables.rs). */
export type JobState = "pending" | "inProgress" | "success" | "failed" | "canceled";

/** A `_scheduled_jobs` document. */
export interface ScheduledJob {
  _id: string;
  _creationTime: number;
  path: string;
  kind: "mutation" | "action";
  args: Json;
  runAt: number;
  state: JobState;
  attempts: number;
  maxAttempts: number;
  /** The return value as (possibly cut) JSON text. */
  result: string | null;
  error: string | null;
  scheduledAt: number;
  startedAt: number | null;
  completedAt: number | null;
  /** A function path, `cron:<name>` or `dashboard`. */
  origin: string | null;
}

export interface CronSummary {
  _id: string;
  name: string;
  schedule: string;
  path: string;
  args: Json;
  lastRun: number | null;
  nextRun: number | null;
  lastJob: string | null;
  lastJobState: JobState | null;
}

export type CancelOutcome = "canceled" | "alreadyFinished" | "running";

const sq = <Args, Out>(name: string) => makeFunctionReference<"query", Args, Out>("query", `_system/scheduler:${name}`);
const sm = <Args, Out>(name: string) =>
  makeFunctionReference<"mutation", Args, Out>("mutation", `_system/scheduler:${name}`);

/** The admin-only scheduler functions (`_system/scheduler:*`, crates/ferrix/src/scheduler/mod.rs). */
export const scheduler = {
  jobs: sq<{ state?: JobState | null; paginationOpts: PaginationOpts }, PaginationResult<ScheduledJob>>("jobs"),
  cancelJob: sm<{ id: string }, CancelOutcome>("cancel_job"),
  runJobNow: sm<{ id: string }, string>("run_job_now"),
  crons: sq<Record<string, never>, CronSummary[]>("crons"),
} as const;

/** One execution from the execution log (`LogEntry` in Rust). */
export interface LogEntry {
  id: number;
  timestamp: number;
  path: string;
  kind: "query" | "mutation" | "action";
  identity: string | null;
  durationUs: number;
  outcome: "success" | "userError" | "serverError";
  error: string | null;
  logLines: string[];
  args: string;
  argsTruncated: boolean;
}

export interface LogFilter {
  pathPrefix?: string;
  kind?: LogEntry["kind"] | "";
  outcome?: LogEntry["outcome"] | "";
  identity?: string;
  excludeSystem?: boolean;
  limit?: number;
  beforeId?: number;
}

export interface FunctionStats {
  path: string;
  kind: LogEntry["kind"];
  calls: number;
  errors: number;
  userErrors: number;
  serverErrors: number;
  p50Us: number;
  p95Us: number;
}

export interface Stats {
  since: number;
  functions: FunctionStats[];
  overview: {
    now: number;
    startedAt: number;
    version: string;
    dev: boolean;
    ts: number;
    commitTime: number;
    tables: number;
    documents: number;
    registeredFunctions: number;
    sockets: number;
    subscriptions: number;
    droppedLogEntries: number;
    files: { name: string; bytes: number }[];
  };
}

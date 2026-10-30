/**
 * Hand-written references to the admin-only system functions and the types they return (codegen leaves system
 * functions out of app APIs): `_system/dashboard:*` (crates/rxferrix/src/dashboard/mod.rs), `_system/storage:*`,
 * `_system/scheduler:*`, `_system/workflow:*`, `_system/migrations:*`, `_system/settings:*`, `_system/env:*` and `_system/audit:list`, plus the types of
 * the `/api/admin/*` endpoints (lib/admin.ts). See docs/CONTRACT.md §4.
 */
import { makeFunctionReference, type PaginationOpts, type PaginationResult } from "@rxferrix/client";

export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
export type JsonObject = { [key: string]: Json };

export type TableAccess = "readWrite" | "limited" | "readOnly";

export interface IndexSummary {
  name: string;
  fields: string[];
  builtin: boolean;
  usable: boolean;
}

export interface SearchIndexSummary {
  name: string;
  kind: "search" | "vector";
  field: string;
  filterFields: string[];
  /** Vector indexes only. */
  dimensions: number | null;
  /** Search indexes only. */
  tokenizer?: "unicode61" | "porter" | "trigram" | "snowball" | null;
  /** Stemming tokenizers (`porter`, `snowball`): the language. */
  language?: string | null;
  /** Search indexes only: whether diacritics are folded (`café` = `cafe`). */
  removeDiacritics?: boolean | null;
  /** Vector indexes only. */
  method?: "exact" | "hnsw" | null;
  /** HNSW vector indexes only: the graph's parameters. */
  hnsw?: { m: number; efConstruction: number; efSearch: number } | null;
}

export interface TableSummary {
  name: string;
  number: number;
  system: boolean;
  defined: boolean;
  documents: number;
  indexes: IndexSummary[];
  searchIndexes: SearchIndexSummary[];
  ts: string | null;
  secretFields: string[];
  access: TableAccess;
  /** In the analytics copy (`--olap`): user tables and the system tables without secrets (absent from older servers). */
  analytics?: boolean;
  /** Its columns in the analytics copy, from the Rust type (empty when not defined in code or not copied). */
  columns?: { name: string; sqlType: string }[];
  /** Its live aggregates (`#[aggregate]`; absent from older servers). */
  aggregates?: { name: string; groupFields: string[]; sumFields: string[] }[];
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

export interface SearchArgs {
  table: string;
  index: string;
  text: string;
  filters?: { field: string; value: Json }[] | null;
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
  /** Two-factor authentication is enabled (a confirmed authenticator). */
  mfaEnabled: boolean;
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
  /** The admin id of an impersonation session, else null. */
  impersonatedBy: string | null;
}

export interface DeletedUser {
  sessions: number;
  accounts: number;
  credentials: number;
  verificationCodes: number;
  /** Second factors (0 or 1). */
  mfaFactors: number;
}

/** The signed-in administrator (`me`). */
export interface AdminSummary {
  _id: string;
  email: string;
  /** Two-factor authentication is enabled for this administrator (live). */
  mfaEnabled: boolean;
}

const q = <Args, Out>(name: string) => makeFunctionReference<"query", Args, Out>("query", `_system/dashboard:${name}`);
const m = <Args, Out>(name: string) =>
  makeFunctionReference<"mutation", Args, Out>("mutation", `_system/dashboard:${name}`);

export const dashboard = {
  tables: q<Record<string, never>, TableSummary[]>("tables"),
  documents: q<DocumentsArgs, PaginationResult<JsonObject>>("documents"),
  searchDocuments: q<SearchArgs, PaginationResult<JsonObject>>("search_documents"),
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
  /** Remove a user's second factor (lost authenticator and recovery codes); audited as `reset_mfa`. */
  resetMfa: m<{ user: string }, null>("reset_mfa"),
  me: q<Record<string, never>, AdminSummary | null>("me"),
} as const;

/** A stored file (`FileSummary` in crates/rxferrix/src/storage/mod.rs). */
export interface FileSummary {
  _id: string;
  _creationTime: number;
  sha256: string;
  size: number;
  contentType: string | null;
  createdBy: string | null;
  filename: string | null;
  /**
   * The serving URL (`ctx.storage.get_url`), absolute when the server has a public URL. In private storage mode it is
   * a signed URL that already has a query string: add parameters with `withParams` (lib/format.ts).
   */
  url: string;
  /** `url` with `download=1` added correctly. */
  downloadUrl: string;
}

/** The Files page's admin-only system functions (`_system/storage:*`, crates/rxferrix/src/storage/mod.rs). */
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

/** Scheduled job states (`JobState` in crates/rxferrix/src/scheduler/tables.rs). */
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
  /** A function path, `cron:<name>`, `dashboard`, `onComplete:<job id>` or `workflow:<id>`. */
  origin: string | null;
  /** The workpool whose parallelism limits this action; null for the default pool. */
  pool: string | null;
  /** The mutation run (as a job of its own) once this one has finished. */
  onComplete: { path: string; context: Json } | null;
  /** The retry backoff; null for the scheduler's. */
  backoff: { initialMs: number; base: number } | null;
  /** How much of each retry delay is random (0 to 1); null for the scheduler's. */
  jitter: number | null;
}

/** `GET /api/admin/job-history` (lib/admin.ts): finished jobs moved from `_scheduled_jobs` to `aux.db`. */
export interface JobHistoryFilter {
  state?: "success" | "failed" | "canceled" | null;
  /** Exactly this function. */
  path?: string;
  limit?: number;
  /** The `completedAt` and `_id` of the last job of the previous page (both or neither). */
  beforeCompletedAt?: number;
  beforeId?: string;
}

export interface CronSummary {
  _id: string;
  name: string;
  schedule: string;
  /** The IANA time zone of the expression; null for UTC. */
  timezone: string | null;
  /** A rxferrix system cron (name starting with `_`, e.g. `_storage_sweep`). */
  system: boolean;
  path: string;
  args: Json;
  lastRun: number | null;
  nextRun: number | null;
  lastJob: string | null;
  lastJobState: JobState | null;
}

/** `cancel_job`: pending jobs and actions in progress can be canceled. */
export type CancelOutcome = "canceled" | "alreadyFinished";

const sq = <Args, Out>(name: string) => makeFunctionReference<"query", Args, Out>("query", `_system/scheduler:${name}`);
const sm = <Args, Out>(name: string) =>
  makeFunctionReference<"mutation", Args, Out>("mutation", `_system/scheduler:${name}`);

/** The admin-only scheduler functions (`_system/scheduler:*`, crates/rxferrix/src/scheduler/mod.rs). */
export const scheduler = {
  jobs: sq<{ state?: JobState | null; paginationOpts: PaginationOpts }, PaginationResult<ScheduledJob>>("jobs"),
  cancelJob: sm<{ id: string }, CancelOutcome>("cancel_job"),
  runJobNow: sm<{ id: string }, string>("run_job_now"),
  crons: sq<Record<string, never>, CronSummary[]>("crons"),
  /** The id of the enqueued job; user error `running` while the previous run is unfinished (unless `force`). */
  runCronNow: sm<{ name: string; force?: boolean | null }, string>("run_cron_now"),
} as const;

// ---------------------------------------------------------------------------------------------------------------------
// Workflows (`_system/workflow:*`, crates/rxferrix/src/workflow)

export type WorkflowState = "running" | "success" | "failed" | "canceled";

/** A `_workflows` document. */
export interface Workflow {
  _id: string;
  _creationTime: number;
  /** The workflow's path, e.g. `onboarding:onboard`. */
  name: string;
  args: Json;
  state: WorkflowState;
  result: Json | null;
  error: string | null;
  /** Steps started so far. */
  steps: number;
  startedAt: number;
  completedAt: number | null;
  origin: string | null;
  onComplete: { path: string; context: Json } | null;
  /** Events sent before the workflow waited for them. */
  events: { name: string; value: Json }[];
}

export type StepKind = "query" | "mutation" | "action" | "sleep" | "event";
export type StepState = "running" | "success" | "failed" | "canceled";

/** A `_workflow_steps` document: one entry of a workflow's journal. */
export interface WorkflowStep {
  _id: string;
  _creationTime: number;
  workflow: string;
  index: number;
  kind: StepKind;
  /** The function's path, the event's name, or `sleep`. */
  name: string;
  /** The function's args; `{ ms }` for a sleep. */
  args: Json;
  state: StepState;
  result: Json | null;
  error: string | null;
  /** The scheduled job that runs it (mutations, actions) or ends it (sleeps). */
  job: string | null;
  startedAt: number;
  completedAt: number | null;
}

export type CancelWorkflowOutcome = "canceled" | "alreadyFinished";

const wq = <Args, Out>(name: string) => makeFunctionReference<"query", Args, Out>("query", `_system/workflow:${name}`);
const wm = <Args, Out>(name: string) =>
  makeFunctionReference<"mutation", Args, Out>("mutation", `_system/workflow:${name}`);

/** The admin-only workflow functions (`_system/workflow:*`, crates/rxferrix/src/workflow/mod.rs). */
export const workflows = {
  list: wq<{ state?: WorkflowState | null; paginationOpts: PaginationOpts }, PaginationResult<Workflow>>("list"),
  steps: wq<{ id: string }, WorkflowStep[]>("steps"),
  cancelWorkflow: wm<{ id: string }, CancelWorkflowOutcome>("cancel_workflow"),
} as const;

// ---------------------------------------------------------------------------------------------------------------------
// Migrations (`_system/migrations:*`, crates/rxferrix/src/migrations)

export type MigrationState = "pending" | "running" | "done" | "failed" | "canceled";

export interface MigrationSummary {
  name: string;
  table: string;
  /** Still declared in code (a row can outlive its migration). */
  declared: boolean;
  batchSize: number | null;
  /** The migrations it runs after (`#[migration(after = [..])]`), which must be done first. */
  after: string[];
  state: MigrationState;
  processed: number;
  changed: number;
  /** The table's current document count, for a progress bar. */
  documents: number;
  startedAt: number | null;
  finishedAt: number | null;
  error: string | null;
  failedDocument: string | null;
  /** `"passed"` or `"failed: <report>"`. */
  schemaCheck: string | null;
  /** The table opened with documents that don't match its type: lenient writes until its migrations finish. */
  tableMigrating: boolean;
  /** `#[migration(on_read)]`: while it is pending, typed reads see old documents in the new shape. */
  onRead: boolean;
  /** This run checks each document it processes (the table's last pending migration): no table scan at the end. */
  validated: boolean | null;
}

export type RunOutcome =
  | "completed"
  | "blocked"
  | "failed"
  | "canceled"
  | "alreadyDone"
  | "alreadyRunning"
  | "stopped"
  | "dryRun";

export interface ChangeExample {
  id: string;
  before: Json;
  /** null: the migration would delete the document. */
  after: Json | null;
}

export interface MigrationReport {
  name: string;
  table: string;
  outcome: RunOutcome;
  batches: number;
  processed: number;
  changed: number;
  error: string | null;
  failedDocument: string | null;
  schemaCheck: string | null;
  /** Dry runs: up to 5 documents that would change. */
  examples: ChangeExample[];
}

export const migrations = {
  list: makeFunctionReference<"query", Record<string, never>, MigrationSummary[]>("query", "_system/migrations:list"),
  run: makeFunctionReference<
    "action",
    { name?: string | null; dryRun?: boolean | null; restart?: boolean | null; force?: boolean | null },
    { started: string[]; reports: MigrationReport[] }
  >("action", "_system/migrations:run"),
  cancel: makeFunctionReference<"mutation", { name: string }, "canceled" | "notRunning">(
    "mutation",
    "_system/migrations:cancel",
  ),
} as const;

// ---------------------------------------------------------------------------------------------------------------------
// Settings (`_system/settings:*`, actions: crates/rxferrix/src/settings)

export type OAuthKind = "github" | "google" | "discord" | "apple" | "oidc";
/** What the server shows in place of a stored secret (and accepts back as "keep it"). */
export const MASK = "••••";

export interface OAuthProviderView {
  id: string;
  kind: OAuthKind;
  issuer: string | null;
  clientId: string;
  /** `"••••"` when a secret is stored, else null. */
  clientSecret: string | null;
  enabled: boolean;
  scopes: string[] | null;
  /** Apple: the developer team id and the key id (the secret is then the private key). */
  teamId: string | null;
  keyId: string | null;
  /** A provider with this id is configured in code: it wins and this entry is ignored. */
  locked: boolean;
}

export interface SmtpView {
  /** Without the password. */
  url: string;
  from: string;
  /** `"••••"` when a password is stored, else null. */
  password: string | null;
}

/** The request headers a reverse proxy puts the client IP in, trusted in order (the first one present wins). */
export interface TrustedProxy {
  headers: string[];
  /** Take the leftmost IP of a list (the original client, but forgeable) instead of the rightmost. */
  useLeftmostIp: boolean;
  /** The proxy's addresses (IPs or CIDR ranges): the headers count only on requests from them. Absent: any address. */
  proxies?: string[] | null;
}

/** Which callers a rate limit rule applies to. */
export type RateLimitAudience = "all" | "guest" | "signedIn";

/** At most `maxRequests` per `intervalSecs` per client IP (a token bucket: bursts up to `maxRequests`). */
export interface RateLimitRule {
  /** An HTTP path prefix (`/api/`) or app functions (`messages:send`, `messages:*`, `*:create`, `*`). */
  label: string;
  maxRequests: number;
  intervalSecs: number;
  audience: RateLimitAudience;
}

/** Request rate limits (crates/rxferrix/src/settings/rate_limits.rs); off keeps the rules without applying them. */
export interface RateLimitSettings {
  enabled: boolean;
  rules: RateLimitRule[];
}

/** An auth email's subject and HTML body, with placeholders (`{link}`, `{code}`, `{expires}`, `{app_name}`, `{email}`). */
export interface EmailTemplate {
  subject: string;
  html: string;
}

/** The kinds of auth email a template can replace. */
export type EmailKind = "verifyEmail" | "resetPassword" | "signIn";

/** The auth emails' templates; a template not set (null): the built-in plain-text email. */
export interface EmailTemplates {
  /** `{app_name}`; null: the display name of the SMTP sender, if any. */
  appName: string | null;
  verifyEmail: EmailTemplate | null;
  resetPassword: EmailTemplate | null;
  signIn: EmailTemplate | null;
}

export interface SettingsView {
  oauth: OAuthProviderView[];
  smtp: SmtpView | null;
  redirectUrls: string[] | null;
  signups: boolean | null;
  accessTokenTtlSecs: number | null;
  sessionTtlSecs: number | null;
  corsOrigins: string[] | null;
  /** Scheduled backups and their destination (crates/rxferrix/src/settings/backups.rs). */
  backups: BackupsView | null;
  trustedProxy: TrustedProxy | null;
  rateLimits: RateLimitSettings | null;
  emailTemplates: EmailTemplates | null;
  /** The built-in emails as templates (each one set): where a custom template starts from. */
  emailTemplateDefaults: EmailTemplates;
  /** Fields the configuration fixes: `signups`, `accessTokenTtlSecs`, `sessionTtlSecs`, `redirectUrls`, `smtp`,
   * `trustedProxy`, `oauth.<id>`, `backups.schedule`, `backups.destination`. */
  locked: string[];
  effective: {
    signups: boolean;
    accessTokenTtlSecs: number;
    sessionTtlSecs: number;
    redirectUrls: string[];
    oauthProviders: string[];
    emailConfigured: boolean;
    /** null: any origin. */
    corsOrigins: string[] | null;
    backups: BackupsEffective;
    /** Header names in lowercase; none: the peer address. */
    trustedProxy: TrustedProxy;
    /** `{app_name}` in the email templates (the setting, else the SMTP sender's display name); may be empty. */
    appName: string;
  };
}

/** Where backups go. */
export type BackupDestination =
  | { kind: "default" }
  | { kind: "local"; path: string }
  | {
      kind: "s3";
      bucket: string;
      prefix?: string;
      /** Default: the server's AWS_REGION, else us-east-1. */
      region?: string | null;
      /** A non-AWS endpoint (R2, MinIO, …). */
      endpoint?: string | null;
      /** Empty: the server's AWS_* environment credentials. */
      accessKeyId?: string | null;
    };

export interface BackupsView {
  /** null: no schedule. */
  everySecs: number | null;
  keep: number;
  includeAux: boolean;
  includeFiles: boolean;
  destination: BackupDestination;
  /** `"••••"` when stored. */
  secretAccessKey: string | null;
}

export interface BackupsInput {
  everySecs: number | null;
  keep?: number;
  includeAux?: boolean;
  includeFiles?: boolean;
  destination: BackupDestination;
  /** Omitted, null or `"••••"`: keep the stored one. */
  secretAccessKey?: string | null;
}

/** Backups as they run (settings and command line merged). */
export interface BackupsEffective {
  target: string;
  everySecs: number | null;
  keep: number;
  includeAux: boolean;
  includeFiles: boolean;
  /** The server was built with S3 support. */
  s3Available: boolean;
}

export interface OAuthProviderInput {
  id: string;
  kind: OAuthKind;
  issuer?: string | null;
  clientId: string;
  /** Omitted, null or `"••••"`: keep the stored one; anything else replaces it. */
  clientSecret?: string | null;
  enabled: boolean;
  scopes?: string[] | null;
  /** Apple only (required there). */
  teamId?: string | null;
  keyId?: string | null;
}

export interface SmtpInput {
  url: string;
  from: string;
  /** Omitted, null or `"••••"`: keep; `""`: none. */
  password?: string | null;
}

export type SettingsKey =
  | "oauth"
  | "smtp"
  | "redirectUrls"
  | "signups"
  | "accessTokenTtlSecs"
  | "sessionTtlSecs"
  | "corsOrigins"
  | "backups"
  | "trustedProxy"
  | "rateLimits"
  | "emailTemplates";

export interface SettingsUpdate {
  oauth?: OAuthProviderInput[];
  smtp?: SmtpInput;
  redirectUrls?: string[];
  signups?: boolean;
  accessTokenTtlSecs?: number;
  sessionTtlSecs?: number;
  corsOrigins?: string[];
  backups?: BackupsInput;
  trustedProxy?: TrustedProxy;
  rateLimits?: RateLimitSettings;
  emailTemplates?: EmailTemplates;
  clear?: SettingsKey[];
}

export const settings = {
  get: makeFunctionReference<"action", Record<string, never>, SettingsView>("action", "_system/settings:get"),
  update: makeFunctionReference<"action", SettingsUpdate, SettingsView>("action", "_system/settings:update"),
  sendTestEmail: makeFunctionReference<"action", { to: string; smtp?: SmtpInput | null }, null>(
    "action",
    "_system/settings:send_test_email",
  ),
  /** Write, read back and delete a probe object at a destination (the one in effect without `backups`). */
  testBackupDestination: makeFunctionReference<"action", { backups?: BackupsInput | null }, string>(
    "action",
    "_system/settings:test_backup_destination",
  ),
} as const;

// ---------------------------------------------------------------------------------------------------------------------
// Environment variables (`_system/env:*`, crates/rxferrix/src/env.rs)

export interface EnvVarView {
  name: string;
  value: string;
  updatedAt: number;
  /** `Id<Admin>` of the last editor (null: the command line). */
  updatedBy: string | null;
}

export const env = {
  list: makeFunctionReference<"query", Record<string, never>, EnvVarView[]>("query", "_system/env:list"),
  set: makeFunctionReference<"mutation", { name: string; value: string }, null>("mutation", "_system/env:set"),
  remove: makeFunctionReference<"mutation", { name: string }, null>("mutation", "_system/env:remove"),
} as const;

/** A valid variable name: what the server accepts (`env::validate_name`). */
export const ENV_NAME = /^[A-Za-z_][A-Za-z0-9_]{0,63}$/;

// ---------------------------------------------------------------------------------------------------------------------
// Audit log (`_system/audit:list`, crates/rxferrix/src/audit.rs)

export const AUDIT_ACTIONS = [
  "insert",
  "patch",
  "replace",
  "delete",
  "revoke_session",
  "delete_user",
  "reset_mfa",
  "mfa_enable",
  "mfa_disable",
  "settings_update",
  "env_create",
  "env_update",
  "env_delete",
  "impersonate",
  "run_function",
] as const;

export interface AuditEntry {
  _id: string;
  _creationTime: number;
  admin: string | null;
  adminEmail: string | null;
  action: string;
  targetTable: string | null;
  targetId: string | null;
  /** The changed fields before and after (whole documents for inserts and deletes), secrets masked. */
  before: Json | null;
  after: Json | null;
  details: Json | null;
  /** ms since the epoch. */
  at: number;
}

export interface AuditArgs {
  admin?: string | null;
  action?: string | null;
  table?: string | null;
  target?: string | null;
  since?: number | null;
  until?: number | null;
  paginationOpts: PaginationOpts;
}

export const audit = {
  list: makeFunctionReference<"query", AuditArgs, PaginationResult<AuditEntry>>("query", "_system/audit:list"),
} as const;

// ---------------------------------------------------------------------------------------------------------------------
// The admin HTTP endpoints' types (lib/admin.ts)

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
  /** Exactly this function. */
  path?: string;
  kind?: LogEntry["kind"] | "";
  outcome?: LogEntry["outcome"] | "";
  identity?: string;
  /** Only executions that took at least this many µs. */
  minDurationUs?: number;
  excludeSystem?: boolean;
  /** Leave out scheduled jobs (crons and workflow steps included) and the functions they call. */
  excludeBackground?: boolean;
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
  /** The slowest call. */
  maxUs: number;
}

/** A column of an analytics query's result. */
export interface OlapColumn {
  name: string;
  /** DuckDB's type name. */
  typeName: string;
}

/** `OlapResult` (crates/rxferrix/src/olap/mod.rs): `POST /api/admin/olap/query` with format json. */
export interface OlapResult {
  columns: OlapColumn[];
  rows: Record<string, Json>[];
  /** The copy reflects every commit up to this one. */
  asOfTs: number;
}

/** `OlapStatus` (crates/rxferrix/src/olap/mod.rs): the analytics copy (`--olap`). */
export interface OlapStatus {
  /** `building`, `ready` or `failed` (retried). */
  state: string;
  /** Why it is being built, or the last error. */
  detail: string | null;
  appliedTs: number;
  latestTs: number;
  appliedTime: number;
  lagMs: number;
  tables: { name: string; rows: number; columns: { name: string; typeName: string }[] }[];
  sizeBytes: number;
  rowsApplied: number;
  rebuilds: number;
}

export interface Stats {
  since: number;
  functions: FunctionStats[];
  /** The analytics copy, or null without `--olap` (absent from older servers). */
  olap?: OlapStatus | null;
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
    /** Distinct queries the server reruns (shared between subscribers). */
    sharedQueries?: number;
    /** Of those, kept as a cache after their last subscriber left (until a write changes them). */
    cachedQueries?: number;
    cacheBytes?: number;
    /** HTTP queries and `ctx.run_query` calls answered from the cache, and those that ran the query. */
    cacheHits?: number;
    cacheMisses?: number;
    droppedLogEntries: number;
    files: { name: string; bytes: number }[];
    /** Over the last 10 seconds. */
    commitsPerSecond?: number;
    /** Pending scheduled jobs. */
    queueDepth?: number;
    /** Scheduled actions in progress. */
    runningJobs?: number;
    subscriptionRounds?: number;
    queryReruns?: number;
    /** `--metrics`: Prometheus `/metrics` is served. */
    metricsEnabled?: boolean;
  };
}

/** `GET /api/admin/client-ip`: the caller's IP as the server resolves it. */
export interface ClientIp {
  ip: string | null;
  /** The trusted header the IP came from; null: the peer address. */
  header: string | null;
  peer: string | null;
  /** Well-known client IP headers (and trusted ones) present on the request, as received. */
  proxyHeaders: { name: string; value: string }[];
}

/** One bucket of `GET /api/admin/stats/timeseries`: the executions that started in `[t, t + bucket)`. */
export interface TimePoint {
  t: number;
  calls: number;
  errors: number;
  userErrors: number;
  serverErrors: number;
  p50Us: number;
  p95Us: number;
  maxUs: number;
}

export interface Timeseries {
  path: string | null;
  /** The bucket size in ms. */
  bucket: number;
  /** The first bucket's start. */
  since: number;
  until: number;
  /** Oldest first; only buckets with executions. */
  points: TimePoint[];
}

/** `BackupInfo` (crates/rxferrix/src/ops/backup.rs). */
export interface BackupInfo {
  name: string;
  bytes: number;
  createdAt: number;
  /** A `rxferrix-auto-*` archive from `--backup-every` (only those are rotated). */
  scheduled: boolean;
  ts: number | null;
}

/** `PendingRestore` (crates/rxferrix/src/ops/restore.rs): a backup staged to replace the database at the next start. */
export interface PendingRestore {
  /** The backup's name, or the uploaded file's. */
  source: string;
  stagedAt: number;
  stagedBy: string | null;
  backupCreatedAt: number;
  /** Commit ts of the backup's snapshot. */
  ts: number;
  rxferrixVersion: string;
  includesAux: boolean;
  blobs: number;
  bytes: number;
  /** The archive had `auth_secret.key`; a staged restore never restores it. */
  secretIgnored: boolean;
  /** Why the last start didn't apply it (cancel it). */
  error: string | null;
  /** A point-in-time restore from the replication target (null for an archive; absent from older servers). */
  pointInTime?: PointInTimeInfo | null;
}

/** `PointInTimeInfo` (crates/rxferrix/src/ops/replicate.rs): what a point-in-time restore restores. */
export interface PointInTimeInfo {
  generation: string;
  /** The snapshot it starts from, and its ts. */
  snapshot: string;
  snapshotTs: number;
  requestedTs: number | null;
  requestedTime: number | null;
  /** The restored commit ts and its time. */
  ts: number;
  time: number;
  /** Journal entries replayed after the snapshot, from how many segments. */
  replayed: number;
  segments: number;
  /** False when the point asked for is beyond the newest replicated commit (which is restored instead). */
  reached: boolean;
}

/** A snapshot of a replication generation. */
export interface ReplicationSnapshot {
  name: string;
  ts: number;
  createdAt: number;
  bytes: number;
}

/** A generation in the replication target: one continuous history, restorable from `fromTs` to `toTs`. */
export interface ReplicationGeneration {
  id: string;
  createdAt: number;
  /** The generation being shipped. */
  current: boolean;
  snapshots: ReplicationSnapshot[];
  segments: number;
  fromTs: number | null;
  toTs: number | null;
  /** When the oldest snapshot was taken; the commit time of `toTs` (current generation only). */
  fromTime: number | null;
  toTime: number | null;
}

/** `ReplicationStatus` (crates/rxferrix/src/ops/replicate.rs): `GET /api/admin/replication` (`--replicate-to`). */
export interface ReplicationStatus {
  target: string;
  everyMs: number;
  /** How long commits of scheduler bookkeeping alone may wait (`--replicate-jobs-every`; absent from older servers). */
  jobsEveryMs?: number;
  snapshotEveryMs: number;
  keepMs: number;
  generation: string | null;
  currentTs: number;
  currentTime: number;
  shippedTs: number;
  shippedTime: number;
  /** Commits not in the target yet, and the age of the oldest of them (0 when caught up). */
  lagCommits: number;
  lagMs: number;
  pendingBytes: number;
  maxPendingBytes: number;
  journaling: boolean;
  lastSegment: { name: string; from: number; to: number; entries: number; bytes: number; shippedAt: number } | null;
  lastSnapshot: ReplicationSnapshot | null;
  nextSnapshotAt: number | null;
  segmentsShipped: number;
  snapshotsTaken: number;
  lastSuccessAt: number | null;
  lastError: string | null;
  lastErrorAt: number | null;
  failures: number;
  generations: ReplicationGeneration[];
}

/** Whether the server can restart itself to apply a staged restore (`--restart-exit-code`). */
export interface RestartInfo {
  available: boolean;
  exitCode: number | null;
}

/** `GET /api/admin/backups`. */
export interface BackupListing {
  target: string;
  backups: BackupInfo[];
  pendingRestore: PendingRestore | null;
  restart: RestartInfo;
  /** Continuous replication, or null when the server runs without `--replicate-to`. */
  replication: ReplicationStatus | null;
}

export interface ImportReport {
  /** `droppedFields`: fields the documents had that the table doesn't define (left out), by path. */
  tables: { table: string; documents: number; replaced: number; deleted: number; droppedFields: string[] }[];
  files: number;
  missingFiles: number;
  remappedIds: boolean;
}

export type ImportMode = "insert" | "upsert" | "replace";
export type ExportFormat = "jsonl" | "csv";

/** `POST /api/admin/impersonate`, without the tokens (the dashboard never keeps or shows them). */
export interface Impersonation {
  userId: string;
  sessionId: string;
  /** ms since the epoch. */
  expiresAt: number;
  /** The app URL with the tokens in its fragment (only with `redirectTo`). */
  url: string | null;
}

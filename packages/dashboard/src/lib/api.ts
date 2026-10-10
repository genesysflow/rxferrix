/**
 * Hand-written references to the admin-only system functions and the types they return (codegen leaves system
 * functions out of app APIs): `_system/dashboard:*` (crates/ferrix/src/dashboard/mod.rs), `_system/storage:*`,
 * `_system/scheduler:*`, `_system/workflow:*`, `_system/migrations:*`, `_system/settings:*` and `_system/audit:list`, plus the types of
 * the `/api/admin/*` endpoints (lib/admin.ts). See docs/CONTRACT.md §4.
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
  /**
   * The serving URL (`ctx.storage.get_url`), absolute when the server has a public URL. In private storage mode it is
   * a signed URL that already has a query string: add parameters with `withParams` (lib/format.ts).
   */
  url: string;
  /** `url` with `download=1` added correctly. */
  downloadUrl: string;
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
  /** A function path, `cron:<name>`, `dashboard`, `onComplete:<job id>` or `workflow:<id>`. */
  origin: string | null;
  /** The workpool whose parallelism limits this action; null for the default pool. */
  pool: string | null;
  /** The mutation run (as a job of its own) once this one has finished. */
  onComplete: { path: string; context: Json } | null;
  /** The retry backoff; null for the scheduler's. */
  backoff: { initialMs: number; base: number } | null;
}

export interface CronSummary {
  _id: string;
  name: string;
  schedule: string;
  /** The IANA time zone of the expression; null for UTC. */
  timezone: string | null;
  /** A ferrix system cron (name starting with `_`, e.g. `_storage_sweep`). */
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

/** The admin-only scheduler functions (`_system/scheduler:*`, crates/ferrix/src/scheduler/mod.rs). */
export const scheduler = {
  jobs: sq<{ state?: JobState | null; paginationOpts: PaginationOpts }, PaginationResult<ScheduledJob>>("jobs"),
  cancelJob: sm<{ id: string }, CancelOutcome>("cancel_job"),
  runJobNow: sm<{ id: string }, string>("run_job_now"),
  crons: sq<Record<string, never>, CronSummary[]>("crons"),
  /** The id of the enqueued job; user error `running` while the previous run is unfinished (unless `force`). */
  runCronNow: sm<{ name: string; force?: boolean | null }, string>("run_cron_now"),
} as const;

// ---------------------------------------------------------------------------------------------------------------------
// Workflows (`_system/workflow:*`, crates/ferrix/src/workflow)

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

/** The admin-only workflow functions (`_system/workflow:*`, crates/ferrix/src/workflow/mod.rs). */
export const workflows = {
  list: wq<{ state?: WorkflowState | null; paginationOpts: PaginationOpts }, PaginationResult<Workflow>>("list"),
  steps: wq<{ id: string }, WorkflowStep[]>("steps"),
  cancelWorkflow: wm<{ id: string }, CancelWorkflowOutcome>("cancel_workflow"),
} as const;

// ---------------------------------------------------------------------------------------------------------------------
// Migrations (`_system/migrations:*`, crates/ferrix/src/migrations)

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
// Settings (`_system/settings:*`, actions: crates/ferrix/src/settings)

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

export interface SettingsView {
  oauth: OAuthProviderView[];
  smtp: SmtpView | null;
  redirectUrls: string[] | null;
  signups: boolean | null;
  accessTokenTtlSecs: number | null;
  sessionTtlSecs: number | null;
  corsOrigins: string[] | null;
  /** Fields the configuration fixes: `signups`, `accessTokenTtlSecs`, `sessionTtlSecs`, `redirectUrls`, `smtp`, `oauth.<id>`. */
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
  };
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
  | "corsOrigins";

export interface SettingsUpdate {
  oauth?: OAuthProviderInput[];
  smtp?: SmtpInput;
  redirectUrls?: string[];
  signups?: boolean;
  accessTokenTtlSecs?: number;
  sessionTtlSecs?: number;
  corsOrigins?: string[];
  clear?: SettingsKey[];
}

export const settings = {
  get: makeFunctionReference<"action", Record<string, never>, SettingsView>("action", "_system/settings:get"),
  update: makeFunctionReference<"action", SettingsUpdate, SettingsView>("action", "_system/settings:update"),
  sendTestEmail: makeFunctionReference<"action", { to: string; smtp?: SmtpInput | null }, null>(
    "action",
    "_system/settings:send_test_email",
  ),
} as const;

// ---------------------------------------------------------------------------------------------------------------------
// Audit log (`_system/audit:list`, crates/ferrix/src/audit.rs)

export const AUDIT_ACTIONS = [
  "insert",
  "patch",
  "replace",
  "delete",
  "revoke_session",
  "delete_user",
  "settings_update",
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

/** `BackupInfo` (crates/ferrix/src/ops/backup.rs). */
export interface BackupInfo {
  name: string;
  bytes: number;
  createdAt: number;
  /** A `ferrix-auto-*` archive from `--backup-every` (only those are rotated). */
  scheduled: boolean;
  ts: number | null;
}

/** `PendingRestore` (crates/ferrix/src/ops/restore.rs): a backup staged to replace the database at the next start. */
export interface PendingRestore {
  /** The backup's name, or the uploaded file's. */
  source: string;
  stagedAt: number;
  stagedBy: string | null;
  backupCreatedAt: number;
  /** Commit ts of the backup's snapshot. */
  ts: number;
  ferrixVersion: string;
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

/** `PointInTimeInfo` (crates/ferrix/src/ops/replicate.rs): what a point-in-time restore restores. */
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

/** `ReplicationStatus` (crates/ferrix/src/ops/replicate.rs): `GET /api/admin/replication` (`--replicate-to`). */
export interface ReplicationStatus {
  target: string;
  everyMs: number;
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
  tables: { table: string; documents: number; replaced: number; deleted: number }[];
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

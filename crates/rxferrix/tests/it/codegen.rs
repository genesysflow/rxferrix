//! TypeScript codegen from the real registry of this test binary (hand-written expansions + real macros), including
//! user types that are only reachable through tables, function signatures and other user types.
//!
//! `codegen_typechecks_with_tsc` (ignored by default, needs `bun`) also type-checks the output with `tsc` against
//! `@rxferrix/client`, using `tests/ts/usage.ts`. Run it with:
//! `cargo test -p rxferrix --test it codegen_typechecks_with_tsc -- --ignored`.

use std::path::{Path, PathBuf};
use std::process::Command;

use rxferrix::codegen::{self, GeneratedFiles};

pub mod tasks {
    use std::collections::HashMap;

    use rxferrix::prelude::*;
    use serde::{Deserialize, Serialize};
    use ts_rs::TS;

    /// How urgent a task is.
    #[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq)]
    #[serde(rename_all = "lowercase")]
    #[ts(rename_all = "lowercase")]
    pub enum Priority {
        Low,
        High,
    }

    /// Only reachable through `Label`.
    #[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
    pub enum Color {
        Red,
        Green,
        Custom { hex: String },
    }

    #[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
    #[serde(rename_all = "camelCase")]
    #[ts(rename_all = "camelCase")]
    pub struct Label {
        pub name: String,
        pub color: Color,
    }

    /// Only used as the `stats` output: declared, and referred to by name.
    #[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
    #[serde(rename_all = "camelCase")]
    #[ts(rename_all = "camelCase")]
    pub struct Stats {
        pub by_priority: HashMap<String, Priority>,
        pub total: u64,
        pub top_label: Option<Label>,
    }

    /// A task as a list shows it (an `#[rxferrix::object]`, returned in a `Vec`).
    #[rxferrix::object]
    #[derive(Clone, Debug, PartialEq)]
    pub struct TaskSummary {
        /// The task's title.
        pub title: String,
        /// How many labels it has.
        ///
        /// Zero when unlabeled.
        pub label_count: u32,
        pub priority: Priority,
        /// Left out when there is none.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub note: Option<String>,
    }

    /// Only used by an internal function: must not be emitted.
    #[derive(Serialize, Deserialize, TS, Clone, Debug)]
    pub struct PurgeSecret {
        pub token: String,
    }

    #[table(name = "tasks")]
    #[index(by_priority = [priority])]
    pub struct Task {
        pub title: String,
        pub priority: Priority,
        pub labels: Vec<Label>,
        pub parent: Option<Id<Task>>,
    }

    #[query]
    pub async fn list(ctx: &QueryCtx, priority: Option<Priority>) -> Result<Vec<Doc<Task>>> {
        match priority {
            Some(p) => {
                ctx.db.query::<Task>().with_index(Task::by_priority, |q| q.eq(Task::priority, p)).collect().await
            }
            None => ctx.db.query::<Task>().collect().await,
        }
    }

    #[query]
    pub async fn stats(ctx: &QueryCtx) -> Result<Stats> {
        let all = ctx.db.query::<Task>().collect().await?;
        let by_priority = all.iter().map(|t| (t.title.clone(), t.priority)).collect();
        let top_label = all.iter().find_map(|t| t.labels.first().cloned());
        Ok(Stats { by_priority, total: all.len() as u64, top_label })
    }

    /// Labels, keyed by name (the values are inlined, so `Label` is referenced through its dependencies only).
    #[query]
    pub async fn labels(ctx: &QueryCtx) -> Result<HashMap<String, Vec<Label>>> {
        let mut out: HashMap<String, Vec<Label>> = HashMap::new();
        for t in ctx.db.query::<Task>().collect().await? {
            out.entry(t.value.title).or_default().extend(t.value.labels);
        }
        Ok(out)
    }

    /// Tasks by priority, one page at a time.
    #[query]
    pub async fn page(
        ctx: &QueryCtx,
        priority: Option<Priority>,
        pagination_opts: PaginationOpts,
    ) -> Result<PaginationResult<Doc<Task>>> {
        let q = match priority {
            Some(p) => ctx.db.query::<Task>().with_index(Task::by_priority, |q| q.eq(Task::priority, p)),
            None => ctx.db.query::<Task>(),
        };
        q.order(Order::Desc).paginate(pagination_opts).await
    }

    #[mutation]
    pub async fn add(
        ctx: &MutationCtx,
        title: String,
        priority: Priority,
        labels: Vec<Label>,
        parent: Option<Id<Task>>,
    ) -> Result<Id<Task>> {
        ctx.db.insert(Task { title, priority, labels, parent }).await
    }

    #[internal_mutation]
    pub async fn purge(_ctx: &MutationCtx, secret: PurgeSecret) -> Result<()> {
        let _ = secret.token;
        Ok(())
    }

    #[action]
    pub async fn ping(_ctx: &ActionCtx) -> Result<String> {
        Ok("pong".into())
    }

    #[query]
    pub async fn summaries(ctx: &QueryCtx) -> Result<Vec<TaskSummary>> {
        let all = ctx.db.query::<Task>().collect().await?;
        Ok(all
            .into_iter()
            .map(|t| TaskSummary {
                label_count: t.labels.len() as u32,
                priority: t.priority,
                title: t.value.title,
                note: None,
            })
            .collect())
    }

    /// The first summary, if any.
    #[query]
    pub async fn first_summary(ctx: &QueryCtx) -> Result<Option<TaskSummary>> {
        Ok(summaries(ctx).await?.into_iter().next())
    }

    /// Dev-only: generated into `devApi`, refused unless the app runs in dev mode.
    #[mutation(dev_only)]
    pub async fn wipe(ctx: &MutationCtx) -> Result<u32> {
        let all = ctx.db.query::<Task>().collect().await?;
        for t in &all {
            ctx.db.delete(t.id).await?;
        }
        Ok(all.len() as u32)
    }
}

fn generated() -> GeneratedFiles {
    codegen::generate(rxferrix::registry().unwrap(), &rxferrix::runtime::registered_tables()).unwrap()
}

#[test]
fn generates_from_the_real_registry() {
    let files = generated();
    let (dm, api) = (&files.data_model_ts, &files.api_ts);

    // dataModel.ts: header, the table map, helpers and every reachable user type, sorted, once.
    assert!(dm.starts_with("/* eslint-disable */\n// Generated by `rxferrix codegen`. Do not edit.\n"), "{dm}");
    assert!(dm.contains("import type { Id as GenericId } from \"@rxferrix/client\";\n"));
    assert!(
        dm.contains(
            "  \"tasks\": { title: string, priority: Priority, labels: Array<Label>, parent: Id<\"tasks\"> | null, };\n"
        ),
        "{dm}"
    );
    assert!(dm.contains("  \"todos\": { text: string, done: boolean, createdBy: string | null, };\n"), "{dm}");
    // One config everywhere: i64 is a number, not a bigint.
    assert!(dm.contains("  \"items\": { group: number, score: number, name: string, };\n"), "{dm}");
    assert!(dm.contains(
        "export type Doc<T extends TableNames | keyof SystemDataModel> = (DataModel & SystemDataModel)[T] & { _id: Id<T>; _creationTime: number };"
    ));
    // `_scheduled_jobs`, `_storage`, `_users` and `_workflows` may be referenced by app code; other system tables are
    // left out entirely. The job's enums are inlined, so no extra named types (that could clash with the app's) are
    // declared.
    assert!(
        dm.contains("export type SystemDataModel = {\n  \"_scheduled_jobs\": { path: string, kind: \"mutation\" | \"action\", args: any, runAt: number, state: \"pending\" | \"inProgress\" | \"success\" | \"failed\" | \"canceled\", attempts: number, maxAttempts: number, result: string | null, error: string | null, scheduledAt: number, startedAt: number | null, completedAt: number | null, origin: string | null, pool: string | null, onComplete: { path: string, context: any } | null, backoff: { initialMs: number, base: number } | null, };\n  \"_storage\": { sha256: string, size: number, contentType: string | null, createdBy: string | null, filename: string | null, };\n  \"_users\": { email: string | null, emailVerified: boolean, name: string | null, image: string | null, };\n  \"_workflows\": { name: string, args: any, state: \"running\" | \"success\" | \"failed\" | \"canceled\", result: any, error: string | null, steps: number, startedAt: number, completedAt: number | null, origin: string | null, onComplete: { path: string, context: any } | null, events: Array<{ name: string, value: any }>, };\n};\n"),
        "{dm}"
    );
    assert!(!dm.contains("JobState") && !dm.contains("_crons"), "{dm}");
    for internal in ["_sessions", "_credentials", "_admins", "_accounts", "passwordHash", "refreshTokenHash"] {
        assert!(!dm.contains(internal), "{internal} must not be emitted");
    }
    assert!(!api.contains("\"_system/"), "system functions are not emitted");
    // Function docs become the JSDoc of their reference.
    assert!(
        api.contains(
            "      /** Labels, keyed by name (the values are inlined, so `Label` is referenced through its dependencies only). */\n      labels: makeFunctionReference"
        ),
        "{api}"
    );
    assert!(dm.contains("/** How urgent a task is. */\nexport type Priority = \"low\" | \"high\";\n"), "{dm}");
    assert!(dm.contains("export type Color = \"Red\" | \"Green\" | { \"Custom\": { hex: string, } };\n"), "{dm}");
    assert!(dm.contains("export type Label = { name: string, color: Color, };\n"), "{dm}");
    // Pagination types: declared once, `PaginationResult` generically (it is referred to by name, see below).
    let opts_decl = dm.find("export type PaginationOpts = {").expect("PaginationOpts declared");
    let opts_decl = &dm[opts_decl..opts_decl + dm[opts_decl..].find("};").unwrap()];
    for field in ["numItems: number,", "cursor: string | null,", "endCursor?: string | null,"] {
        assert!(opts_decl.contains(field), "{field} in {opts_decl}");
    }
    let result_decl = dm.find("export type PaginationResult<T> = {").expect("PaginationResult declared generically");
    let result_decl = &dm[result_decl..result_decl + dm[result_decl..].find("};").unwrap()];
    for field in ["page: Array<T>,", "isDone: boolean,", "continueCursor: string,"] {
        assert!(result_decl.contains(field), "{field} in {result_decl}");
    }
    for t in ["PaginationOpts", "PaginationResult"] {
        assert_eq!(dm.matches(&format!("export type {t}")).count(), 1, "{t} declared once");
    }
    // A named output type is referred to by name and declared, also inside a `Vec` or an `Option`.
    assert!(
        dm.contains("export type Stats = { byPriority: { [key in string]: Priority }, total: number, topLabel: Label | null, };\n"),
        "{dm}"
    );
    // Field docs: JSDoc on their own lines, one field per line.
    assert!(
        dm.contains(
            "/** A task as a list shows it (an `#[rxferrix::object]`, returned in a `Vec`). */\nexport type TaskSummary = {\n  \
             /** The task's title. */\n  title: string,\n  /**\n   * How many labels it has.\n   *\n   * Zero when unlabeled.\n   \
             */\n  labelCount: number,\n  priority: Priority,\n  /** Left out when there is none. */\n  note?: string | null,\n};\n"
        ),
        "{dm}"
    );
    let order: Vec<usize> = ["type Color", "type Label", "type Priority", "type Stats", "type TaskSummary"]
        .iter()
        .map(|t| dm.find(t).unwrap())
        .collect();
    assert!(order.is_sorted(), "types are sorted by name");
    for t in ["Color", "Label", "Priority", "Stats", "TaskSummary"] {
        assert_eq!(dm.matches(&format!("export type {t} ")).count(), 1, "{t} declared once");
    }
    assert!(!dm.contains("PurgeSecret") && !api.contains("PurgeSecret"), "internal-only types are not emitted");
    assert!(!dm.contains("bigint") && !api.contains("bigint"));

    // api.ts
    assert!(api.contains("import { makeFunctionReference } from \"@rxferrix/client\";\n"));
    assert!(
        api.contains(
            "import type { AttachmentView, DevFlags, Doc, Hit, Id, JsonValue, Label, Mode, NoteView, PageHit, PaginationOpts, PaginationResult, Priority, ScanSpec, Stats, TaskSummary, VqHit } from \"./dataModel\";\n"
        ),
        "{api}"
    );
    let expect = [
        "  codegen: {\n    tasks: {\n",
        "      add: makeFunctionReference<\"mutation\", { title: string, priority: Priority, labels: Array<Label>, parent?: Id<\"tasks\"> | null, }, Id<\"tasks\">>(\"mutation\", \"codegen/tasks:add\"),\n",
        "      first_summary: makeFunctionReference<\"query\", Record<string, never>, TaskSummary | null>(\"query\", \"codegen/tasks:first_summary\"),\n",
        "      labels: makeFunctionReference<\"query\", Record<string, never>, { [key in string]: Array<Label> }>(\"query\", \"codegen/tasks:labels\"),\n",
        "      list: makeFunctionReference<\"query\", { priority?: Priority | null, }, Array<Doc<\"tasks\">>>(\"query\", \"codegen/tasks:list\"),\n",
        "      page: makeFunctionReference<\"query\", { priority?: Priority | null, paginationOpts: PaginationOpts, }, PaginationResult<Doc<\"tasks\">>>(\"query\", \"codegen/tasks:page\"),\n",
        "      ping: makeFunctionReference<\"action\", Record<string, never>, string>(\"action\", \"codegen/tasks:ping\"),\n",
        "      stats: makeFunctionReference<\"query\", Record<string, never>, Stats>(\"query\", \"codegen/tasks:stats\"),\n",
        "      summaries: makeFunctionReference<\"query\", Record<string, never>, Array<TaskSummary>>(\"query\", \"codegen/tasks:summaries\"),\n",
        "  common: {\n",
        "    todos: {\n      add: makeFunctionReference<\"mutation\", { text: string, }, Id<\"todos\">>(\"mutation\", \"common/todos:add\"),\n",
        "  macros: {\n    notes: {\n",
        "      on_board: makeFunctionReference<\"query\", { board: string, limit?: number | null, }, Array<Doc<\"notes\">>>(\"query\", \"macros/notes:on_board\"),\n",
    ];
    for e in expect {
        assert!(api.contains(e), "missing {e:?} in\n{api}");
    }
    // Internal functions are excluded.
    for internal in ["codegen/tasks:purge", "common/todos:count", "macros/notes:count"] {
        assert!(!api.contains(&format!("\"{internal}\"")), "{internal} must not be emitted");
    }
    assert!(!api.contains("symbol"), "zero-arg functions use Record<string, never>");
    assert!(api.ends_with("} as const;\n"));
    // Dev-only functions: in `devApi`, not in `api`.
    let (public, dev) = api.split_once("export const devApi = {\n").expect("devApi");
    assert!(!public.contains("codegen/tasks:wipe"), "{api}");
    assert!(
        dev.contains("      wipe: makeFunctionReference<\"mutation\", Record<string, never>, number>(\"mutation\", \"codegen/tasks:wipe\"),\n"),
        "{api}"
    );

    assert_eq!(
        files.index_ts,
        "/* eslint-disable */\n// Generated by `rxferrix codegen`. Do not edit.\nexport * from \"./api\";\nexport * from \"./dataModel\";\n"
    );

    // Deterministic.
    assert_eq!(files, generated());
}

#[test]
fn write_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let changed = codegen::write(dir.path()).unwrap();
    assert_eq!(changed.len(), 3);
    assert!(codegen::write(dir.path()).unwrap().is_empty(), "unchanged files are not rewritten");
    assert_eq!(std::fs::read_to_string(dir.path().join("api.ts")).unwrap(), generated().api_ts);
}

/// Type-checks the generated files with `tsc` against `@rxferrix/client` (its sources) using `tests/ts/usage.ts`,
/// which also has `@ts-expect-error` lines for wrong args. Needs `bun`; set `RXFERRIX_CODEGEN_KEEP=1` to keep the
/// temp dir for inspection.
#[test]
#[ignore = "needs bun; run with --ignored"]
fn codegen_typechecks_with_tsc() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let client = manifest.join("../../packages/client").canonicalize().expect("packages/client exists");
    // A temp dir inside packages/client, so that `typescript` and `../src` resolve; not matched by its tsconfig.
    let dir = tempfile::Builder::new().prefix(".codegen-check-").tempdir_in(&client).unwrap();
    let generated_dir = dir.path().join("generated");
    codegen::write_files(&generated_dir, &generated()).unwrap();
    std::fs::copy(manifest.join("tests/ts/usage.ts"), dir.path().join("usage.ts")).unwrap();
    std::fs::copy(manifest.join("tests/ts/tsconfig.json"), dir.path().join("tsconfig.json")).unwrap();

    let out = Command::new("bunx")
        .args(["tsc", "--noEmit", "-p"])
        .arg(dir.path().join("tsconfig.json"))
        .current_dir(&client)
        .output()
        .expect("failed to run bunx (is bun installed?)");
    let log = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let kept: Option<PathBuf> = std::env::var_os("RXFERRIX_CODEGEN_KEEP").map(|_| dir.keep());
    assert!(out.status.success(), "tsc failed (kept: {kept:?}):\n{log}");
}

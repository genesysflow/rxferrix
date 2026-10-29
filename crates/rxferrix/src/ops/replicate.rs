//! Continuous replication and point-in-time restores.
//!
//! RxFerrix serializes every commit through one writer, so replication is a **logical change journal** rather than a
//! copy of SQLite pages:
//!
//! - **Journal** (`crate::db::journal`): with `serve --replicate-to <location>`, every commit also inserts its
//!   document changes (`ts`, `time`, `[table, id, document | null]…`) into the raw table `_journal`, in the same SQLite
//!   transaction as the writes. Nothing committed is missing from it and nothing rolled back is in it.
//! - **Generations.** Everything replicated from one continuous history of the database lives under
//!   `<target>/generations/<generation>/`, where `<generation>` is `<UTC time>-<8 hex digits>` (sortable by creation).
//!   A generation starts with a snapshot; a new one begins whenever the journal can't continue the previous one: the
//!   first start, a restored database (restores remove the replication state, `journal::scrub`), commits made without
//!   journaling (e.g. `rxferrix import` offline), a target that already holds more than this database (a copy of
//!   `data.db` put in place by hand), or a journal dropped because the target was unreachable for too long.
//! - **Snapshots**: `generations/<g>/snapshots/rxferrix-auto-<time>-ts<N>.tar.zst`, ordinary backup archives
//!   (`super::backup`, without `aux.db` and blobs) taken when a generation starts and then every `--snapshot-every`.
//! - **Segments**: every `--replicate-every`, the unshipped journal rows are packed into
//!   `generations/<g>/journal/<from ts>-<to ts>.zst` (ts zero-padded to 20 digits; at most
//!   [`ReplicationOptions::segment_max_entries`] commits or [`ReplicationOptions::segment_max_bytes`] each): zstd of a
//!   JSON header line (`SegmentHeader`: format, version, generation, ts and time range, entry count, SHA-256 of the
//!   body) and one `<ts>\t<time>\t<changes>` line per commit. Shipped rows are deleted locally only after the upload
//!   succeeded (with the shipped ts recorded in `_meta` in the same transaction); an upload that is retried after a
//!   crash may overlap an earlier segment, which restores tolerate.
//! - **Scheduler bookkeeping waits** (`--replicate-jobs-every`, 60s): while every unshipped commit only records the
//!   scheduler's progress (`JournalEntry::is_scheduler_bookkeeping`: cron runs, jobs starting and finishing, finished
//!   jobs moving to the job history), shipping waits until the oldest of them is that old, so a busy scheduler on an
//!   otherwise idle app doesn't upload a segment every second. Any other commit ships within `--replicate-every`,
//!   with everything before it (segments are always a prefix). The cost: a point-in-time restore of the newest commit
//!   after losing the server may miss up to that long of job progress (such jobs run again, or are marked failed).
//! - **Retention** (`--replicate-keep`): every snapshot taken within the window is kept, plus the newest one before it
//!   (the base for the start of the window), and the segments after the oldest kept snapshot of each generation;
//!   everything else, whole generations included, is deleted ([`plan_retention`]).
//! - **Failure**: while the target is unreachable the journal keeps growing locally and shipping resumes where it
//!   stopped once it is back. Past [`ReplicationOptions::max_pending_bytes`] (`--replicate-max-pending`) the journal
//!   is dropped and journaling paused, with an error in the log; the next successful contact starts a new
//!   generation. The lag (unshipped commits, age of the oldest unshipped one) is in `GET /api/admin/replication`,
//!   `GET /api/admin/backups` and `/metrics`.
//!
//! **Point-in-time restore** ([`prepare_point_in_time`], `rxferrix restore --from <location> [--to-ts N | --to-time T]`,
//! `POST /api/admin/restore/point-in-time`): pick the generation (the newest, the one live at `--to-time`, or
//! `--generation`), its newest snapshot at or before the target, restore it like any archive, then replay the journal
//! entries after it up to the target through the write path ([`crate::db::Database::replay`]: one commit per entry,
//! with its original ts and time, so indexes, full-text and vector entries are rebuilt). The result is checked like
//! every restore (schema, `verify_indexes`) and its ts must be the target's (or the newest available when the
//! target is beyond it). Blobs of stored files are not journaled: keep files in S3 (`--storage s3://…`) or back them
//! up separately.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use futures::StreamExt;
use object_store::{ObjectStoreExt, PutPayload};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::backup::{self, BackupSpec, BackupTarget, Manifest};
use crate::UserError;
use crate::db::journal::{self, JournalEntry};
use crate::db::storage;
use crate::runtime::{App, WeakApp};

/// The directory of generations in a replication target.
pub const GENERATIONS: &str = "generations";
/// `SegmentHeader::format`.
pub const SEGMENT_FORMAT: &str = "rxferrix-journal";
/// `SegmentHeader::version`.
pub const SEGMENT_VERSION: u32 = 1;
const SEGMENT_EXTENSION: &str = ".zst";

/// Default `--replicate-every`.
pub const DEFAULT_EVERY: Duration = Duration::from_secs(1);
/// Default `--replicate-jobs-every`.
pub const DEFAULT_JOBS_EVERY: Duration = Duration::from_secs(60);
/// Default `--snapshot-every`.
pub const DEFAULT_SNAPSHOT_EVERY: Duration = Duration::from_secs(24 * 3600);
/// Default `--replicate-keep`.
pub const DEFAULT_KEEP: Duration = Duration::from_secs(7 * 24 * 3600);
/// Default `--replicate-max-pending`.
pub const DEFAULT_MAX_PENDING_BYTES: u64 = 1 << 30;
/// Longest wait between attempts while the target fails.
const MAX_BACKOFF: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------------------------------------------------

/// Replication settings (`AppOptions::ops.replication`; `serve --replicate-to …`).
#[derive(Debug, Clone)]
pub struct ReplicationOptions {
    /// A directory, `file:///dir` or `s3://bucket/prefix`.
    pub target: String,
    /// Use this target instead of parsing [`Self::target`] (tests, custom object stores).
    pub store: Option<BackupTarget>,
    /// How often unshipped commits are shipped (`--replicate-every`, default 1s).
    pub every: Duration,
    /// How long commits that only record the scheduler's progress may wait while nothing else is unshipped
    /// (`--replicate-jobs-every`, default 60s; zero ships them like any other commit). See the module docs.
    pub jobs_every: Duration,
    /// How often a snapshot is taken (`--snapshot-every`, default 24h).
    pub snapshot_every: Duration,
    /// How far back point-in-time restores must reach (`--replicate-keep`, default 7 days).
    pub keep: Duration,
    /// Journal bytes kept locally while the target is unreachable before the journal is dropped and a new
    /// generation is started on recovery (`--replicate-max-pending`, default 1 GiB).
    pub max_pending_bytes: u64,
    /// Commits per segment at most.
    pub segment_max_entries: usize,
    /// Uncompressed bytes of changes per segment (approximately; a single larger commit gets a segment of its own).
    pub segment_max_bytes: usize,
    /// Run the background loop that ships every [`Self::every`] and snapshots every [`Self::snapshot_every`] (default
    /// `true`). With `false` nothing happens on its own: the embedder (or a test) drives replication through
    /// [`Replicator::sync_now`] / [`Replicator::snapshot_now`], and journaling still records every commit.
    pub background: bool,
}

impl ReplicationOptions {
    /// Replicate to `target` (a directory, `file:///dir` or `s3://bucket/prefix`) with the default intervals.
    pub fn new(target: impl Into<String>) -> Self {
        ReplicationOptions {
            target: target.into(),
            store: None,
            every: DEFAULT_EVERY,
            jobs_every: DEFAULT_JOBS_EVERY,
            snapshot_every: DEFAULT_SNAPSHOT_EVERY,
            keep: DEFAULT_KEEP,
            max_pending_bytes: DEFAULT_MAX_PENDING_BYTES,
            segment_max_entries: 10_000,
            segment_max_bytes: 16 << 20,
            background: true,
        }
    }

    /// Replicate to an already built target (any object store).
    pub fn with_target(target: BackupTarget) -> Self {
        ReplicationOptions { store: Some(target.clone()), ..Self::new(target.location) }
    }

    /// The target.
    pub fn build_target(&self, data_dir: &Path) -> crate::Result<BackupTarget> {
        match &self.store {
            Some(t) => Ok(t.clone()),
            None => BackupTarget::parse(&self.target, data_dir),
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Target layout and listing
// ---------------------------------------------------------------------------------------------------------------------

/// `<UTC time>-<8 hex digits>`.
fn new_generation_id(now_ms: f64) -> String {
    format!("{}-{:08x}", super::file_timestamp(now_ms), rand::random::<u32>())
}

/// When a generation was created, from its id.
pub fn generation_created_at(id: &str) -> Option<f64> {
    let (time, suffix) = id.rsplit_once('-')?;
    (suffix.len() == 8 && suffix.chars().all(|c| c.is_ascii_hexdigit())).then_some(())?;
    super::parse_file_timestamp(time)
}

/// The segment file name of `[from, to]`.
pub fn segment_name(from: u64, to: u64) -> String {
    format!("{from:020}-{to:020}{SEGMENT_EXTENSION}")
}

fn parse_segment_name(name: &str) -> Option<(u64, u64)> {
    let (from, to) = name.strip_suffix(SEGMENT_EXTENSION)?.split_once('-')?;
    let (from, to): (u64, u64) = (from.parse().ok()?, to.parse().ok()?);
    (from <= to && from > 0).then_some((from, to))
}

fn segment_rel(generation: &str, from: u64, to: u64) -> String {
    format!("{GENERATIONS}/{generation}/journal/{}", segment_name(from, to))
}

/// The snapshots of `generation`, as a backup target.
pub fn snapshots_target(target: &BackupTarget, generation: &str) -> BackupTarget {
    target.child(&format!("{GENERATIONS}/{generation}/snapshots"))
}

/// A snapshot of a generation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotInfo {
    pub name: String,
    pub ts: u64,
    /// When it was taken (ms, from its name).
    pub created_at: f64,
    pub bytes: u64,
}

/// A journal segment of a generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SegmentInfo {
    pub from: u64,
    pub to: u64,
    pub bytes: u64,
}

/// One generation in a target: its snapshots (oldest first) and segments (by `from`, then `to`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Generation {
    pub id: String,
    pub created_at: f64,
    pub snapshots: Vec<SnapshotInfo>,
    pub segments: Vec<SegmentInfo>,
}

impl Generation {
    fn sort(&mut self) {
        self.snapshots.sort_by(|a, b| (a.ts, &a.name).cmp(&(b.ts, &b.name)));
        self.segments.sort_by_key(|s| (s.from, s.to));
    }

    /// The oldest restorable ts (its oldest snapshot's).
    pub fn oldest_ts(&self) -> Option<u64> {
        self.snapshots.first().map(|s| s.ts)
    }

    /// The newest restorable ts: the newest snapshot, extended by the segments that follow it without a gap.
    pub fn newest_ts(&self) -> Option<u64> {
        let newest = self.snapshots.iter().map(|s| s.ts).max()?;
        Some(chain(&self.segments, newest, None).1)
    }
}

/// The segments that continue `from` without a gap (each the one reaching furthest), up to `until` if given, and
/// the last ts they reach.
fn chain(segments: &[SegmentInfo], from: u64, until: Option<u64>) -> (Vec<SegmentInfo>, u64) {
    let mut out = Vec::new();
    let mut cur = from;
    while until.is_none_or(|u| cur < u) {
        let Some(next) = segments.iter().filter(|s| s.from <= cur + 1 && s.to > cur).max_by_key(|s| s.to) else {
            break;
        };
        out.push(*next);
        cur = next.to;
    }
    (out, cur)
}

/// Every generation in `target`, oldest first.
pub async fn list_generations(target: &BackupTarget) -> crate::Result<Vec<Generation>> {
    let (store, root) = target.raw(GENERATIONS);
    let mut listing = store.list(Some(&root));
    let mut gens: BTreeMap<String, Generation> = BTreeMap::new();
    while let Some(meta) = listing.next().await {
        let meta = meta.context("listing the replication target")?;
        let Some(parts) = meta.location.prefix_match(&root) else { continue };
        let parts: Vec<String> = parts.map(|p| p.as_ref().to_string()).collect();
        let [generation, kind, name] = parts.as_slice() else { continue };
        let Some(created_at) = generation_created_at(generation) else { continue };
        let entry = gens.entry(generation.clone()).or_insert_with(|| Generation {
            id: generation.clone(),
            created_at,
            snapshots: Vec::new(),
            segments: Vec::new(),
        });
        match kind.as_str() {
            "snapshots" => {
                let (Some(ts), true) = (backup::ts_from_name(name), backup::validate_name(name).is_ok()) else {
                    continue;
                };
                let created_at = snapshot_time(name).unwrap_or(created_at);
                entry.snapshots.push(SnapshotInfo { name: name.clone(), ts, created_at, bytes: meta.size });
            }
            "journal" => {
                if let Some((from, to)) = parse_segment_name(name) {
                    entry.segments.push(SegmentInfo { from, to, bytes: meta.size });
                }
            }
            _ => {}
        }
    }
    let mut out: Vec<Generation> = gens.into_values().collect();
    out.iter_mut().for_each(Generation::sort);
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

/// The time in a snapshot name (`rxferrix-auto-<time>-ts<N>.tar.zst`).
fn snapshot_time(name: &str) -> Option<f64> {
    let rest = name.strip_prefix("rxferrix-auto-").or_else(|| name.strip_prefix(backup::NAME_PREFIX))?;
    super::parse_file_timestamp(rest.get(..20)?)
}

async fn put(target: &BackupTarget, rel: &str, bytes: Vec<u8>) -> crate::Result<()> {
    let (store, path) = target.raw(rel);
    store
        .put(&path, PutPayload::from(bytes))
        .await
        .with_context(|| format!("uploading {rel} to {}", target.location))?;
    Ok(())
}

async fn get(target: &BackupTarget, rel: &str) -> crate::Result<Vec<u8>> {
    let (store, path) = target.raw(rel);
    let result = store.get(&path).await.with_context(|| format!("reading {rel} from {}", target.location))?;
    Ok(result.bytes().await.with_context(|| format!("reading {rel} from {}", target.location))?.to_vec())
}

async fn delete(target: &BackupTarget, rel: &str) -> crate::Result<()> {
    let (store, path) = target.raw(rel);
    match store.delete(&path).await {
        Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
        Err(e) => Err(e).with_context(|| format!("deleting {rel} from {}", target.location)),
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Segments
// ---------------------------------------------------------------------------------------------------------------------

/// The first line of a segment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SegmentHeader {
    pub format: String,
    pub version: u32,
    pub generation: String,
    pub from: u64,
    pub to: u64,
    pub from_time: f64,
    pub to_time: f64,
    pub entries: u64,
    /// Bytes of the body (everything after the header line), and its SHA-256.
    pub bytes: u64,
    pub sha256: String,
}

/// Encode consecutive journal entries as a segment (see the module docs).
pub fn encode_segment(generation: &str, entries: &[JournalEntry]) -> crate::Result<Vec<u8>> {
    let (Some(first), Some(last)) = (entries.first(), entries.last()) else { bail!("an empty segment") };
    let mut body = String::with_capacity(entries.iter().map(|e| e.changes.len() + 40).sum());
    for (i, e) in entries.iter().enumerate() {
        anyhow::ensure!(e.ts == first.ts + i as u64, "journal entries are not consecutive at ts {}", e.ts);
        body.push_str(&format!("{}\t{}\t", e.ts, e.time));
        body.push_str(&e.changes);
        body.push('\n');
    }
    let header = SegmentHeader {
        format: SEGMENT_FORMAT.into(),
        version: SEGMENT_VERSION,
        generation: generation.into(),
        from: first.ts,
        to: last.ts,
        from_time: first.time,
        to_time: last.time,
        entries: entries.len() as u64,
        bytes: body.len() as u64,
        sha256: data_encoding::HEXLOWER.encode(&Sha256::digest(body.as_bytes())),
    };
    let mut raw = serde_json::to_vec(&header)?;
    raw.push(b'\n');
    raw.extend_from_slice(body.as_bytes());
    Ok(zstd::encode_all(raw.as_slice(), 3)?)
}

/// Decode and check a segment: the header, the body's size and checksum, consecutive entries matching the header's
/// range, and (if given) the generation.
pub fn decode_segment(bytes: &[u8], generation: Option<&str>) -> crate::Result<(SegmentHeader, Vec<JournalEntry>)> {
    let raw = zstd::decode_all(bytes).context("not a zstd segment")?;
    let split = raw.iter().position(|b| *b == b'\n').context("a segment without a header")?;
    let header: SegmentHeader = serde_json::from_slice(&raw[..split]).context("invalid segment header")?;
    if header.format != SEGMENT_FORMAT {
        bail!("not a rxferrix journal segment (format `{}`)", header.format);
    }
    if header.version > SEGMENT_VERSION {
        bail!("journal segment version {} is newer than this binary supports ({SEGMENT_VERSION})", header.version);
    }
    if let Some(g) = generation.filter(|g| *g != header.generation) {
        bail!("the segment belongs to generation {}, not {g}", header.generation);
    }
    let body = &raw[split + 1..];
    if body.len() as u64 != header.bytes || data_encoding::HEXLOWER.encode(&Sha256::digest(body)) != header.sha256 {
        bail!("segment {} does not match its checksum: it is corrupt", segment_name(header.from, header.to));
    }
    let body = std::str::from_utf8(body).context("segment body is not UTF-8")?;
    let mut entries = Vec::with_capacity(header.entries as usize);
    for line in body.lines() {
        let mut parts = line.splitn(3, '\t');
        let (Some(ts), Some(time), Some(changes)) = (parts.next(), parts.next(), parts.next()) else {
            bail!("malformed segment line");
        };
        let entry = JournalEntry { ts: ts.parse()?, time: time.parse()?, changes: changes.to_string() };
        anyhow::ensure!(
            entry.ts == header.from + entries.len() as u64,
            "segment entries are not consecutive at ts {}",
            entry.ts
        );
        entries.push(entry);
    }
    anyhow::ensure!(
        entries.len() as u64 == header.entries && entries.last().map(|e| e.ts) == Some(header.to),
        "segment {} is incomplete",
        segment_name(header.from, header.to)
    );
    Ok((header, entries))
}

/// Download and check one segment of `generation`.
pub async fn read_segment(
    target: &BackupTarget,
    generation: &str,
    from: u64,
    to: u64,
) -> crate::Result<(SegmentHeader, Vec<JournalEntry>)> {
    let raw = get(target, &segment_rel(generation, from, to)).await?;
    let (header, entries) = decode_segment(&raw, Some(generation))?;
    anyhow::ensure!(
        (header.from, header.to) == (from, to),
        "segment {} holds ts {}..{}",
        segment_name(from, to),
        header.from,
        header.to
    );
    Ok((header, entries))
}

// ---------------------------------------------------------------------------------------------------------------------
// Retention
// ---------------------------------------------------------------------------------------------------------------------

/// What [`plan_retention`] deletes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetentionPlan {
    /// Generations deleted entirely.
    pub generations: Vec<String>,
    /// `(generation, snapshot name)`.
    pub snapshots: Vec<(String, String)>,
    /// `(generation, from, to)`.
    pub segments: Vec<(String, u64, u64)>,
}

impl RetentionPlan {
    pub fn is_empty(&self) -> bool {
        self.generations.is_empty() && self.snapshots.is_empty() && self.segments.is_empty()
    }
}

/// Keep every snapshot taken at or after `now - keep`, plus the newest one before that (across generations: it is
/// the base of the window's start), and in each generation the segments after its oldest kept snapshot (a segment
/// is dropped only when its last entry is at or before that snapshot's ts). A generation with no kept snapshot goes
/// entirely.
pub fn plan_retention(generations: &[Generation], now_ms: f64, keep: Duration) -> RetentionPlan {
    let cutoff = now_ms - keep.as_secs_f64() * 1000.0;
    let mut all: Vec<(&str, &SnapshotInfo)> =
        generations.iter().flat_map(|g| g.snapshots.iter().map(move |s| (g.id.as_str(), s))).collect();
    all.sort_by(|a, b| a.1.created_at.total_cmp(&b.1.created_at).then(a.0.cmp(b.0)).then(a.1.ts.cmp(&b.1.ts)));
    let base = all.iter().rev().find(|(_, s)| s.created_at < cutoff).map(|(g, s)| (*g, s.name.as_str()));
    let kept = |g: &str, s: &SnapshotInfo| s.created_at >= cutoff || base == Some((g, s.name.as_str()));
    let mut plan = RetentionPlan::default();
    for g in generations {
        let oldest_kept = g.snapshots.iter().filter(|s| kept(&g.id, s)).map(|s| s.ts).min();
        let Some(oldest_kept) = oldest_kept else {
            plan.generations.push(g.id.clone());
            continue;
        };
        for s in g.snapshots.iter().filter(|s| !kept(&g.id, s)) {
            plan.snapshots.push((g.id.clone(), s.name.clone()));
        }
        for seg in g.segments.iter().filter(|seg| seg.to <= oldest_kept) {
            plan.segments.push((g.id.clone(), seg.from, seg.to));
        }
    }
    plan
}

/// Delete what `plan` lists from `target`.
pub async fn apply_retention(
    target: &BackupTarget,
    generations: &[Generation],
    plan: &RetentionPlan,
) -> crate::Result<()> {
    for id in &plan.generations {
        let Some(g) = generations.iter().find(|g| &g.id == id) else { continue };
        // Snapshots first: a generation without snapshots is ignored by restores even if segments remain.
        for s in &g.snapshots {
            snapshots_target(target, id).delete(&s.name).await?;
        }
        for seg in &g.segments {
            delete(target, &segment_rel(id, seg.from, seg.to)).await?;
        }
    }
    for (g, name) in &plan.snapshots {
        snapshots_target(target, g).delete(name).await?;
    }
    for (g, from, to) in &plan.segments {
        delete(target, &segment_rel(g, *from, *to)).await?;
    }
    Ok(())
}

fn without(generations: &mut Vec<Generation>, plan: &RetentionPlan) {
    generations.retain(|g| !plan.generations.contains(&g.id));
    for g in generations.iter_mut() {
        g.snapshots.retain(|s| !plan.snapshots.iter().any(|(id, n)| id == &g.id && n == &s.name));
        g.segments.retain(|s| !plan.segments.iter().any(|(id, f, t)| id == &g.id && *f == s.from && *t == s.to));
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// The replicator
// ---------------------------------------------------------------------------------------------------------------------

/// The last shipped segment.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShippedSegment {
    pub name: String,
    pub from: u64,
    pub to: u64,
    pub entries: u64,
    pub bytes: u64,
    pub shipped_at: f64,
}

/// A generation as the status reports it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationSummary {
    pub id: String,
    pub created_at: f64,
    pub current: bool,
    /// Oldest first.
    pub snapshots: Vec<SnapshotInfo>,
    pub segments: usize,
    /// The restorable range: from the oldest snapshot's ts to the newest shipped ts.
    pub from_ts: Option<u64>,
    pub to_ts: Option<u64>,
    /// When the oldest snapshot was taken, and the commit time of `to_ts` (known for the current generation).
    pub from_time: Option<f64>,
    pub to_time: Option<f64>,
}

/// `GET /api/admin/replication`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplicationStatus {
    pub target: String,
    pub every_ms: f64,
    /// How long commits of scheduler bookkeeping alone may wait (`ReplicationOptions::jobs_every`).
    pub jobs_every_ms: f64,
    pub snapshot_every_ms: f64,
    pub keep_ms: f64,
    /// The generation being shipped (`None` until the first contact with the target, or after the journal was
    /// dropped).
    pub generation: Option<String>,
    /// The latest commit, and the newest commit stored in the target.
    pub current_ts: u64,
    pub current_time: f64,
    pub shipped_ts: u64,
    pub shipped_time: f64,
    /// Commits not in the target yet, and how long ago the oldest of them committed (0 when caught up).
    pub lag_commits: u64,
    pub lag_ms: f64,
    /// Journal bytes kept locally (unshipped), and the bound.
    pub pending_bytes: u64,
    pub max_pending_bytes: u64,
    /// Commits are being journaled (false after the journal was dropped, until the target is reachable again).
    pub journaling: bool,
    pub last_segment: Option<ShippedSegment>,
    pub last_snapshot: Option<SnapshotInfo>,
    pub next_snapshot_at: Option<f64>,
    /// Since the server started.
    pub segments_shipped: u64,
    pub snapshots_taken: u64,
    pub last_success_at: Option<f64>,
    pub last_error: Option<String>,
    pub last_error_at: Option<f64>,
    /// Consecutive failed attempts.
    pub failures: u32,
    /// Every generation in the target, oldest first (as of the last listing, kept up to date while shipping).
    pub generations: Vec<GenerationSummary>,
}

#[derive(Debug, Default)]
struct State {
    generation: Option<String>,
    shipped_ts: u64,
    shipped_time: f64,
    last_segment: Option<ShippedSegment>,
    last_snapshot: Option<SnapshotInfo>,
    /// The target's generations, kept up to date while shipping (summaries: a week of segments is a long list).
    generations: Vec<GenerationSummary>,
    segments_shipped: u64,
    snapshots_taken: u64,
    last_success_at: Option<f64>,
    last_error: Option<String>,
    last_error_at: Option<f64>,
    failures: u32,
    last_warned_at: f64,
    /// Background snapshots that failed in a row, and when the last one did: the next waits (`snapshot_due`).
    snapshot_failures: u32,
    snapshot_failed_at: Option<f64>,
}

/// The background replicator of an app (`App::replication`). Started by `App::open` when
/// `AppOptions::ops.replication` is set.
pub struct Replicator {
    options: ReplicationOptions,
    target: BackupTarget,
    state: Mutex<State>,
    /// One step (ship, retention, a snapshot it takes inline) at a time. Taken before `snapshotting` when both are.
    running: tokio::sync::Mutex<()>,
    /// One snapshot at a time. The background loop takes them next to shipping, not inside a step: copying and
    /// uploading a large database takes minutes, and commits keep shipping meanwhile.
    snapshotting: Arc<tokio::sync::Mutex<()>>,
    wake: tokio::sync::Notify,
}

impl std::fmt::Debug for Replicator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Replicator").field("target", &self.target.location).finish()
    }
}

/// Start replicating `app` if its options ask for it. Called once by `App::open`.
pub(crate) fn start(app: &App, weak: WeakApp) -> crate::Result<Option<Arc<Replicator>>> {
    let Some(options) = app.options().ops.replication.clone() else { return Ok(None) };
    let target = options.build_target(app.dir()).context("--replicate-to")?;
    tracing::info!(target = %target.location, every = ?options.every, snapshot_every = ?options.snapshot_every, "replication on");
    let replicator = Arc::new(Replicator {
        options,
        target,
        state: Mutex::new(State::default()),
        running: tokio::sync::Mutex::new(()),
        snapshotting: Arc::new(tokio::sync::Mutex::new(())),
        wake: tokio::sync::Notify::new(),
    });
    if replicator.options.background {
        tokio::spawn(run(replicator.clone(), weak));
    }
    Ok(Some(replicator))
}

async fn run(replicator: Arc<Replicator>, weak: WeakApp) {
    let mut delay = Duration::ZERO;
    loop {
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = replicator.wake.notified() => {}
        }
        // The app is upgraded only once the step may run, so a step never keeps a dropped app's database open while
        // it waits for another one.
        let one = replicator.running.lock().await;
        let Some(app) = weak.upgrade() else { return };
        let ok = replicator.sync_locked(&app, false).await.is_ok();
        drop(one);
        // A due snapshot is taken beside the steps (unless one is running already): shipping goes on meanwhile, and
        // its failures don't slow shipping down.
        if ok
            && replicator.snapshot_due(&app)
            && let Ok(guard) = replicator.snapshotting.clone().try_lock_owned()
        {
            let (replicator, app) = (replicator.clone(), app.clone());
            tokio::spawn(async move {
                replicator.background_snapshot(&app).await;
                drop(guard);
            });
        }
        drop(app);
        let every = replicator.options.every.max(Duration::from_millis(10));
        delay = if ok {
            every
        } else {
            let failures = replicator.state.lock().unwrap_or_else(|e| e.into_inner()).failures.min(16);
            (every * 2u32.saturating_pow(failures)).min(MAX_BACKOFF.max(every))
        };
    }
}

fn summarize(g: &Generation) -> GenerationSummary {
    GenerationSummary {
        id: g.id.clone(),
        created_at: g.created_at,
        current: false,
        snapshots: g.snapshots.clone(),
        segments: g.segments.len(),
        from_ts: g.oldest_ts(),
        to_ts: g.newest_ts(),
        from_time: g.snapshots.first().map(|s| s.created_at),
        to_time: None,
    }
}

fn meta_text(conn: &Connection, key: &str) -> anyhow::Result<Option<String>> {
    Ok(conn
        .query_row("SELECT v FROM _meta WHERE k = ?1", [key], |r| r.get::<_, Option<String>>(0))
        .optional()?
        .flatten())
}

fn set_shipped(conn: &Connection, generation: &str, ts: u64, time: f64) -> anyhow::Result<()> {
    storage::write_meta(conn, journal::META_GENERATION, generation.to_string().into())?;
    storage::write_meta(conn, journal::META_SHIPPED_TS, (ts as i64).into())?;
    storage::write_meta(conn, journal::META_SHIPPED_TIME, time.into())
}

/// What `begin` found locally and decided.
enum Resume {
    Continue { generation: String, shipped_ts: u64, shipped_time: f64, pending_bytes: u64 },
    NewGeneration(String),
}

impl Replicator {
    pub fn options(&self) -> &ReplicationOptions {
        &self.options
    }

    pub fn target(&self) -> &BackupTarget {
        &self.target
    }

    /// Run the next step now instead of waiting for the interval.
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// One step, as the background loop runs it every `every`: start (or resume) the generation if needed, ship
    /// every unshipped commit, and take a snapshot (then apply retention) when one is due. Failures are recorded in
    /// the status; past `max_pending_bytes` the journal is dropped (see the module docs).
    pub async fn sync_now(&self, app: &App) -> crate::Result<()> {
        let _one = self.running.lock().await;
        self.sync_locked(app, true).await
    }

    /// A step, with a due snapshot taken inline (`sync_now`) or left to the caller (the background loop).
    async fn sync_locked(&self, app: &App, inline_snapshot: bool) -> crate::Result<()> {
        let result = self.step(app, inline_snapshot).await;
        let now = app.db().clock().now_ms();
        match &result {
            Ok(()) => {
                let mut st = self.state();
                if st.failures > 0 {
                    tracing::info!(target = %self.target.location, "replication: the target is reachable again");
                }
                st.failures = 0;
                st.last_error = None;
                st.last_success_at = Some(now);
            }
            Err(e) => {
                let pending = app.db().journal().pending_bytes();
                let warn = {
                    let mut st = self.state();
                    st.failures += 1;
                    st.last_error = Some(format!("{e:#}"));
                    st.last_error_at = Some(now);
                    let warn = st.failures == 1 || now - st.last_warned_at >= 60_000.0;
                    if warn {
                        st.last_warned_at = now;
                    }
                    warn
                };
                if warn {
                    tracing::warn!(
                        target = %self.target.location,
                        pending_bytes = pending,
                        "replication failed (the journal keeps growing locally and shipping resumes when the target is back): {e:#}"
                    );
                }
                if pending > self.options.max_pending_bytes
                    && let Err(e) = self.drop_journal(app).await
                {
                    tracing::error!("replication: dropping the journal failed: {e:#}");
                }
            }
        }
        result
    }

    async fn step(&self, app: &App, inline_snapshot: bool) -> crate::Result<()> {
        if self.state().generation.is_none() {
            self.begin(app).await?;
        }
        let snapshot_due = self.snapshot_due(app);
        self.ship(app, !snapshot_due).await?;
        if snapshot_due && inline_snapshot {
            let _one = self.snapshotting.lock().await;
            self.snapshot(app).await?;
        }
        Ok(())
    }

    /// Take a due snapshot outside the steps (the background loop). A failure is logged and delays the next attempt
    /// (a minute, doubling up to `snapshot_every`); shipping isn't affected.
    async fn background_snapshot(&self, app: &App) {
        match self.snapshot(app).await {
            Ok(_) => {
                let mut st = self.state();
                (st.snapshot_failures, st.snapshot_failed_at) = (0, None);
            }
            Err(e) => {
                let failures = {
                    let mut st = self.state();
                    st.snapshot_failures += 1;
                    st.snapshot_failed_at = Some(app.db().clock().now_ms());
                    st.snapshot_failures
                };
                tracing::warn!(
                    target = %self.target.location,
                    failures,
                    "replication: taking a snapshot failed (commits keep shipping; retrying later): {e:#}"
                );
            }
        }
    }

    /// Ship what is unshipped without taking snapshots (graceful shutdown).
    pub async fn flush(&self, app: &App) -> crate::Result<()> {
        let _one = self.running.lock().await;
        if self.state().generation.is_some() { self.ship(app, false).await } else { Ok(()) }
    }

    /// Take a snapshot now (in the current generation, starting one if needed), then apply retention. Shipping goes
    /// on while it is taken.
    pub async fn snapshot_now(&self, app: &App) -> crate::Result<SnapshotInfo> {
        // `running` before `snapshotting`, as in a step that snapshots inline: the other order could deadlock with it.
        let one = self.running.lock().await;
        let _snapshot = self.snapshotting.lock().await;
        if self.state().generation.is_none() {
            self.begin(app).await?;
        }
        self.ship(app, false).await?;
        drop(one);
        self.snapshot(app).await
    }

    fn snapshot_due(&self, app: &App) -> bool {
        let st = self.state();
        let now = app.db().clock().now_ms();
        let every_ms = self.options.snapshot_every.as_secs_f64() * 1000.0;
        if let Some(failed_at) = st.snapshot_failed_at {
            let wait = (60_000.0 * 2f64.powi(st.snapshot_failures.saturating_sub(1).min(16) as i32)).min(every_ms);
            if now < failed_at + wait {
                return false;
            }
        }
        match &st.last_snapshot {
            Some(s) => s.created_at + every_ms <= now,
            None => true,
        }
    }

    /// Resume the generation recorded in `_meta` if the journal continues it exactly, or start a new one.
    async fn begin(&self, app: &App) -> crate::Result<()> {
        // Reachability first: while the target is down, journaling stays as it is.
        let generations = list_generations(&self.target).await?;
        let db = app.db();
        let journal_switch = db.journal().clone();
        let candidates: Vec<(String, u64)> =
            generations.iter().filter_map(|g| g.newest_ts().map(|ts| (g.id.clone(), ts))).collect();
        let decision = db
            .with_writer(move |conn| {
                // From here on every commit is journaled (a no-op when it already was, since the database opened).
                journal_switch.set_enabled(true);
                journal_switch.set_consumer(journal::Consumer::Replication, true);
                let ts = storage::read_meta_u64(conn, storage::META_TS)?;
                let Some(generation) = meta_text(conn, journal::META_GENERATION)? else {
                    return Ok(Resume::NewGeneration("this database has not been replicated yet".into()));
                };
                let Some((_, remote_ts)) = candidates.iter().find(|(id, _)| *id == generation) else {
                    return Ok(Resume::NewGeneration(format!("generation {generation} is not in the target")));
                };
                if *remote_ts > ts {
                    return Ok(Resume::NewGeneration(format!(
                        "the target's generation {generation} reaches ts {remote_ts}, beyond this database's ts {ts}"
                    )));
                }
                let mut shipped = storage::read_meta_u64(conn, journal::META_SHIPPED_TS)?;
                let mut shipped_time = storage::read_meta_f64(conn, journal::META_SHIPPED_TIME)?;
                if *remote_ts < shipped {
                    // Segments shipped earlier are gone (deleted by hand or a lifecycle rule, or lost by a target
                    // that didn't sync them): later ones couldn't be chained to a restore.
                    return Ok(Resume::NewGeneration(format!(
                        "the target's generation {generation} ends at ts {remote_ts}, but ts {shipped} was shipped: \
                         its newest segments are missing"
                    )));
                }
                if *remote_ts > shipped {
                    // Uploaded, but the trim didn't happen (a crash in between).
                    let time: Option<f64> =
                        conn.query_row("SELECT time FROM _journal WHERE ts = ?1", [*remote_ts as i64], |r| r.get(0)).optional()?;
                    shipped = *remote_ts;
                    shipped_time = time.unwrap_or(shipped_time);
                }
                let (rows, max): (i64, Option<i64>) = conn.query_row(
                    "SELECT COUNT(*), MAX(ts) FROM _journal WHERE ts > ?1",
                    [shipped as i64],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                let complete = rows as u64 == ts.saturating_sub(shipped) && (rows == 0 || max == Some(ts as i64));
                if shipped > ts || !complete {
                    return Ok(Resume::NewGeneration(format!(
                        "the journal doesn't hold every commit after ts {shipped} (commits were made without journaling)"
                    )));
                }
                set_shipped(conn, &generation, shipped, shipped_time)?;
                journal::release(conn, &journal_switch, journal::Consumer::Replication, shipped)?;
                let pending_bytes = journal::stats(conn)?.map(|s| s.3).unwrap_or(0);
                Ok(Resume::Continue { generation, shipped_ts: shipped, shipped_time, pending_bytes })
            })
            .await?;
        match decision {
            Resume::Continue { generation, shipped_ts, shipped_time, pending_bytes } => {
                db.journal().set_pending_bytes(pending_bytes);
                tracing::info!(generation, shipped_ts, pending_bytes, "replication: resuming");
                let mut st = self.state();
                st.last_snapshot = generations
                    .iter()
                    .find(|g| g.id == generation)
                    .and_then(|g| g.snapshots.iter().max_by(|a, b| a.created_at.total_cmp(&b.created_at)).cloned());
                st.generation = Some(generation);
                st.shipped_ts = shipped_ts;
                st.shipped_time = shipped_time;
                st.generations = generations.iter().map(summarize).collect();
            }
            Resume::NewGeneration(why) => {
                self.state().generations = generations.iter().map(summarize).collect();
                self.new_generation(app, &why).await?;
            }
        }
        self.retain(app).await
    }

    async fn new_generation(&self, app: &App, why: &str) -> crate::Result<()> {
        let now = app.db().clock().now_ms();
        let id = new_generation_id(now);
        // Journaling is on (`begin`): every commit after the snapshot's ts is in the journal.
        let (snapshot, manifest) = self.upload_snapshot(app, &id).await?;
        let (gen2, ts, time) = (id.clone(), manifest.ts, manifest.commit_time);
        let switch = app.db().journal().clone();
        let pending_bytes = app
            .db()
            .with_writer(move |conn| {
                set_shipped(conn, &gen2, ts, time)?;
                journal::release(conn, &switch, journal::Consumer::Replication, ts)?;
                Ok(journal::stats(conn)?.map(|s| s.3).unwrap_or(0))
            })
            .await?;
        app.db().journal().set_pending_bytes(pending_bytes);
        tracing::info!(generation = id, ts, reason = why, "replication: started a new generation");
        let mut st = self.state();
        st.generations.push(GenerationSummary {
            id: id.clone(),
            created_at: now,
            current: false,
            snapshots: vec![snapshot.clone()],
            segments: 0,
            from_ts: Some(ts),
            to_ts: Some(ts),
            from_time: Some(snapshot.created_at),
            to_time: None,
        });
        st.generation = Some(id);
        st.shipped_ts = ts;
        st.shipped_time = time;
        st.last_snapshot = Some(snapshot);
        st.snapshots_taken += 1;
        Ok(())
    }

    async fn upload_snapshot(&self, app: &App, generation: &str) -> crate::Result<(SnapshotInfo, Manifest)> {
        let target = snapshots_target(&self.target, generation);
        let (info, manifest) = backup::create_with_manifest(app, &BackupSpec::default(), &target, true).await?;
        let snapshot =
            SnapshotInfo { name: info.name, ts: manifest.ts, created_at: info.created_at, bytes: info.bytes };
        Ok((snapshot, manifest))
    }

    async fn snapshot(&self, app: &App) -> crate::Result<SnapshotInfo> {
        let generation = self.state().generation.clone().context("no replication generation")?;
        let (snapshot, _) = self.upload_snapshot(app, &generation).await?;
        tracing::info!(generation, name = snapshot.name, ts = snapshot.ts, "replication: snapshot stored");
        {
            let mut st = self.state();
            if st.generation.as_ref() != Some(&generation) {
                // A new generation started while it was taken (shipping went on): it belongs to the old one.
                return Ok(snapshot);
            }
            if let Some(g) = st.generations.iter_mut().find(|g| g.id == generation) {
                g.snapshots.push(snapshot.clone());
                g.snapshots.sort_by(|a, b| (a.ts, &a.name).cmp(&(b.ts, &b.name)));
                g.to_ts = g.to_ts.max(Some(snapshot.ts));
            }
            st.last_snapshot = Some(snapshot.clone());
            st.snapshots_taken += 1;
        }
        self.retain(app).await?;
        Ok(snapshot)
    }

    /// List the target again, apply retention, and keep the listing.
    async fn retain(&self, app: &App) -> crate::Result<()> {
        let mut generations = list_generations(&self.target).await?;
        let plan = plan_retention(&generations, app.db().clock().now_ms(), self.options.keep);
        let current = self.state().generation.clone();
        if let Some(c) = &current {
            // The plan never drops the generation being shipped (its newest snapshot is the newest overall); refuse
            // to act on a plan that would, rather than lose the replica.
            if plan.generations.contains(c) {
                bail!("retention would delete the current generation {c}");
            }
        }
        if !plan.is_empty() {
            apply_retention(&self.target, &generations, &plan).await?;
            tracing::info!(
                generations = plan.generations.len(),
                snapshots = plan.snapshots.len(),
                segments = plan.segments.len(),
                "replication: retention deleted old data"
            );
            without(&mut generations, &plan);
        }
        self.state().generations = generations.iter().map(summarize).collect();
        Ok(())
    }

    /// Ship every unshipped commit, one segment at a time. With `lazy`, nothing is shipped while every unshipped
    /// commit is scheduler bookkeeping younger than `jobs_every` (see the module docs).
    async fn ship(&self, app: &App, lazy: bool) -> crate::Result<()> {
        let mut first_segment = true;
        loop {
            let (generation, shipped) = {
                let st = self.state();
                (st.generation.clone().context("no replication generation")?, st.shipped_ts)
            };
            let snapshot = app.db().snapshot().await?;
            let (max_entries, max_bytes) =
                (self.options.segment_max_entries.max(1), self.options.segment_max_bytes.max(1));
            let entries = snapshot.with_conn(|c| journal::read_after(c, shipped, max_entries, max_bytes))?;
            drop(snapshot);
            let (Some(first), Some(last)) = (entries.first(), entries.last()) else { return Ok(()) };
            if lazy && first_segment {
                // A full segment ships anyway: there is a lot to ship, and it may hide other commits after it.
                let full =
                    entries.len() >= max_entries || entries.iter().map(|e| e.changes.len()).sum::<usize>() >= max_bytes;
                let jobs_every_ms = self.options.jobs_every.as_secs_f64() * 1000.0;
                let young = app.db().clock().now_ms() - first.time < jobs_every_ms;
                if !full && young && entries.iter().all(JournalEntry::is_scheduler_bookkeeping) {
                    return Ok(());
                }
            }
            first_segment = false;
            if first.ts != shipped + 1 {
                // The journal no longer continues the generation (it can't happen while journaling stays on): start over.
                self.state().generation = None;
                bail!(
                    "the journal starts at ts {} but ts {} was shipped last; starting a new generation",
                    first.ts,
                    shipped
                );
            }
            let (from, to, to_time, count) = (first.ts, last.ts, last.time, entries.len() as u64);
            let gen2 = generation.clone();
            let bytes = tokio::task::spawn_blocking(move || encode_segment(&gen2, &entries)).await??;
            let size = bytes.len() as u64;
            put(&self.target, &segment_rel(&generation, from, to), bytes).await?;
            let (gen2, switch) = (generation.clone(), app.db().journal().clone());
            app.db()
                .with_writer(move |conn| {
                    set_shipped(conn, &gen2, to, to_time)?;
                    journal::release(conn, &switch, journal::Consumer::Replication, to)
                })
                .await?;
            let now = app.db().clock().now_ms();
            let mut st = self.state();
            st.shipped_ts = to;
            st.shipped_time = to_time;
            st.segments_shipped += 1;
            st.last_segment = Some(ShippedSegment {
                name: segment_name(from, to),
                from,
                to,
                entries: count,
                bytes: size,
                shipped_at: now,
            });
            if let Some(g) = st.generations.iter_mut().find(|g| g.id == generation) {
                g.segments += 1;
                g.to_ts = g.to_ts.max(Some(to));
            }
        }
    }

    /// The target has been unreachable for too long: stop journaling and drop the journal. The next successful step
    /// starts a new generation (with a snapshot) and journals again. While the analytics copy reads the journal,
    /// journaling stays on and only the rows the copy no longer needs go.
    async fn drop_journal(&self, app: &App) -> crate::Result<()> {
        let switch = app.db().journal().clone();
        let max = self.options.max_pending_bytes;
        let pending = app
            .db()
            .with_writer(move |conn| {
                switch.set_consumer(journal::Consumer::Replication, false);
                if let Some(floor) = journal::others_floor(conn, &switch, journal::Consumer::Replication)? {
                    let removed = journal::trim(conn, floor)?;
                    switch.sub_pending_bytes(removed);
                    return Ok(switch.pending_bytes());
                }
                switch.set_enabled(false);
                journal::scrub(conn)?;
                Ok(0)
            })
            .await?;
        app.db().journal().set_pending_bytes(pending);
        self.state().generation = None;
        tracing::error!(
            target = %self.target.location,
            max_pending_bytes = max,
            "replication: the target has been unreachable too long and the local journal passed --replicate-max-pending; \
             it was dropped and journaling paused. A new generation (with a new snapshot) starts once the target is \
             reachable; point-in-time restores can't reach the commits in between"
        );
        Ok(())
    }

    /// The current status (reads the latest commit and the oldest unshipped one on a reader snapshot).
    pub async fn status(&self, app: &App) -> ReplicationStatus {
        let st = {
            let st = self.state();
            (
                st.generation.clone(),
                st.shipped_ts,
                st.shipped_time,
                st.last_segment.clone(),
                st.last_snapshot.clone(),
                st.generations.clone(),
                (
                    st.segments_shipped,
                    st.snapshots_taken,
                    st.last_success_at,
                    st.last_error.clone(),
                    st.last_error_at,
                    st.failures,
                ),
            )
        };
        let (generation, shipped_ts, shipped_time, last_segment, last_snapshot, generations, counters) = st;
        let (current_ts, current_time, oldest_unshipped) = match app.db().snapshot().await {
            Ok(s) => {
                let oldest = s
                    .with_conn(|c| {
                        Ok(c.query_row(
                            "SELECT time FROM _journal WHERE ts > ?1 ORDER BY ts LIMIT 1",
                            [shipped_ts as i64],
                            |r| r.get::<_, f64>(0),
                        )
                        .optional()?)
                    })
                    .ok()
                    .flatten();
                (s.ts, s.time, oldest)
            }
            Err(_) => (app.db().write_log().latest(), 0.0, None),
        };
        let now = app.db().clock().now_ms();
        let lag_commits = current_ts.saturating_sub(shipped_ts);
        let lag_ms = match (lag_commits, oldest_unshipped) {
            (0, _) => 0.0,
            (_, Some(t)) => (now - t).max(0.0),
            (_, None) => (now - shipped_time.max(current_time)).max(0.0),
        };
        let summaries = generations
            .into_iter()
            .map(|mut g| {
                g.current = generation.as_deref() == Some(g.id.as_str());
                g.to_time = g.current.then_some(shipped_time);
                g
            })
            .collect();
        let (segments_shipped, snapshots_taken, last_success_at, last_error, last_error_at, failures) = counters;
        ReplicationStatus {
            target: self.target.location.clone(),
            every_ms: self.options.every.as_secs_f64() * 1000.0,
            jobs_every_ms: self.options.jobs_every.as_secs_f64() * 1000.0,
            snapshot_every_ms: self.options.snapshot_every.as_secs_f64() * 1000.0,
            keep_ms: self.options.keep.as_secs_f64() * 1000.0,
            next_snapshot_at: last_snapshot
                .as_ref()
                .map(|s| s.created_at + self.options.snapshot_every.as_secs_f64() * 1000.0),
            generation,
            current_ts,
            current_time,
            shipped_ts,
            shipped_time,
            lag_commits,
            lag_ms,
            pending_bytes: app.db().journal().pending_bytes(),
            max_pending_bytes: self.options.max_pending_bytes,
            journaling: app.db().journal().is_enabled(),
            last_segment,
            last_snapshot,
            segments_shipped,
            snapshots_taken,
            last_success_at,
            last_error,
            last_error_at,
            failures,
            generations: summaries,
        }
    }
}

/// Ship what is unshipped before the server exits (best effort, at most `timeout`).
pub async fn flush(app: &App, timeout: Duration) {
    let Some(replicator) = app.replication() else { return };
    match tokio::time::timeout(timeout, replicator.flush(app)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!("replication: shipping the last commits at shutdown failed: {e:#}"),
        Err(_) => tracing::warn!("replication: shipping the last commits at shutdown timed out"),
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Point-in-time restore
// ---------------------------------------------------------------------------------------------------------------------

/// Which point to restore. Without `to_ts` and `to_time`: the newest one available.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PointInTime {
    /// A commit ts (in the chosen generation).
    pub to_ts: Option<u64>,
    /// A time (ms since the epoch): the state after the last commit at or before it.
    pub to_time: Option<f64>,
    /// The generation (default: the newest; with `to_time`, the one that was live then).
    pub generation: Option<String>,
}

/// What a point-in-time restore restored (`PendingRestore::point_in_time`, the CLI's report).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct PointInTimeInfo {
    pub generation: String,
    /// The snapshot it started from, and its ts.
    pub snapshot: String,
    #[ts(type = "number")]
    pub snapshot_ts: u64,
    /// What was asked for.
    #[ts(type = "number | null")]
    pub requested_ts: Option<u64>,
    pub requested_time: Option<f64>,
    /// The restored commit ts and its time.
    #[ts(type = "number")]
    pub ts: u64,
    pub time: f64,
    /// Journal entries replayed, from how many segments.
    #[ts(type = "number")]
    pub replayed: u64,
    #[ts(type = "number")]
    pub segments: u64,
    /// The requested point was reached; `false` when it is beyond the newest replicated commit, which was restored
    /// instead.
    pub reached: bool,
}

/// The snapshot and segments a point-in-time restore uses.
#[derive(Debug, Clone, PartialEq)]
pub struct RestorePlan {
    pub generation: String,
    pub snapshot: SnapshotInfo,
    /// The segments after the snapshot, in order (up to the target ts when known).
    pub segments: Vec<SegmentInfo>,
}

fn user_error(message: impl Into<String>) -> crate::Error {
    UserError::new("invalid_point_in_time", message).into()
}

fn time_str(ms: f64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms as i64)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_else(|| format!("{ms} ms"))
}

/// Choose the generation, the base snapshot and the segments for `point` (see [`PointInTime`]).
pub fn plan_restore(generations: &[Generation], point: &PointInTime) -> crate::Result<RestorePlan> {
    if point.to_ts.is_some() && point.to_time.is_some() {
        return Err(user_error("give a ts or a time, not both"));
    }
    let usable: Vec<&Generation> = generations.iter().filter(|g| !g.snapshots.is_empty()).collect();
    let generation = match (&point.generation, point.to_time) {
        (Some(id), _) => *usable
            .iter()
            .find(|g| &g.id == id)
            .ok_or_else(|| user_error(format!("no generation {id} in the target")))?,
        (None, Some(t)) => *usable.iter().rev().find(|g| g.created_at <= t).ok_or_else(|| match usable.first() {
            Some(g) => user_error(format!(
                "{} is before the oldest replicated data (generation {} started {})",
                time_str(t),
                g.id,
                time_str(g.created_at)
            )),
            None => user_error("the target holds no replicated data"),
        })?,
        (None, None) => *usable.last().ok_or_else(|| user_error("the target holds no replicated data"))?,
    };
    let snapshot = match (point.to_ts, point.to_time) {
        (Some(n), _) => generation.snapshots.iter().rev().find(|s| s.ts <= n).ok_or_else(|| {
            user_error(format!(
                "ts {n} is before the oldest snapshot (ts {}) of generation {}{}",
                generation.snapshots[0].ts,
                generation.id,
                if point.generation.is_none() { "; pick an older generation" } else { "" }
            ))
        })?,
        (_, Some(t)) => generation.snapshots.iter().rev().find(|s| s.created_at <= t).ok_or_else(|| {
            user_error(format!(
                "{} is before the oldest snapshot (taken {}) of generation {}",
                time_str(t),
                time_str(generation.snapshots[0].created_at),
                generation.id
            ))
        })?,
        (None, None) => generation.snapshots.last().expect("usable generations have snapshots"),
    };
    let (segments, _) = chain(&generation.segments, snapshot.ts, point.to_ts);
    Ok(RestorePlan { generation: generation.id.clone(), snapshot: snapshot.clone(), segments })
}

/// Make sure `staging/data.db` knows every table the replayed entries write, with the numbers the journal uses
/// (tables created after the snapshot). Tables the binary doesn't define get the two built-in indexes.
fn seed_tables(db: &Path, tables: &BTreeMap<u32, String>) -> crate::Result<()> {
    let conn = Connection::open(db)?;
    let tx = conn.unchecked_transaction()?;
    for (number, name) in tables {
        let by_number: Option<String> =
            tx.query_row("SELECT name FROM _tables WHERE table_number = ?1", [number], |r| r.get(0)).optional()?;
        match by_number {
            Some(n) if &n == name => continue,
            Some(n) => bail!("table number {number} is `{n}` in the snapshot but `{name}` in the journal"),
            None => {}
        }
        let by_name: Option<u32> =
            tx.query_row("SELECT table_number FROM _tables WHERE name = ?1", [name], |r| r.get(0)).optional()?;
        if let Some(other) = by_name {
            bail!("table `{name}` is number {other} in the snapshot but {number} in the journal");
        }
        tx.execute("INSERT INTO _tables (table_number, name) VALUES (?1, ?2)", params![number, name])?;
        for (index, fields) in [(crate::schema::BY_ID, "[]"), (crate::schema::BY_CREATION_TIME, "[\"_creationTime\"]")]
        {
            tx.execute(
                "INSERT INTO _indexes (index_id, table_number, name, fields)
                 VALUES ((SELECT COALESCE(MAX(index_id), 0) + 1 FROM _indexes), ?1, ?2, ?3)",
                params![number, index, fields],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Segments a point-in-time restore downloads at once.
const SEGMENT_PREFETCH: usize = 8;

/// Restore the state of `point` from the replication `target` into the empty directory `staging`, using `scratch`
/// for downloads, and check it (see the module docs; `skip_verify` skips opening it with this binary's tables, as
/// for an archive). Leaves `staging` as [`backup::prepare`] does, ready for [`backup::apply_staged`]. Doesn't touch
/// any data directory: safe while a server runs.
pub async fn prepare_point_in_time(
    target: &BackupTarget,
    scratch: &Path,
    staging: &Path,
    point: &PointInTime,
    skip_verify: bool,
) -> crate::Result<(Manifest, PointInTimeInfo)> {
    let generations = list_generations(target).await?;
    let plan = plan_restore(&generations, point)?;
    let archive = scratch.join(&plan.snapshot.name);
    snapshots_target(target, &plan.generation).download(&plan.snapshot.name, &archive).await?;
    let mut manifest = backup::prepare(&archive, staging, true).await?;
    anyhow::ensure!(
        manifest.ts == plan.snapshot.ts,
        "snapshot {} holds ts {}, not {}",
        plan.snapshot.name,
        manifest.ts,
        plan.snapshot.ts
    );
    let _ = std::fs::remove_file(&archive);

    // Download and check the segments, stopping at the first entry beyond the target; note the tables they write.
    let beyond = |e: &JournalEntry| point.to_ts.is_some_and(|n| e.ts > n) || point.to_time.is_some_and(|t| e.time > t);
    let mut files: Vec<PathBuf> = Vec::new();
    let mut tables: BTreeMap<u32, String> = BTreeMap::new();
    let (mut ts, mut time) = (manifest.ts, manifest.commit_time);
    let mut stopped = false;
    // A day of one-second segments is tens of thousands of objects: fetch several at a time, in order, and decode
    // (zstd, SHA-256) off the runtime's threads.
    let ranges: Vec<(u64, u64)> = plan.segments.iter().map(|s| (s.from, s.to)).collect();
    let mut fetched = futures::stream::iter(ranges.into_iter().map(|(from, to)| {
        let (target, generation) = (target.clone(), plan.generation.clone());
        async move {
            let raw = get(&target, &segment_rel(&generation, from, to)).await?;
            let decoded = tokio::task::spawn_blocking(move || {
                decode_segment(&raw, Some(&generation)).map(|(header, entries)| (header, entries, raw))
            })
            .await??;
            Ok::<_, crate::Error>(((from, to), decoded))
        }
    }))
    .buffered(SEGMENT_PREFETCH);
    'segments: while let Some(next) = fetched.next().await {
        let ((from, to), (header, entries, raw)) = next?;
        let seg = SegmentInfo { from, to, bytes: raw.len() as u64 };
        anyhow::ensure!(
            (header.from, header.to) == (seg.from, seg.to),
            "segment {} holds ts {}..{}",
            segment_name(seg.from, seg.to),
            header.from,
            header.to
        );
        let file = scratch.join(segment_name(seg.from, seg.to));
        let to = file.clone();
        tokio::task::spawn_blocking(move || std::fs::write(&to, &raw)).await??;
        files.push(file);
        for e in &entries {
            if e.ts <= ts {
                continue;
            }
            anyhow::ensure!(e.ts == ts + 1, "journal gap after ts {ts}");
            if beyond(e) {
                stopped = true;
                break 'segments;
            }
            for change in e.parse_changes()? {
                tables.entry(change.id.table_number()).or_insert(change.table);
            }
            (ts, time) = (e.ts, e.time);
        }
    }
    let (final_ts, final_time) = (ts, time);
    let db_path = staging.join("data.db");
    tokio::task::spawn_blocking(move || seed_tables(&db_path, &tables)).await??;

    // Replay through the write path.
    let dir = staging.to_path_buf();
    let db = tokio::task::spawn_blocking(move || {
        let tables = crate::runtime::registered_tables();
        let options = crate::db::OpenOptions { readers: 1, skip_schema_check: true, ..Default::default() };
        crate::db::Database::open_with_migrations(&dir, &tables, &options, &[])
    })
    .await?
    .context("the snapshot can't be opened by this binary")?;
    drop(fetched);
    let mut replayed = 0u64;
    for file in &files {
        let (file, generation) = (file.clone(), plan.generation.clone());
        let (_, entries) = tokio::task::spawn_blocking(move || -> crate::Result<_> {
            let raw = std::fs::read(&file)?;
            decode_segment(&raw, Some(&generation))
        })
        .await??;
        for e in &entries {
            if e.ts <= manifest.ts + replayed || e.ts > final_ts {
                continue;
            }
            db.replay(e).await?;
            replayed += 1;
        }
    }
    drop(db);
    if skip_verify {
        backup::close_wal(staging).await?;
    } else {
        backup::verify(staging).await.context("the restored point fails the checks")?;
    }
    let path = staging.join("data.db");
    let restored_ts = tokio::task::spawn_blocking(move || -> crate::Result<u64> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(30))?;
        conn.pragma_update(None, "journal_mode", "WAL").context("switching the restored database to WAL")?;
        storage::read_meta_u64(&conn, storage::META_TS)
    })
    .await??;
    anyhow::ensure!(restored_ts == final_ts, "the restored database is at ts {restored_ts}, expected {final_ts}");
    let reached = match (point.to_ts, point.to_time) {
        (Some(n), _) => final_ts == n,
        // Known exactly only if a later commit exists (the replay stopped before it); otherwise the replicated data
        // ends before the time asked for.
        (_, Some(_)) => stopped,
        _ => true,
    };
    let info = PointInTimeInfo {
        generation: plan.generation.clone(),
        snapshot: plan.snapshot.name.clone(),
        snapshot_ts: plan.snapshot.ts,
        requested_ts: point.to_ts,
        requested_time: point.to_time,
        ts: final_ts,
        time: final_time,
        replayed,
        segments: files.len() as u64,
        reached,
    };
    manifest.ts = final_ts;
    manifest.commit_time = final_time;
    std::fs::write(staging.join("manifest.json"), serde_json::to_vec_pretty(&manifest)?)?;
    tracing::info!(
        generation = info.generation,
        snapshot_ts = info.snapshot_ts,
        ts = final_ts,
        replayed,
        "point-in-time restore prepared"
    );
    Ok((manifest, info))
}

/// Restore `point` from `target` into the data directory `dir` (offline: takes the directory lock).
pub async fn restore_point_in_time(
    target: &BackupTarget,
    dir: &Path,
    point: &PointInTime,
    options: &backup::RestoreOptions,
) -> crate::Result<(backup::RestoreReport, PointInTimeInfo)> {
    let _lock = super::DirLock::acquire(dir)?;
    let scratch = tempfile::Builder::new().prefix(".rxferrix-pitr-").tempdir_in(dir)?;
    let staging = scratch.path().join("staged");
    std::fs::create_dir_all(&staging)?;
    let (manifest, info) = prepare_point_in_time(target, scratch.path(), &staging, point, options.skip_verify).await?;
    let previous = dir.join(format!("pre-restore-{}", super::file_timestamp(crate::db::wall_clock_ms())));
    let report = backup::apply_staged(&staging, dir, manifest, options.storage_dir.clone(), &previous)?;
    Ok((report, info))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(ts: u64, at: f64) -> SnapshotInfo {
        SnapshotInfo { name: backup::archive_name_with_ts(at, true, ts), ts, created_at: at, bytes: 1 }
    }

    fn seg(from: u64, to: u64) -> SegmentInfo {
        SegmentInfo { from, to, bytes: 1 }
    }

    fn generation(id_at: f64, snapshots: Vec<SnapshotInfo>, segments: Vec<SegmentInfo>) -> Generation {
        Generation {
            id: format!("{}-0000000a", super::super::file_timestamp(id_at)),
            created_at: id_at,
            snapshots,
            segments,
        }
    }

    #[test]
    fn names_round_trip() {
        assert_eq!(segment_name(5, 12), "00000000000000000005-00000000000000000012.zst");
        assert_eq!(parse_segment_name(&segment_name(5, 12)), Some((5, 12)));
        assert_eq!(parse_segment_name("00000000000000000012-00000000000000000005.zst"), None);
        let id = new_generation_id(1_760_011_200_123.0);
        assert_eq!(generation_created_at(&id), Some(1_760_011_200_123.0));
        assert_eq!(generation_created_at("20251009T120000.123Z"), None);
        assert_eq!(
            snapshot_time(&backup::archive_name_with_ts(1_760_011_200_123.0, true, 4)),
            Some(1_760_011_200_123.0)
        );
    }

    #[test]
    fn segments_round_trip_and_reject_damage() {
        let entries: Vec<JournalEntry> = (3..=5)
            .map(|ts| JournalEntry { ts, time: 1000.25 + ts as f64, changes: format!("[[\"t\",\"x{ts}\",null]]") })
            .collect();
        let raw = encode_segment("g", &entries).unwrap();
        let (header, back) = decode_segment(&raw, Some("g")).unwrap();
        assert_eq!((header.from, header.to, header.entries), (3, 5, 3));
        assert_eq!(back, entries);
        assert!(decode_segment(&raw, Some("other")).is_err());
        // A changed body fails the checksum.
        let mut plain = zstd::decode_all(raw.as_slice()).unwrap();
        let n = plain.len();
        plain[n - 3] = b'X';
        let damaged = zstd::encode_all(plain.as_slice(), 3).unwrap();
        assert!(decode_segment(&damaged, None).unwrap_err().to_string().contains("checksum"));
        let gap = vec![entries[0].clone(), entries[2].clone()];
        assert!(encode_segment("g", &gap).is_err());
    }

    #[test]
    fn chains_tolerate_overlaps() {
        let segs = [seg(1, 5), seg(1, 7), seg(6, 9), seg(10, 10), seg(12, 14)];
        let (used, end) = chain(&segs, 0, None);
        assert_eq!(used, [seg(1, 7), seg(6, 9), seg(10, 10)]);
        assert_eq!(end, 10);
        assert_eq!(chain(&segs, 3, Some(6)).0, [seg(1, 7)]);
        assert_eq!(chain(&segs, 10, None).1, 10, "a gap ends the chain");
    }

    #[test]
    fn restore_plans() {
        let day = 86_400_000.0;
        let g1 = generation(0.0, vec![snap(10, 0.0), snap(20, day)], vec![seg(11, 15), seg(16, 25), seg(26, 30)]);
        let g2 = generation(2.0 * day, vec![snap(5, 2.0 * day)], vec![seg(6, 8)]);
        let gens = [g1.clone(), g2.clone()];
        let p = plan_restore(&gens, &PointInTime::default()).unwrap();
        assert_eq!((p.generation.as_str(), p.snapshot.ts, p.segments.len()), (g2.id.as_str(), 5, 1));
        let p = plan_restore(
            &gens,
            &PointInTime { to_ts: Some(17), generation: Some(g1.id.clone()), ..Default::default() },
        )
        .unwrap();
        assert_eq!((p.snapshot.ts, p.segments.clone()), (10, vec![seg(11, 15), seg(16, 25)]));
        let p = plan_restore(&gens, &PointInTime { to_time: Some(1.5 * day), ..Default::default() }).unwrap();
        assert_eq!((p.generation.as_str(), p.snapshot.ts), (g1.id.as_str(), 20));
        assert_eq!(p.segments, [seg(16, 25), seg(26, 30)]);
        assert!(plan_restore(&gens, &PointInTime { to_ts: Some(3), ..Default::default() }).is_err());
        assert!(plan_restore(&gens, &PointInTime { to_time: Some(-1.0), ..Default::default() }).is_err());
        assert!(
            plan_restore(&gens, &PointInTime { to_ts: Some(3), to_time: Some(1.0), ..Default::default() }).is_err()
        );
        assert!(plan_restore(&[], &PointInTime::default()).is_err());
    }
}

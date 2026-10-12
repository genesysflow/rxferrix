//! Staged restores: restoring a backup from the dashboard (or any admin client) without ever replacing the database
//! under a running server.
//!
//! A restore replaces `data.db`, so it can only happen while no process has the database open (`rxferrix restore` is
//! offline for that reason). A staged restore splits it in two:
//!
//! 1. **Stage** ([`stage`], `POST /api/admin/backups/{name}/restore` or `POST /api/admin/restore` with an uploaded
//!    archive), while the server runs: the archive is extracted into a temporary directory next to the data and goes
//!    through every check of `rxferrix restore` ([`super::backup::prepare`]: the manifest's sizes and checksums, SQLite's
//!    `integrity_check`, no hand-made triggers or views, opening it with this binary's tables, `verify_indexes`). A
//!    backup that fails is refused there and then, and nothing changes. One that passes is moved to
//!    `<dir>/restore-staged/`, the SHA-256 of its databases is recorded, and the marker `<dir>/pending-restore.json`
//!    is written last (atomically: a temporary file renamed into place). The running server is not affected.
//! 2. **Apply** ([`apply_pending`]), on the next start of `serve`, after it took the data directory lock and before it
//!    opens the database: the staged databases are checked against the recorded checksums, then put in place exactly
//!    as `rxferrix restore` does (the current `data.db`, `aux.db` and persisted vector indexes are moved to
//!    `<dir>/pre-restore-<time staged>/`; blobs are added to local storage). The marker is removed, then the staging
//!    directory. Every step can be repeated, so a crash halfway is finished by the next start. If the staged files
//!    were changed or damaged since staging, nothing is replaced: the server starts on the current database and the
//!    marker records the error (shown on the dashboard) until it is canceled.
//!
//! **Point in time** ([`stage_point_in_time`], `POST /api/admin/restore/point-in-time`): the same, with the staging
//! directory filled by `super::replicate::prepare_point_in_time` (a snapshot of the replication target plus the
//! replayed journal, checked like an archive) instead of an archive; the marker records what was restored
//! (`PendingRestore::point_in_time`).
//!
//! **Cancel** ([`cancel`], `DELETE /api/admin/restore`, `rxferrix restore --cancel`): removes the marker, then the
//! staged files. **Restart** (`POST /api/admin/restore/restart`): only when the server runs with
//! `--restart-exit-code N` (under systemd with `Restart=on-failure` or `RestartForceExitStatus=N`, or Docker with a
//! restart policy): the server shuts down gracefully and exits with `N`, and the supervisor starts it again, which
//! applies the restore. Without the flag the dashboard asks for a manual restart. A server never restarts on its own.
//!
//! **What is (not) restored over HTTP.** `auth_secret.key` is never restored by a staged restore, even if the archive
//! has one (only `rxferrix restore` restores it): otherwise an administrator's token, which expires in minutes, could be
//! turned into a signing key chosen by whoever holds the token, valid long after. Secrets in `_settings` were
//! encrypted with the key of the server that made the backup, so they are readable only if it is the same key.
//! Everything else in the backup replaces the current data, `_admins` and `_sessions` included: the administrator who
//! restores may not exist in the backup (create one with `admin create`), and every session not in the backup ends.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::backup::{self, Manifest, ManifestFile};
use crate::UserError;

/// The staging directory, in the data directory.
pub const STAGED_DIR: &str = "restore-staged";
/// The marker of a pending restore, in the data directory.
pub const MARKER: &str = "pending-restore.json";
const SECRET_FILE: &str = "auth_secret.key";

/// A staged restore, waiting for the next start (`pending-restore.json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct PendingRestore {
    /// The backup's name, or the uploaded file's.
    pub source: String,
    /// When it was staged (ms since the epoch).
    pub staged_at: f64,
    /// The administrator who staged it (`Id<"_admins">`), if known.
    pub staged_by: Option<String>,
    /// When the backup was taken (ms), and the commit ts of its snapshot.
    pub backup_created_at: f64,
    #[ts(type = "number")]
    pub ts: u64,
    /// The rxferrix version that wrote the backup.
    pub rxferrix_version: String,
    /// The backup has the execution log (`aux.db`).
    pub includes_aux: bool,
    /// Stored file blobs in the backup.
    #[ts(type = "number")]
    pub blobs: u64,
    /// Bytes staged (databases and blobs).
    #[ts(type = "number")]
    pub bytes: u64,
    /// The archive had `auth_secret.key`; it is not restored (see the module docs).
    pub secret_ignored: bool,
    /// Why the last start didn't apply it (the staged files were changed or damaged). Cancel it and stage again.
    pub error: Option<String>,
    /// A point-in-time restore from the replication target (`super::replicate`): the snapshot it started from and
    /// the point reached. `None` for an archive.
    #[serde(default)]
    pub point_in_time: Option<super::replicate::PointInTimeInfo>,
    /// SHA-256 of the staged databases, checked before they are applied (in the marker only; [`Self::public`]
    /// leaves them out).
    #[ts(skip)]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub staged_files: Vec<ManifestFile>,
}

impl PendingRestore {
    /// As the admin endpoints show it: without the internal checksums.
    pub fn public(mut self) -> Self {
        self.staged_files.clear();
        self
    }
}

fn user_error(code: &str, message: impl Into<String>) -> crate::Error {
    UserError::new(code, message).into()
}

/// The pending restore of the data directory `dir`, if any.
pub fn pending(dir: &Path) -> crate::Result<Option<PendingRestore>> {
    let path = dir.join(MARKER);
    match std::fs::read(&path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).with_context(|| format!("reading {}", path.display()))?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Write the marker atomically (a temporary file, synced, renamed into place, the directory synced).
fn write_marker(dir: &Path, marker: &PendingRestore) -> crate::Result<()> {
    let mut tmp = tempfile::Builder::new().prefix(".pending-restore-").tempfile_in(dir)?;
    serde_json::to_writer_pretty(tmp.as_file_mut(), marker)?;
    tmp.as_file().sync_all()?;
    tmp.persist(dir.join(MARKER)).context("writing the pending-restore marker")?;
    sync_dir(dir);
    Ok(())
}

fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

fn sha256_file(path: &Path) -> crate::Result<ManifestFile> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut bytes = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        bytes += n as u64;
    }
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
    Ok(ManifestFile { path: name, bytes, sha256: data_encoding::HEXLOWER.encode(&hasher.finalize()) })
}

/// One staging at a time per process.
static STAGING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Stage `archive` for a restore into the data directory `dir` at the next start (see the module docs). Fails with
/// the user error `restore_pending` if one is staged already, or `invalid_backup` with the reason if the archive
/// doesn't pass the checks (nothing is changed then).
pub async fn stage(
    dir: &Path,
    archive: &Path,
    source: &str,
    staged_by: Option<String>,
) -> crate::Result<PendingRestore> {
    let archive = archive.to_path_buf();
    stage_with(dir, source, staged_by, |work| async move {
        let manifest = backup::prepare(&archive, &work, false)
            .await
            .map_err(|e| user_error("invalid_backup", format!("This backup can't be restored: {e:#}")))?;
        Ok((manifest, None))
    })
    .await
}

/// Stage a point-in-time restore from the replication `target` (`super::replicate::prepare_point_in_time`: the
/// snapshot plus the replayed journal, checked like an archive). Fails with `restore_pending`, or
/// `invalid_point_in_time` / `invalid_backup` with the reason.
pub async fn stage_point_in_time(
    dir: &Path,
    target: &super::BackupTarget,
    point: &super::replicate::PointInTime,
    staged_by: Option<String>,
) -> crate::Result<PendingRestore> {
    let scratch = tempfile::Builder::new().prefix(".rxferrix-pitr-").tempdir_in(dir)?;
    let (target, point, scratch_path) = (target.clone(), point.clone(), scratch.path().to_path_buf());
    let marker = stage_with(dir, "point in time", staged_by, |work| async move {
        let (manifest, info) = super::replicate::prepare_point_in_time(&target, &scratch_path, &work, &point)
            .await
            .map_err(|e| match e.chain().find_map(|c| c.downcast_ref::<UserError>()) {
                Some(u) => crate::Error::from(u.clone()),
                None => user_error("invalid_backup", format!("This point in time can't be restored: {e:#}")),
            })?;
        Ok((manifest, Some(info)))
    })
    .await;
    drop(scratch);
    marker
}

/// Stage what `prepare` puts into the (empty) staging directory it is given, with its manifest.
async fn stage_with<F, Fut>(
    dir: &Path,
    source: &str,
    staged_by: Option<String>,
    prepare: F,
) -> crate::Result<PendingRestore>
where
    F: FnOnce(PathBuf) -> Fut,
    Fut: std::future::Future<Output = crate::Result<(Manifest, Option<super::replicate::PointInTimeInfo>)>>,
{
    let _one = STAGING.lock().await;
    if pending(dir)?.is_some() {
        return Err(user_error("restore_pending", "A restore is staged already; cancel it first"));
    }
    let work = tempfile::Builder::new().prefix(".rxferrix-restore-staging-").tempdir_in(dir)?;
    let (manifest, point_in_time) = prepare(work.path().to_path_buf()).await?;
    let source = match &point_in_time {
        Some(p) => format!("point in time: ts {} of generation {}", p.ts, p.generation),
        None => source.to_string(),
    };
    let secret_ignored = work.path().join(SECRET_FILE).exists();
    if secret_ignored {
        std::fs::remove_file(work.path().join(SECRET_FILE))?;
    }
    let w = work.path().to_path_buf();
    let staged_files = tokio::task::spawn_blocking(move || -> crate::Result<Vec<ManifestFile>> {
        let mut files = Vec::new();
        for db in ["data.db", "aux.db"] {
            if w.join(db).exists() {
                files.push(sha256_file(&w.join(db))?);
            }
        }
        Ok(files)
    })
    .await??;
    let blob_bytes: u64 = manifest.files.iter().filter(|f| f.path.starts_with("storage/")).map(|f| f.bytes).sum();
    let marker = PendingRestore {
        source,
        staged_at: crate::db::wall_clock_ms(),
        staged_by,
        backup_created_at: manifest.created_at,
        ts: manifest.ts,
        rxferrix_version: manifest.rxferrix_version.clone(),
        includes_aux: staged_files.iter().any(|f| f.path == "aux.db"),
        blobs: manifest.files.iter().filter(|f| f.path.starts_with("storage/")).count() as u64,
        bytes: staged_files.iter().map(|f| f.bytes).sum::<u64>() + blob_bytes,
        secret_ignored,
        error: None,
        point_in_time,
        staged_files,
    };
    // A leftover staging directory (a cancel interrupted halfway) goes first.
    let dest = dir.join(STAGED_DIR);
    if dest.exists() {
        std::fs::remove_dir_all(&dest).with_context(|| format!("removing {}", dest.display()))?;
    }
    let staging = work.keep();
    std::fs::rename(&staging, &dest).with_context(|| format!("moving the staged restore to {}", dest.display()))?;
    sync_dir(dir);
    write_marker(dir, &marker)?;
    tracing::warn!(
        source = marker.source,
        ts = marker.ts,
        "restore: a backup is staged and replaces the database at the next start (cancel it to keep the current data)"
    );
    Ok(marker)
}

/// Cancel the pending restore of `dir`: the marker goes first (so a crash can't leave a marker without its files),
/// then the staged files. `false` if there was none.
pub fn cancel(dir: &Path) -> crate::Result<bool> {
    let marker = dir.join(MARKER);
    let existed = match std::fs::remove_file(&marker) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(e).with_context(|| format!("removing {}", marker.display())),
    };
    sync_dir(dir);
    let staged = dir.join(STAGED_DIR);
    if staged.exists() {
        std::fs::remove_dir_all(&staged).with_context(|| format!("removing {}", staged.display()))?;
    }
    if existed {
        tracing::info!("restore: the staged restore was canceled");
    }
    Ok(existed)
}

/// Apply the pending restore of `dir`, if any (see the module docs). Call it with the data directory locked and
/// before opening the database. `Ok(None)`: nothing to do (no marker, or one that failed before). If the staged files
/// don't match their checksums, nothing is replaced: the error is recorded in the marker and logged, and `Ok(None)`
/// is returned so the server starts on its current data.
pub fn apply_pending(dir: &Path, storage_dir: Option<PathBuf>) -> crate::Result<Option<backup::RestoreReport>> {
    let staging = dir.join(STAGED_DIR);
    let Some(mut marker) = pending(dir)? else {
        if staging.exists() {
            // A cancel interrupted after removing the marker.
            std::fs::remove_dir_all(&staging).with_context(|| format!("removing {}", staging.display()))?;
        }
        return Ok(None);
    };
    if marker.error.is_some() {
        tracing::error!(
            "restore: the staged restore of {} could not be applied ({}); cancel it from the dashboard or with \
             `restore --cancel`",
            marker.source,
            marker.error.as_deref().unwrap_or_default()
        );
        return Ok(None);
    }
    // The staged databases must be what was checked. A file already moved into place by an interrupted apply is
    // checked where it is now (nothing has opened it since: this runs before the database opens).
    let check = || -> crate::Result<Manifest> {
        if marker.staged_files.is_empty() {
            anyhow::bail!("the marker lists no staged files");
        }
        for expected in &marker.staged_files {
            let at = if staging.join(&expected.path).exists() {
                staging.join(&expected.path)
            } else {
                dir.join(&expected.path)
            };
            let found = sha256_file(&at)?;
            if found.bytes != expected.bytes || found.sha256 != expected.sha256 {
                anyhow::bail!("{} changed since it was staged", at.display());
            }
        }
        let raw = std::fs::read(staging.join("manifest.json")).context("the staged manifest.json is missing")?;
        serde_json::from_slice(&raw).context("invalid staged manifest.json")
    };
    let manifest = match check() {
        Ok(m) => m,
        Err(e) => {
            let message = format!("{e:#}");
            tracing::error!(
                "restore: not applying the staged restore of {}: {message}; starting on the current data",
                marker.source
            );
            marker.error = Some(message);
            write_marker(dir, &marker)?;
            return Ok(None);
        }
    };
    let previous = dir.join(format!("pre-restore-{}", super::file_timestamp(marker.staged_at)));
    let report = backup::apply_staged(&staging, dir, manifest, storage_dir, &previous)?;
    std::fs::remove_file(dir.join(MARKER)).context("removing the pending-restore marker")?;
    sync_dir(dir);
    std::fs::remove_dir_all(&staging).with_context(|| format!("removing {}", staging.display()))?;
    tracing::warn!(
        source = marker.source,
        ts = marker.ts,
        previous = %previous.display(),
        "restore: applied the staged restore; the previous database files were moved aside"
    );
    Ok(Some(report))
}

// ---------------------------------------------------------------------------------------------------------------------
// Restart on request
// ---------------------------------------------------------------------------------------------------------------------

/// Lets an administrator ask a supervised server to restart (to apply a staged restore): `serve` waits for
/// [`RestartHandle::requested`] next to SIGTERM, shuts down gracefully and exits with the configured code
/// (`--restart-exit-code`). Without a code, requests are refused.
#[derive(Clone, Default)]
pub struct RestartHandle {
    exit_code: Option<i32>,
    tx: Arc<tokio::sync::watch::Sender<bool>>,
}

impl std::fmt::Debug for RestartHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RestartHandle")
            .field("exit_code", &self.exit_code)
            .field("requested", &self.is_requested())
            .finish()
    }
}

impl RestartHandle {
    /// Restarts exit with `exit_code` (`None`: restarting is not available).
    pub fn new(exit_code: Option<i32>) -> Self {
        RestartHandle { exit_code, tx: Default::default() }
    }

    pub fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }

    /// Ask for a restart; `false` when restarting isn't available.
    pub fn request(&self) -> bool {
        if self.exit_code.is_none() {
            return false;
        }
        self.tx.send_replace(true);
        true
    }

    pub fn is_requested(&self) -> bool {
        *self.tx.borrow()
    }

    /// Resolves once a restart was requested.
    pub async fn requested(&self) {
        let mut rx = self.tx.subscribe();
        let _ = rx.wait_for(|r| *r).await;
    }
}

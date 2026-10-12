//! Backups and restore.
//!
//! **Snapshot.** `data.db` is copied with `VACUUM INTO` on a separate connection. That is one read transaction: the
//! copy is a consistent snapshot at a single commit ts, and in WAL mode it never blocks the writer (mutations keep
//! committing while it runs; only WAL checkpoints wait for it). The copy is compact and self-contained (no `-wal`).
//! `aux.db` (the execution log) is copied the same way when asked for.
//!
//! **Archive** (`rxferrix-<UTC time>.tar.zst`, `rxferrix-auto-<UTC time>.tar.zst` for scheduled ones): a zstd-compressed
//! tar with
//!
//! | Entry | When |
//! |---|---|
//! | `data.db` | always |
//! | `aux.db` | `include_aux` (`--with-aux`) |
//! | `storage/<storage id>` | `include_files` (`--with-files`) and local file storage: the blob of every `_storage` document in the snapshot |
//! | `auth_secret.key` | only with `include_secret` (`--include-secret`): it signs sessions and upload URLs, so anyone holding the archive could forge them (it also encrypts the secrets in `_settings`: keep a copy safe either way) |
//! | `manifest.json` | always, last: format, version, creation time, the snapshot's commit ts, and the size and SHA-256 of every other entry |
//!
//! **Targets** ([`BackupTarget`]): a directory (default `<data dir>/backups`) or an S3-compatible bucket
//! (`s3://bucket/prefix`, credentials from the `AWS_*` variables), through the same `object_store` backends as file
//! storage.
//!
//! **Restore** ([`restore`], `rxferrix restore <archive>`) is offline: it takes the data directory lock (so it refuses
//! while a server runs), extracts the archive next to the data, checks every entry against the manifest, runs SQLite's
//! `integrity_check`, opens the database with this binary's tables (schema check) and runs `verify_indexes`. Only then
//! does it move the current `data.db` (with its `-wal`/`-shm`), `aux.db` and, if the archive has one,
//! `auth_secret.key` into `<dir>/pre-restore-<time>/` and the restored files into place. Blobs are added to the local
//! storage directory (existing ones are kept). The two halves are [`prepare`] (extract and check, anywhere) and
//! [`apply_staged`] (swap, offline); `super::restore` uses them to stage a restore from the dashboard while the server
//! runs and apply it at the next start.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use futures::StreamExt;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, ObjectStoreExt, WriteMultipart};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::Table as _;
use crate::id::DocId;
use crate::prelude::*;
use crate::runtime::App;
use crate::storage::{StorageBackend, StorageConfig};

/// `manifest.json`'s `format`.
pub const MANIFEST_FORMAT: &str = "rxferrix-backup";
/// `manifest.json`'s `version`.
pub const MANIFEST_VERSION: u32 = 1;
/// Name prefix of every backup archive; scheduled ones continue with `auto-`.
pub const NAME_PREFIX: &str = "rxferrix-";
const AUTO_PREFIX: &str = "rxferrix-auto-";
/// File extension of backup archives.
pub const EXTENSION: &str = ".tar.zst";
/// Name of the cron that runs scheduled backups.
pub const CRON_NAME: &str = "rxferrix-backup";
const SECRET_FILE: &str = "auth_secret.key";

/// What a backup contains besides `data.db`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase", default)]
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct BackupSpec {
    /// `aux.db`, the execution log.
    pub include_aux: bool,
    /// The blobs of stored files, when they are in a local directory (S3 storage is not copied).
    pub include_files: bool,
    /// `auth_secret.key`. Anyone with the archive can then forge sessions and upload URLs: keep it safe.
    pub include_secret: bool,
}

/// A backup archive in a target.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct BackupInfo {
    /// The archive's file name, e.g. `rxferrix-20261009T120000.000Z.tar.zst`.
    pub name: String,
    #[ts(type = "number")]
    pub bytes: u64,
    /// When it was written (ms since the epoch).
    pub created_at: f64,
    /// Written by a scheduled backup (`rxferrix-auto-*`; only those are rotated).
    pub scheduled: bool,
    /// The commit ts of the snapshot. Embedded in the archive name, so listings report it too; `None` for archives
    /// named before that (or renamed by hand).
    #[ts(type = "number | null")]
    pub ts: Option<u64>,
}

/// One entry of `manifest.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestFile {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

/// `manifest.json`, the last entry of an archive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub format: String,
    pub version: u32,
    /// ms since the epoch.
    pub created_at: f64,
    pub rxferrix_version: String,
    /// The commit ts of the `data.db` snapshot.
    pub ts: u64,
    /// The wall-clock ms of that commit (`_meta.time`; 0 in archives written before it was recorded).
    #[serde(default)]
    pub commit_time: f64,
    /// `"included"`, `"excluded"` or `"remote"` (S3 storage, never copied).
    pub storage: String,
    /// `_storage` documents whose blob was missing when the backup was taken.
    pub missing_blobs: u64,
    pub files: Vec<ManifestFile>,
}

/// Where a snapshot is taken from.
#[derive(Debug, Clone)]
pub struct SnapshotSource {
    /// The data directory (`data.db`, `aux.db`, `auth_secret.key`).
    pub dir: PathBuf,
    /// The local blob directory, or `None` when file storage is remote.
    pub storage_dir: Option<PathBuf>,
}

impl SnapshotSource {
    /// The data directory with the given file storage configuration.
    pub fn new(dir: &Path, storage: &StorageConfig) -> SnapshotSource {
        SnapshotSource { dir: dir.to_path_buf(), storage_dir: local_storage_dir(dir, &storage.backend) }
    }

    fn of_app(app: &App) -> SnapshotSource {
        SnapshotSource::new(app.dir(), app.storage().config())
    }
}

/// The directory blobs live in, for local storage.
pub(crate) fn local_storage_dir(dir: &Path, backend: &StorageBackend) -> Option<PathBuf> {
    match backend {
        StorageBackend::Local(None) => Some(dir.join("storage")),
        StorageBackend::Local(Some(d)) => Some(d.clone()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Writing an archive
// ---------------------------------------------------------------------------------------------------------------------

/// Open an existing database for reading (and `VACUUM INTO`) without creating it.
fn open_existing(path: &Path) -> crate::Result<Connection> {
    anyhow::ensure!(path.exists(), "{} does not exist", path.display());
    crate::db::search::vector::register_extension();
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .with_context(|| format!("opening {}", path.display()))?;
    conn.pragma_update(None, "busy_timeout", 10_000)?;
    Ok(conn)
}

/// Copy the database at `src` to `dest` (which must not exist) as a consistent snapshot, with `VACUUM INTO`.
pub fn snapshot_db(src: &Path, dest: &Path) -> crate::Result<()> {
    let conn = open_existing(src)?;
    let dest_str = dest.to_str().context("backup path is not valid UTF-8")?;
    conn.execute("VACUUM INTO ?1", [dest_str]).with_context(|| format!("snapshotting {}", src.display()))?;
    Ok(())
}

/// A reader that hashes what passes through it.
struct Hashing<R> {
    inner: R,
    hasher: Sha256,
    bytes: u64,
}

impl<R: Read> Read for Hashing<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        self.bytes += n as u64;
        Ok(n)
    }
}

fn append_file<W: Write>(
    tar: &mut tar::Builder<W>,
    path: &Path,
    name: &str,
    mtime: u64,
) -> crate::Result<ManifestFile> {
    let file = std::fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let len = file.metadata()?.len();
    let mut header = tar::Header::new_gnu();
    header.set_size(len);
    header.set_mode(0o600);
    header.set_mtime(mtime);
    header.set_entry_type(tar::EntryType::Regular);
    let mut reader = Hashing { inner: file.take(len), hasher: Sha256::new(), bytes: 0 };
    tar.append_data(&mut header, name, &mut reader).with_context(|| format!("archiving {name}"))?;
    anyhow::ensure!(reader.bytes == len, "{} changed size while it was archived", path.display());
    Ok(ManifestFile {
        path: name.to_string(),
        bytes: len,
        sha256: data_encoding::HEXLOWER.encode(&reader.hasher.finalize()),
    })
}

/// The ids of every `_storage` document in the database at `db`.
fn stored_file_ids(db: &Path) -> crate::Result<Vec<DocId>> {
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let number: Option<u32> = conn
        .query_row("SELECT table_number FROM _tables WHERE name = ?1", [crate::storage::StoredFile::NAME], |r| r.get(0))
        .optional()?;
    let Some(number) = number else { return Ok(Vec::new()) };
    Ok(crate::db::storage::table_documents(&conn, number)?.into_iter().map(|(id, _)| DocId(id)).collect())
}

/// The commit ts and time of a snapshot copy, after removing its replication journal and state
/// (`crate::db::journal::scrub`): a backup describes the data, not a replication stream, and a database restored from
/// it starts a new replication generation.
fn prepare_copy(db: &Path) -> crate::Result<(u64, f64)> {
    let conn = Connection::open(db)?;
    let ts = crate::db::storage::read_meta_u64(&conn, crate::db::storage::META_TS)?;
    let time = crate::db::storage::read_meta_f64(&conn, crate::db::storage::META_TIME)?;
    let journaled = crate::db::journal::stats(&conn).ok().flatten().is_some();
    crate::db::journal::scrub(&conn)?;
    if journaled {
        // Unshipped journal rows can be large: don't archive their free pages.
        conn.execute_batch("VACUUM")?;
    }
    Ok((ts, time))
}

/// Write a backup archive of `source` to `out` (blocking). The archive is written to a temporary file next to `out`
/// and renamed into place when complete.
pub fn write_archive(source: &SnapshotSource, spec: &BackupSpec, out: &Path, now_ms: f64) -> crate::Result<Manifest> {
    let parent = out.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let work = tempfile::Builder::new().prefix(".rxferrix-backup-").tempdir_in(&source.dir)?;
    let data = work.path().join("data.db");
    snapshot_db(&source.dir.join("data.db"), &data)?;
    let (ts, commit_time) = prepare_copy(&data)?;
    let aux_src = source.dir.join("aux.db");
    let aux = if spec.include_aux && aux_src.exists() {
        let aux = work.path().join("aux.db");
        snapshot_db(&aux_src, &aux)?;
        Some(aux)
    } else {
        None
    };

    let partial = tempfile::Builder::new().prefix(".rxferrix-backup-").suffix(".partial").tempfile_in(parent)?;
    let mtime = (now_ms / 1000.0) as u64;
    let encoder = zstd::Encoder::new(partial.as_file(), 3)?;
    let mut tar = tar::Builder::new(encoder);
    let mut files = vec![append_file(&mut tar, &data, "data.db", mtime)?];
    if let Some(aux) = &aux {
        files.push(append_file(&mut tar, aux, "aux.db", mtime)?);
    }
    let mut missing_blobs = 0;
    let storage = match (&source.storage_dir, spec.include_files) {
        (None, _) => "remote",
        (Some(_), false) => "excluded",
        (Some(dir), true) => {
            for id in stored_file_ids(&data)? {
                let name = id.encode();
                let blob = dir.join(&name);
                if blob.is_file() {
                    files.push(append_file(&mut tar, &blob, &format!("storage/{name}"), mtime)?);
                } else {
                    missing_blobs += 1;
                }
            }
            if missing_blobs > 0 {
                tracing::warn!(
                    "backup: {missing_blobs} stored file(s) have no blob; their documents are backed up anyway"
                );
            }
            "included"
        }
    };
    if spec.include_secret {
        let secret = source.dir.join(SECRET_FILE);
        if secret.exists() {
            tracing::warn!(
                "backup: including {SECRET_FILE}: anyone holding this archive can forge sessions and upload URLs"
            );
            files.push(append_file(&mut tar, &secret, SECRET_FILE, mtime)?);
        }
    }
    let manifest = Manifest {
        format: MANIFEST_FORMAT.into(),
        version: MANIFEST_VERSION,
        created_at: now_ms,
        rxferrix_version: env!("CARGO_PKG_VERSION").into(),
        ts,
        commit_time,
        storage: storage.into(),
        missing_blobs,
        files,
    };
    let json = serde_json::to_vec_pretty(&manifest)?;
    let mut header = tar::Header::new_gnu();
    header.set_size(json.len() as u64);
    header.set_mode(0o600);
    header.set_mtime(mtime);
    header.set_entry_type(tar::EntryType::Regular);
    tar.append_data(&mut header, "manifest.json", json.as_slice())?;
    let encoder = tar.into_inner()?;
    let file = encoder.finish()?;
    file.sync_all()?;
    partial.persist(out).with_context(|| format!("writing {}", out.display()))?;
    Ok(manifest)
}

// ---------------------------------------------------------------------------------------------------------------------
// Targets
// ---------------------------------------------------------------------------------------------------------------------

/// Where backups are kept: a directory or an S3-compatible bucket (with a key prefix).
#[derive(Clone)]
pub struct BackupTarget {
    store: Arc<dyn ObjectStore>,
    prefix: String,
    /// What it was parsed from (for messages).
    pub location: String,
}

impl std::fmt::Debug for BackupTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackupTarget").field("location", &self.location).finish()
    }
}

impl BackupTarget {
    /// A directory (created if missing), `file:///dir`, or `s3://bucket/prefix`. A relative directory is relative to
    /// the working directory.
    pub fn parse(location: &str, data_dir: &Path) -> crate::Result<BackupTarget> {
        let backend = StorageBackend::parse(location).context("invalid backup target")?;
        let config = StorageConfig { backend, ..Default::default() };
        let (store, prefix, _) = config.build_store(data_dir)?;
        Ok(BackupTarget { store, prefix, location: location.to_string() })
    }

    /// `<data dir>/backups`.
    pub fn default_for(data_dir: &Path) -> crate::Result<BackupTarget> {
        let dir = data_dir.join("backups");
        Self::parse(dir.to_str().context("data dir is not valid UTF-8")?, data_dir)
    }

    /// The target configured for `app`: `AppOptions::ops.backup_target` (`--backup-to`), else the destination of the
    /// `backups` setting (the dashboard; `crate::settings::backups`), else `<data dir>/backups`.
    pub fn for_app(app: &App) -> crate::Result<BackupTarget> {
        if let Some(t) = app.options().ops.backup_target.as_deref().filter(|t| !t.trim().is_empty()) {
            return Self::parse(t, app.dir());
        }
        match app.backup_settings() {
            Some(settings) => settings.target(app.dir()),
            None => Self::default_for(app.dir()),
        }
    }

    /// Check that archives can be stored here: write a small object, find it in a listing, delete it.
    pub async fn probe(&self) -> crate::Result<()> {
        let name = format!(".rxferrix-probe-{}", rand::random::<u64>());
        let path = self.path(&name);
        self.store.put(&path, b"rxferrix backup destination check".to_vec().into()).await.context("writing")?;
        let head = self.store.head(&path).await.context("reading it back");
        let deleted = self.store.delete(&path).await.context("deleting it");
        head?;
        deleted?;
        Ok(())
    }

    /// Any object store, with archives under `prefix` (tests, custom setups).
    pub fn object_store(store: Arc<dyn ObjectStore>, prefix: impl Into<String>) -> BackupTarget {
        let prefix = prefix.into().trim_matches('/').to_string();
        BackupTarget { location: format!("{store}/{prefix}"), store, prefix }
    }

    fn path(&self, name: &str) -> ObjectPath {
        if self.prefix.is_empty() {
            ObjectPath::from(name)
        } else {
            ObjectPath::from(format!("{}/{name}", self.prefix))
        }
    }

    /// The same store with archives under `<prefix>/<sub>` (replication keeps each generation's snapshots there).
    pub fn child(&self, sub: &str) -> BackupTarget {
        let sub = sub.trim_matches('/');
        let prefix = if self.prefix.is_empty() { sub.to_string() } else { format!("{}/{sub}", self.prefix) };
        BackupTarget {
            store: self.store.clone(),
            location: format!("{}/{sub}", self.location.trim_end_matches('/')),
            prefix,
        }
    }

    /// The object store and the path of `rel` under the prefix (`crate::ops::replicate`).
    pub(crate) fn raw(&self, rel: &str) -> (&Arc<dyn ObjectStore>, ObjectPath) {
        (&self.store, self.path(rel))
    }

    /// Upload a finished archive file as `name`.
    pub async fn upload_file(&self, file: &Path, name: &str) -> crate::Result<()> {
        let upload = self.store.put_multipart(&self.path(name)).await.context("starting the backup upload")?;
        let mut writer = WriteMultipart::new_with_chunk_size(upload, 8 << 20);
        let mut f = tokio::fs::File::open(file).await?;
        let mut buf = vec![0u8; 1 << 20];
        let result: crate::Result<()> = async {
            loop {
                let n = tokio::io::AsyncReadExt::read(&mut f, &mut buf).await?;
                if n == 0 {
                    return Ok(());
                }
                writer.wait_for_capacity(4).await?;
                writer.write(&buf[..n]);
            }
        }
        .await;
        match result {
            Ok(()) => {
                writer.finish().await.context("finishing the backup upload")?;
                Ok(())
            }
            Err(e) => {
                let _ = writer.abort().await;
                Err(e)
            }
        }
    }

    /// Every archive in the target, newest first.
    pub async fn list(&self) -> crate::Result<Vec<BackupInfo>> {
        let prefix = (!self.prefix.is_empty()).then(|| ObjectPath::from(self.prefix.as_str()));
        let mut listing = self.store.list(prefix.as_ref());
        let mut out = Vec::new();
        while let Some(meta) = listing.next().await {
            let meta = meta.context("listing backups")?;
            let Some(name) = meta.location.filename() else { continue };
            if meta.location != self.path(name) || validate_name(name).is_err() {
                continue;
            }
            out.push(BackupInfo {
                name: name.to_string(),
                bytes: meta.size,
                created_at: meta.last_modified.timestamp_millis() as f64,
                scheduled: name.starts_with(AUTO_PREFIX),
                ts: ts_from_name(name),
            });
        }
        // Names embed the UTC time, so they sort chronologically.
        out.sort_by(|a, b| b.name.cmp(&a.name));
        Ok(out)
    }

    /// Open an archive for download: its size and a stream of its bytes.
    pub async fn open(
        &self,
        name: &str,
    ) -> crate::Result<Option<(u64, futures::stream::BoxStream<'static, object_store::Result<axum::body::Bytes>>)>>
    {
        validate_name(name)?;
        match self.store.get(&self.path(name)).await {
            Ok(r) => Ok(Some((r.meta.size, r.into_stream()))),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e).context("reading a backup"),
        }
    }

    /// Download an archive to a local file.
    pub async fn download(&self, name: &str, dest: &Path) -> crate::Result<()> {
        let (_, mut stream) =
            self.open(name).await?.with_context(|| format!("backup {name} not found in {}", self.location))?;
        let mut f = tokio::fs::File::create(dest).await?;
        while let Some(chunk) = stream.next().await {
            tokio::io::AsyncWriteExt::write_all(&mut f, &chunk?).await?;
        }
        tokio::io::AsyncWriteExt::flush(&mut f).await?;
        Ok(())
    }

    /// Delete an archive. `false` if it didn't exist.
    pub async fn delete(&self, name: &str) -> crate::Result<bool> {
        validate_name(name)?;
        let path = self.path(name);
        match self.store.head(&path).await {
            Err(object_store::Error::NotFound { .. }) => return Ok(false),
            Err(e) => return Err(e).context("deleting a backup"),
            Ok(_) => {}
        }
        self.store.delete(&path).await.context("deleting a backup")?;
        Ok(true)
    }

    /// Keep the newest `keep` scheduled archives (`rxferrix-auto-*`) and delete the rest. Returns the deleted names.
    pub async fn rotate(&self, keep: usize) -> crate::Result<Vec<String>> {
        let scheduled: Vec<BackupInfo> = self.list().await?.into_iter().filter(|b| b.scheduled).collect();
        let mut deleted = Vec::new();
        for old in scheduled.into_iter().skip(keep) {
            self.delete(&old.name).await?;
            deleted.push(old.name);
        }
        Ok(deleted)
    }
}

/// Archive names are `rxferrix-<letters, digits, '.', '-', '_'>.tar.zst`: never a path.
pub fn validate_name(name: &str) -> crate::Result<()> {
    let ok = name.starts_with(NAME_PREFIX)
        && name.ends_with(EXTENSION)
        && name.len() <= 200
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        && !name.contains("..");
    if !ok {
        return Err(
            UserError::new("invalid_name", format!("`{name}` is not a backup name (rxferrix-….tar.zst)")).into()
        );
    }
    Ok(())
}

/// The archive name for a backup made at `now_ms`.
pub fn archive_name(now_ms: f64, scheduled: bool) -> String {
    let prefix = if scheduled { AUTO_PREFIX } else { NAME_PREFIX };
    format!("{prefix}{}{EXTENSION}", super::file_timestamp(now_ms))
}

/// The stored name of a backup made at `now_ms` whose snapshot is at commit `ts`: `rxferrix-<time>-ts<ts>.tar.zst`. The
/// ts in the name lets [`BackupTarget::list`] report it without opening archives (cheap on S3 too); names still sort
/// chronologically since the time comes first.
pub fn archive_name_with_ts(now_ms: f64, scheduled: bool, ts: u64) -> String {
    let base = archive_name(now_ms, scheduled);
    format!("{}-ts{ts}{EXTENSION}", base.trim_end_matches(EXTENSION))
}

/// The snapshot ts embedded in a backup name by [`archive_name_with_ts`], if any (older names have none).
pub fn ts_from_name(name: &str) -> Option<u64> {
    let stem = name.strip_suffix(EXTENSION)?;
    let (_, ts) = stem.rsplit_once("-ts")?;
    ts.parse().ok()
}

// ---------------------------------------------------------------------------------------------------------------------
// Backups of a running app
// ---------------------------------------------------------------------------------------------------------------------

/// One backup at a time per process.
static RUNNING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Take a backup of `app` while it runs and store it in `target`. Writes keep committing meanwhile.
pub async fn create(app: &App, spec: &BackupSpec, target: &BackupTarget, scheduled: bool) -> crate::Result<BackupInfo> {
    Ok(create_with_manifest(app, spec, target, scheduled).await?.0)
}

/// [`create`], also returning the archive's manifest.
pub(crate) async fn create_with_manifest(
    app: &App,
    spec: &BackupSpec,
    target: &BackupTarget,
    scheduled: bool,
) -> crate::Result<(BackupInfo, Manifest)> {
    let _one = RUNNING.lock().await;
    let now = app.db().clock().now_ms();
    let name = archive_name(now, scheduled);
    let source = SnapshotSource::of_app(app);
    let work = tempfile::Builder::new().prefix(".rxferrix-backup-").tempdir_in(app.dir())?;
    let local = work.path().join(&name);
    let (spec2, local2, source2) = (*spec, local.clone(), source.clone());
    let manifest = tokio::task::spawn_blocking(move || write_archive(&source2, &spec2, &local2, now)).await??;
    let bytes = std::fs::metadata(&local)?.len();
    let name = archive_name_with_ts(now, scheduled, manifest.ts);
    target.upload_file(&local, &name).await.with_context(|| format!("storing the backup in {}", target.location))?;
    tracing::info!(name, bytes, ts = manifest.ts, target = %target.location, "backup written");
    Ok((BackupInfo { name, bytes, created_at: now, scheduled, ts: Some(manifest.ts) }, manifest))
}

/// The settings of scheduled backups: the args of the [`run_scheduled`] cron.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct ScheduledBackup {
    /// The target (`None`: the app's backup target, `AppOptions::ops.backup_target` or `<data dir>/backups`).
    pub to: Option<String>,
    /// How many scheduled archives to keep.
    pub keep: u32,
    pub include_aux: bool,
    pub include_files: bool,
}

impl ScheduledBackup {
    /// The interval cron that runs these backups every `every` (named [`CRON_NAME`]; it shows on the dashboard's
    /// Scheduler page).
    pub fn cron(self, every: std::time::Duration) -> crate::scheduler::CronSpec {
        crate::scheduler::CronSpec::interval::<run_scheduled>(CRON_NAME, every, (self,))
    }
}

/// A scheduled backup: take one, store it, then delete scheduled archives beyond `keep`.
#[internal_action]
pub async fn run_scheduled(ctx: &ActionCtx, schedule: ScheduledBackup) -> Result<BackupInfo> {
    let app = &ctx.app;
    let target = match schedule.to.as_deref().filter(|t| !t.trim().is_empty()) {
        Some(t) => BackupTarget::parse(t, app.dir())?,
        None => BackupTarget::for_app(app)?,
    };
    let spec =
        BackupSpec { include_aux: schedule.include_aux, include_files: schedule.include_files, include_secret: false };
    let info = create(app, &spec, &target, true).await?;
    let deleted = target.rotate(schedule.keep.max(1) as usize).await?;
    if !deleted.is_empty() {
        crate::log::info!("deleted {} old scheduled backup(s): {}", deleted.len(), deleted.join(", "));
    }
    Ok(info)
}

// ---------------------------------------------------------------------------------------------------------------------
// Restore
// ---------------------------------------------------------------------------------------------------------------------

/// Options of [`restore`].
#[derive(Debug, Clone, Default)]
pub struct RestoreOptions {
    /// Where to put restored blobs (default `<dir>/storage`).
    pub storage_dir: Option<PathBuf>,
    /// Skip opening the database with this binary's tables and `verify_indexes` (the integrity check and the
    /// manifest checks still run).
    pub skip_verify: bool,
}

/// What [`restore`] did.
#[derive(Debug, Clone)]
pub struct RestoreReport {
    pub manifest: Manifest,
    /// Where the previous database files were moved (if there were any).
    pub previous: Option<PathBuf>,
    /// Blobs written into the storage directory.
    pub blobs_restored: u64,
}

/// Which archive entries may be restored and where they go.
fn entry_kind(name: &str) -> Option<&'static str> {
    match name {
        "manifest.json" | "data.db" | "aux.db" | SECRET_FILE => Some("file"),
        _ => {
            let blob = name.strip_prefix("storage/")?;
            DocId::decode(blob).ok().map(|_| "blob")
        }
    }
}

/// Extract and check an archive into `staging` (blocking). Returns the manifest.
fn extract(archive: &Path, staging: &Path) -> crate::Result<Manifest> {
    let file = std::fs::File::open(archive).with_context(|| format!("opening {}", archive.display()))?;
    let decoder = zstd::Decoder::new(file).context("not a zstd archive")?;
    let mut tar = tar::Archive::new(decoder);
    let mut seen: BTreeMap<String, ManifestFile> = BTreeMap::new();
    std::fs::create_dir_all(staging.join("storage"))?;
    for entry in tar.entries().context("reading the archive")? {
        let mut entry = entry.context("reading the archive")?;
        let name = entry.path()?.to_string_lossy().into_owned();
        if entry.header().entry_type() != tar::EntryType::Regular || entry_kind(&name).is_none() {
            bail!("unexpected entry `{name}` in the archive: not a rxferrix backup?");
        }
        if seen.contains_key(&name) {
            bail!("duplicate entry `{name}` in the archive");
        }
        let dest = staging.join(&name);
        let mut out = std::fs::File::create(&dest)?;
        let mut reader = Hashing { inner: &mut entry, hasher: Sha256::new(), bytes: 0 };
        std::io::copy(&mut reader, &mut out)?;
        out.sync_all()?;
        let sha256 = data_encoding::HEXLOWER.encode(&reader.hasher.finalize());
        seen.insert(name.clone(), ManifestFile { path: name, bytes: reader.bytes, sha256 });
    }
    let raw = std::fs::read(staging.join("manifest.json"))
        .context("the archive has no manifest.json: not a rxferrix backup?")?;
    let manifest: Manifest = serde_json::from_slice(&raw).context("invalid manifest.json")?;
    if manifest.format != MANIFEST_FORMAT {
        bail!("not a rxferrix backup (format `{}`)", manifest.format);
    }
    if manifest.version > MANIFEST_VERSION {
        bail!("backup format version {} is newer than this binary supports ({MANIFEST_VERSION})", manifest.version);
    }
    seen.remove("manifest.json");
    for file in &manifest.files {
        match seen.remove(&file.path) {
            Some(found) if found == *file => {}
            Some(_) => bail!("`{}` does not match its checksum in the manifest: the archive is corrupt", file.path),
            None => bail!("`{}` is listed in the manifest but missing from the archive", file.path),
        }
    }
    if let Some(extra) = seen.keys().next() {
        bail!("`{extra}` is in the archive but not in its manifest");
    }
    if !manifest.files.iter().any(|f| f.path == "data.db") {
        bail!("the archive has no data.db");
    }
    for db in ["data.db", "aux.db"] {
        let path = staging.join(db);
        if path.exists() {
            let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            let result: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
            if result != "ok" {
                bail!("{db} in the archive fails SQLite's integrity check: {result}");
            }
            // RxFerrix never creates triggers or views; one in a backup was put there by hand, and would run (or be
            // read) with every later write.
            let extra: Option<String> = conn
                .query_row(
                    "SELECT type || ' ' || name FROM sqlite_master WHERE type IN ('trigger', 'view') LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(extra) = extra {
                bail!("{db} in the archive contains a {extra}, which rxferrix never creates: refusing it");
            }
        }
    }
    Ok(manifest)
}

/// Open the restored database with this binary's tables and check every index. Leaves no `-wal`/`-shm` behind.
pub(crate) async fn verify(staging: &Path) -> crate::Result<()> {
    let tables = crate::runtime::registered_tables();
    let dir = staging.to_path_buf();
    let db = tokio::task::spawn_blocking(move || crate::db::Database::open_with(&dir, &tables, &Default::default()))
        .await?
        .context("the backup can't be opened by this binary")?;
    let problems = db.verify_indexes().await?;
    drop(db);
    if !problems.is_empty() {
        bail!("the backup's indexes are inconsistent: {}", problems.join("; "));
    }
    // Fold anything left in the WAL back into data.db before it moves. The vector index thread of the dropped database
    // (`db::search::ann`) may hold its reader for a moment longer, and leaving WAL mode needs every other connection
    // gone (SQLite doesn't wait for that): retry for a while.
    let path = staging.join("data.db");
    tokio::task::spawn_blocking(move || -> crate::Result<()> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let attempt = (|| -> rusqlite::Result<()> {
                let conn = Connection::open(&path)?;
                conn.busy_timeout(std::time::Duration::from_secs(5))?;
                conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
                conn.pragma_update(None, "journal_mode", "DELETE")
            })();
            match attempt {
                Ok(()) => return Ok(()),
                Err(rusqlite::Error::SqliteFailure(e, _))
                    if e.code == rusqlite::ErrorCode::DatabaseBusy && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                Err(e) => return Err(e).context("closing the restored database's WAL"),
            }
        }
    })
    .await?
}

/// Extract `archive` into the empty directory `staging` and check it: the manifest (every entry's size and SHA-256,
/// nothing missing or extra), SQLite's `integrity_check`, no hand-made triggers or views, and (unless `skip_verify`)
/// opening it with this binary's tables and `verify_indexes`. Leaves `staging/data.db` in WAL mode, ready to be moved
/// into a data directory by [`apply_staged`]. Doesn't touch any data directory: safe while a server runs.
pub async fn prepare(archive: &Path, staging: &Path, skip_verify: bool) -> crate::Result<Manifest> {
    let (archive2, staging2) = (archive.to_path_buf(), staging.to_path_buf());
    let manifest = tokio::task::spawn_blocking(move || -> crate::Result<Manifest> {
        let manifest = extract(&archive2, &staging2)?;
        // Archives are written without replication state; one added by hand must not leak into the restored database.
        crate::db::journal::scrub(&Connection::open(staging2.join("data.db"))?)?;
        Ok(manifest)
    })
    .await??;
    if !skip_verify {
        verify(staging).await?;
    }
    // WAL mode again, as rxferrix opens it (the next open would switch anyway).
    let path = staging.join("data.db");
    tokio::task::spawn_blocking(move || -> crate::Result<()> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        conn.pragma_update(None, "journal_mode", "WAL").context("switching the restored database to WAL")?;
        Ok(())
    })
    .await??;
    Ok(manifest)
}

/// Move the files [`prepare`] checked in `staging` into the data directory `dir`, the current ones aside into
/// `previous` (blocking; `dir` must be locked, no server may have the database open).
///
/// Every step can be repeated after a crash: a current file is moved aside only while its restored counterpart is
/// still in `staging` (so a restored file already in place is never moved aside), and a restored file is moved only
/// if it is still in `staging`. Running it again finishes what an interrupted run started.
pub fn apply_staged(
    staging: &Path,
    dir: &Path,
    manifest: Manifest,
    storage_dir: Option<PathBuf>,
    previous: &Path,
) -> crate::Result<RestoreReport> {
    let staged = |name: &str| staging.join(name).exists();
    let mut moved_aside = Vec::new();
    let mut aside_names: Vec<String> = Vec::new();
    if staged("data.db") {
        aside_names.extend(["data.db", "data.db-wal", "data.db-shm", "data.db-journal"].map(String::from));
        // Persisted HNSW vector indexes (`vector_index_<id>.hnsw`) describe the old database: rebuilt from the
        // restored one at startup.
        for entry in std::fs::read_dir(dir)? {
            let name = entry?.file_name().to_string_lossy().into_owned();
            if name.starts_with("vector_index_") && name.ends_with(".hnsw") {
                aside_names.push(name);
            }
        }
    }
    if staged("aux.db") {
        aside_names.extend(["aux.db", "aux.db-wal", "aux.db-shm", "aux.db-journal"].map(String::from));
    }
    if staged(SECRET_FILE) {
        aside_names.push(SECRET_FILE.into());
    }
    for name in aside_names {
        let path = dir.join(&name);
        if path.exists() {
            std::fs::create_dir_all(previous)?;
            std::fs::rename(&path, previous.join(&name)).with_context(|| format!("moving {} aside", path.display()))?;
            moved_aside.push(name);
        }
    }
    for name in ["data.db", "aux.db", SECRET_FILE] {
        if staged(name) {
            std::fs::rename(staging.join(name), dir.join(name)).with_context(|| format!("restoring {name}"))?;
        }
    }
    let mut blobs_restored = 0;
    let blobs: Vec<&ManifestFile> = manifest.files.iter().filter(|f| f.path.starts_with("storage/")).collect();
    if !blobs.is_empty() {
        let storage = storage_dir.unwrap_or_else(|| dir.join("storage"));
        std::fs::create_dir_all(&storage)?;
        for blob in blobs {
            let name = &blob.path["storage/".len()..];
            let (src, dest) = (staging.join(&blob.path), storage.join(name));
            if src.exists() && !dest.exists() {
                std::fs::rename(&src, &dest)
                    .or_else(|_| std::fs::copy(&src, &dest).map(|_| ()))
                    .with_context(|| format!("restoring the blob {name}"))?;
                blobs_restored += 1;
            }
        }
    }
    let previous = (!moved_aside.is_empty() || previous.exists()).then(|| previous.to_path_buf());
    Ok(RestoreReport { manifest, previous, blobs_restored })
}

/// Restore `archive` into the data directory `dir` (offline). See the module docs.
pub async fn restore(archive: &Path, dir: &Path, options: &RestoreOptions) -> crate::Result<RestoreReport> {
    let _lock = super::DirLock::acquire(dir)?;
    let staging = tempfile::Builder::new().prefix(".rxferrix-restore-").tempdir_in(dir)?;
    let manifest = prepare(archive, staging.path(), options.skip_verify).await?;
    let previous = dir.join(format!("pre-restore-{}", super::file_timestamp(crate::db::wall_clock_ms())));
    apply_staged(staging.path(), dir, manifest, options.storage_dir.clone(), &previous)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        let n = archive_name(1_760_011_200_123.0, false);
        assert_eq!(n, "rxferrix-20251009T120000.123Z.tar.zst");
        assert!(validate_name(&n).is_ok());
        assert!(archive_name(0.0, true).starts_with("rxferrix-auto-"));
        let with_ts = archive_name_with_ts(1_760_011_200_123.0, true, 4321);
        assert!(
            validate_name(&with_ts).is_ok()
                && with_ts.starts_with("rxferrix-auto-")
                && with_ts.ends_with("-ts4321.tar.zst")
        );
        assert_eq!(ts_from_name(&with_ts), Some(4321));
        assert_eq!(ts_from_name(&n), None, "older names have no ts");
        for bad in
            ["../rxferrix-x.tar.zst", "rxferrix-a/b.tar.zst", "x.tar.zst", "rxferrix-x.tar", "rxferrix-..tar.zst"]
        {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
        assert_eq!(entry_kind("storage/../data.db"), None);
        assert_eq!(entry_kind("/etc/passwd"), None);
        assert_eq!(entry_kind("data.db"), Some("file"));
    }

    /// An archive whose (otherwise valid) database has a trigger, which rxferrix never creates, is refused.
    #[test]
    fn databases_with_triggers_are_refused() {
        let work = tempfile::tempdir().unwrap();
        let db = work.path().join("data.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE documents (id BLOB PRIMARY KEY, json TEXT);
             CREATE TABLE stolen (json TEXT);
             CREATE TRIGGER copy_out AFTER INSERT ON documents BEGIN INSERT INTO stolen VALUES (new.json); END;",
        )
        .unwrap();
        drop(conn);
        let archive = work.path().join("rxferrix-x.tar.zst");
        {
            let out = std::fs::File::create(&archive).unwrap();
            let mut tar = tar::Builder::new(zstd::Encoder::new(out, 3).unwrap());
            let file = append_file(&mut tar, &db, "data.db", 0).unwrap();
            let manifest = Manifest {
                format: MANIFEST_FORMAT.into(),
                version: MANIFEST_VERSION,
                created_at: 0.0,
                rxferrix_version: "test".into(),
                ts: 0,
                commit_time: 0.0,
                storage: "excluded".into(),
                missing_blobs: 0,
                files: vec![file],
            };
            let json = serde_json::to_vec(&manifest).unwrap();
            let mut header = tar::Header::new_gnu();
            header.set_size(json.len() as u64);
            header.set_mode(0o600);
            header.set_entry_type(tar::EntryType::Regular);
            tar.append_data(&mut header, "manifest.json", json.as_slice()).unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        }
        let staging = work.path().join("staging");
        let err = extract(&archive, &staging).unwrap_err().to_string();
        assert!(err.contains("trigger copy_out") && err.contains("refusing"), "{err}");
    }
}

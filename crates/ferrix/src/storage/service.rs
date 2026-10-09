//! [`StorageService`]: the blob store of an [`App`] (`app.storage()`), upload URL signing, signed file URLs,
//! streaming writes with hashing and a size limit, the `_storage` document writes, the thumbnail cache, and the
//! orphan sweep.

use std::collections::HashSet;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use axum::body::Bytes;
use data_encoding::{BASE64URL_NOPAD, HEXLOWER};
use futures::{Stream, StreamExt};
use hmac::{Hmac, KeyInit, Mac};
use object_store::path::Path as ObjectPath;
use object_store::{GetOptions, GetRange, GetResult, ObjectStore, ObjectStoreExt, PutPayload, WriteMultipart};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::StoredFile;
use super::config::{StorageConfig, upload_rule};
use crate::auth::rate_limit::RateLimiter;
use crate::id::{DocId, Id};
use crate::runtime::App;
use crate::schema::Table;

/// Bodies up to this size are buffered and written with one `put`; larger ones use a multipart upload.
const SINGLE_PUT_MAX: usize = 8 * 1024 * 1024;
/// Part size of multipart uploads (S3 needs at least 5 MiB per part but the last).
const PART_SIZE: usize = 8 * 1024 * 1024;
/// Parts uploaded concurrently per multipart upload.
const PART_CONCURRENCY: usize = 4;

/// Raw SQLite table (in `data.db`, outside the catalog) of upload tokens that have been used, so each upload URL
/// works once. Rows are written in the same transaction as the `_storage` document and kept until the token expires.
const USED_TOKENS_TABLE: &str = "_storage_used_upload_tokens";

/// The claims of an upload URL's token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct UploadToken {
    /// Random nonce (128 bits, base64url): makes the token single-use.
    pub n: String,
    /// Expiry, seconds since the epoch.
    pub e: u64,
    /// Who generated the URL (`Identity::log_summary`), recorded as the file's `createdBy`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub b: Option<String>,
}

/// Why an upload token was refused. The messages are safe to show to clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokenProblem {
    Invalid,
    Expired,
    Used,
    InProgress,
}

impl TokenProblem {
    pub(crate) fn code(self) -> &'static str {
        match self {
            TokenProblem::Invalid => "invalid_upload_url",
            TokenProblem::Expired => "upload_url_expired",
            TokenProblem::Used => "upload_url_used",
            TokenProblem::InProgress => "upload_in_progress",
        }
    }

    pub(crate) fn status(self) -> u16 {
        match self {
            TokenProblem::InProgress => 409,
            _ => 403,
        }
    }
}

impl fmt::Display for TokenProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TokenProblem::Invalid => "The upload URL is invalid",
            TokenProblem::Expired => "The upload URL has expired; generate a new one",
            TokenProblem::Used => "The upload URL has already been used; generate a new one",
            TokenProblem::InProgress => "An upload with this URL is already in progress",
        })
    }
}

/// Why writing a blob failed.
#[derive(Debug)]
pub(crate) enum WriteError {
    /// More than the allowed size was sent.
    TooLarge,
    /// The request body failed (e.g. the client disconnected).
    Body(String),
    /// The object store failed.
    Store(anyhow::Error),
}

/// Why a signed file URL was refused. The messages are safe to show to clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SignatureProblem {
    /// Private mode and no signature.
    Required,
    /// Malformed or wrongly signed.
    Invalid,
    Expired,
}

impl SignatureProblem {
    pub(crate) fn code(self) -> &'static str {
        match self {
            SignatureProblem::Required => "signature_required",
            SignatureProblem::Invalid => "invalid_signature",
            SignatureProblem::Expired => "url_expired",
        }
    }
}

impl fmt::Display for SignatureProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            SignatureProblem::Required => "Files are private: use a signed URL",
            SignatureProblem::Invalid => "The file URL's signature is invalid",
            SignatureProblem::Expired => "The file URL has expired",
        })
    }
}

/// What [`StorageService::sweep`] did (also the result of the `_storage_sweep` system cron's jobs).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct SweepStats {
    /// Blobs looked at.
    #[ts(type = "number")]
    pub scanned: u64,
    /// Blobs deleted because no `_storage` document refers to them.
    #[ts(type = "number")]
    pub deleted: u64,
    /// Cached thumbnails deleted because their file is gone.
    #[ts(type = "number")]
    pub thumbnails: u64,
}

/// File storage of an [`App`]: get it with `app.storage()`.
pub struct StorageService {
    pub(crate) config: StorageConfig,
    store: Arc<dyn ObjectStore>,
    prefix: String,
    key: [u8; 32],
    /// Signs file URLs (`get_signed_url`); derived from `key`.
    file_url_key: [u8; 32],
    base_url: String,
    pub(crate) limiter: RateLimiter,
    /// Nonces of upload tokens whose upload is running (a second concurrent upload with one token is refused).
    tokens_in_flight: Mutex<HashSet<String>>,
    /// Blobs being written whose document doesn't exist yet (never swept).
    blobs_in_flight: Mutex<HashSet<DocId>>,
    /// Thumbnails generated at the same time.
    pub(crate) thumbnail_permits: tokio::sync::Semaphore,
}

impl fmt::Debug for StorageService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StorageService").field("store", &self.store.to_string()).field("prefix", &self.prefix).finish()
    }
}

fn now_s() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Removes an entry from an in-flight set when dropped.
pub(crate) struct InFlight<'a, T: std::hash::Hash + Eq> {
    set: &'a Mutex<HashSet<T>>,
    value: Option<T>,
}

impl<T: std::hash::Hash + Eq> Drop for InFlight<'_, T> {
    fn drop(&mut self) {
        if let Some(v) = self.value.take() {
            self.set.lock().unwrap_or_else(|e| e.into_inner()).remove(&v);
        }
    }
}

impl StorageService {
    /// Open the configured backend. `public_url` is the fallback base URL (the auth public URL); `key` signs upload
    /// URLs.
    pub(crate) fn new(
        data_dir: &std::path::Path,
        config: StorageConfig,
        public_url: Option<&str>,
        key: [u8; 32],
    ) -> crate::Result<StorageService> {
        let (store, prefix) = config.build_store(data_dir)?;
        let base_url =
            config.public_url.as_deref().or(public_url).map(|u| u.trim_end_matches('/').to_string()).unwrap_or_default();
        let file_url_key = blake3::derive_key("ferrix storage v1 signed file url HMAC key", &key);
        let thumbnail_permits = std::thread::available_parallelism().map_or(2, |n| n.get().clamp(1, 4));
        Ok(StorageService {
            config,
            store,
            prefix,
            key,
            file_url_key,
            base_url,
            limiter: RateLimiter::default(),
            tokens_in_flight: Mutex::new(HashSet::new()),
            blobs_in_flight: Mutex::new(HashSet::new()),
            thumbnail_permits: tokio::sync::Semaphore::new(thumbnail_permits),
        })
    }

    /// The settings this service runs with.
    pub fn config(&self) -> &StorageConfig {
        &self.config
    }

    /// The underlying object store (blobs are named `<prefix>/<storage id>`).
    pub fn object_store(&self) -> &Arc<dyn ObjectStore> {
        &self.store
    }

    /// The key of a file's blob in the object store.
    pub fn blob_path(&self, id: &str) -> ObjectPath {
        if self.prefix.is_empty() { ObjectPath::from(id) } else { ObjectPath::from(format!("{}/{id}", self.prefix)) }
    }

    /// The serving URL of a file: `<public url>/api/storage/<id>` (root-relative without a public URL). Stable: the
    /// id is an unguessable capability, so the URL is the same in every query and needs no signature. (In private
    /// mode it is refused; see [`StorageService::signed_url`].)
    pub fn file_url(&self, id: &str) -> String {
        format!("{}/api/storage/{id}", self.base_url)
    }

    /// A signed file URL valid until `expires` (seconds since the epoch):
    /// `<public url>/api/storage/<id>?exp=<expires>&sig=<base64url HMAC-SHA256(id "\n" expires)>`, keyed by a key
    /// derived from the storage key. Query parameters such as `thumb`, `fit` and `download` may be appended: the
    /// signature covers the file and the expiry, not the variant.
    pub fn signed_url(&self, id: &str, expires: u64) -> String {
        format!("{}?exp={expires}&sig={}", self.file_url(id), self.file_signature(id, expires))
    }

    fn file_mac(&self, id: &str, expires: u64) -> Hmac<Sha256> {
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(&self.file_url_key).expect("HMAC takes any key length");
        mac.update(id.as_bytes());
        mac.update(b"\n");
        mac.update(expires.to_string().as_bytes());
        mac
    }

    fn file_signature(&self, id: &str, expires: u64) -> String {
        BASE64URL_NOPAD.encode(&self.file_mac(id, expires).finalize().into_bytes())
    }

    /// Check a request's `exp` / `sig` for file `id` at `now_s` (constant-time comparison). Without either, the
    /// request is fine unless storage is private.
    pub(crate) fn check_signature(
        &self,
        id: &str,
        exp: Option<&str>,
        sig: Option<&str>,
        now_s: u64,
    ) -> Result<Option<u64>, SignatureProblem> {
        let (exp, sig) = match (exp, sig) {
            (None, None) if self.config.private => return Err(SignatureProblem::Required),
            (None, None) => return Ok(None),
            (Some(exp), Some(sig)) => (exp, sig),
            _ => return Err(SignatureProblem::Invalid),
        };
        let expires: u64 = exp.parse().map_err(|_| SignatureProblem::Invalid)?;
        let sig = BASE64URL_NOPAD.decode(sig.as_bytes()).map_err(|_| SignatureProblem::Invalid)?;
        self.file_mac(id, expires).verify_slice(&sig).map_err(|_| SignatureProblem::Invalid)?;
        if expires <= now_s {
            return Err(SignatureProblem::Expired);
        }
        Ok(Some(expires))
    }

    // -----------------------------------------------------------------------------------------------------------------
    // Thumbnails

    /// The cache directory of a file's thumbnails: `<prefix>/_thumbs/<id>`.
    fn thumbs_dir(&self, id: &str) -> ObjectPath {
        let dir = format!("{}/{id}", super::thumbs::THUMBS_DIR);
        if self.prefix.is_empty() { ObjectPath::from(dir) } else { ObjectPath::from(format!("{}/{dir}", self.prefix)) }
    }

    /// The cache key of one thumbnail variant (`200x200-cover`) of a file.
    pub fn thumbnail_path(&self, id: &str, variant: &str) -> ObjectPath {
        self.thumbs_dir(id).join(variant)
    }

    /// A cached thumbnail, if any.
    pub(crate) async fn cached_thumbnail(&self, id: &str, variant: &str) -> crate::Result<Option<Bytes>> {
        match self.store.get(&self.thumbnail_path(id, variant)).await {
            Ok(result) => Ok(Some(result.bytes().await?)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(anyhow::Error::from(e).context(format!("reading a thumbnail of {id}"))),
        }
    }

    /// Cache a generated thumbnail (a failure is only logged: it is generated again next time).
    pub(crate) async fn cache_thumbnail(&self, id: &str, variant: &str, bytes: Bytes) {
        if let Err(e) = self.store.put(&self.thumbnail_path(id, variant), PutPayload::from(bytes)).await {
            tracing::warn!("storage: caching thumbnail {variant} of {id} failed: {e}");
        }
    }

    /// Delete every cached thumbnail of a file.
    async fn delete_thumbnails(&self, id: &str) -> crate::Result<()> {
        let dir = self.thumbs_dir(id);
        let mut listing = self.store.list(Some(&dir));
        while let Some(meta) = listing.next().await {
            let meta = meta.with_context(|| format!("listing the thumbnails of {id}"))?;
            match self.store.delete(&meta.location).await {
                Ok(()) | Err(object_store::Error::NotFound { .. }) => {}
                Err(e) => return Err(e).with_context(|| format!("deleting a thumbnail of {id}")),
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------------------------------------------------
    // Upload URLs

    /// A new single-use upload URL, valid for `config.upload_url_ttl`; `created_by` becomes the file's `createdBy`.
    pub(crate) fn upload_url(&self, created_by: Option<String>) -> String {
        let nonce: [u8; 16] = rand::random();
        let claims = UploadToken {
            n: BASE64URL_NOPAD.encode(&nonce),
            e: now_s() + self.config.upload_url_ttl.as_secs().max(1),
            b: created_by,
        };
        format!("{}/api/storage/upload?token={}", self.base_url, self.sign(&claims))
    }

    fn mac(&self, data: &[u8]) -> Hmac<Sha256> {
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(&self.key).expect("HMAC takes any key length");
        mac.update(data);
        mac
    }

    pub(crate) fn sign(&self, claims: &UploadToken) -> String {
        let payload = BASE64URL_NOPAD.encode(&serde_json::to_vec(claims).expect("claims serialize"));
        let sig = self.mac(payload.as_bytes()).finalize().into_bytes();
        format!("{payload}.{}", BASE64URL_NOPAD.encode(&sig))
    }

    /// Check an upload token's signature (in constant time) and expiry.
    pub(crate) fn verify(&self, token: &str) -> Result<UploadToken, TokenProblem> {
        let (payload, sig) = token.split_once('.').ok_or(TokenProblem::Invalid)?;
        let sig = BASE64URL_NOPAD.decode(sig.as_bytes()).map_err(|_| TokenProblem::Invalid)?;
        self.mac(payload.as_bytes()).verify_slice(&sig).map_err(|_| TokenProblem::Invalid)?;
        let json = BASE64URL_NOPAD.decode(payload.as_bytes()).map_err(|_| TokenProblem::Invalid)?;
        let claims: UploadToken = serde_json::from_slice(&json).map_err(|_| TokenProblem::Invalid)?;
        if claims.e <= now_s() {
            return Err(TokenProblem::Expired);
        }
        Ok(claims)
    }

    /// Reserve a verified token for one upload: refused when it was already used or another upload with it is
    /// running. The reservation ends when the guard drops; the token is only marked used when the file's document is
    /// committed ([`StorageService::insert_file`]), so a failed upload can be retried with the same URL.
    pub(crate) async fn claim(&self, app: &App, token: &UploadToken) -> Result<InFlight<'_, String>, TokenProblem> {
        if !self.tokens_in_flight.lock().unwrap_or_else(|e| e.into_inner()).insert(token.n.clone()) {
            return Err(TokenProblem::InProgress);
        }
        let guard = InFlight { set: &self.tokens_in_flight, value: Some(token.n.clone()) };
        let used = async {
            let snapshot = app.db().snapshot().await?;
            snapshot.with_conn(|c| token_used(c, &token.n))
        }
        .await;
        match used {
            Ok(false) => Ok(guard),
            Ok(true) => Err(TokenProblem::Used),
            Err(e) => {
                tracing::error!("storage: checking an upload token failed: {e:#}");
                Err(TokenProblem::Invalid)
            }
        }
    }

    // -----------------------------------------------------------------------------------------------------------------
    // Blobs

    /// A fresh storage id, protected from the sweep until the guard drops.
    pub(crate) fn new_id(&self, app: &App) -> crate::Result<(DocId, InFlight<'_, DocId>)> {
        let table = app.db().catalog().table(StoredFile::NAME)?;
        let id = DocId::new_random(table.number);
        self.blobs_in_flight.lock().unwrap_or_else(|e| e.into_inner()).insert(id);
        Ok((id, InFlight { set: &self.blobs_in_flight, value: Some(id) }))
    }

    /// Stream `body` into the blob of `id`, hashing it on the way, and fail with [`WriteError::TooLarge`] as soon as
    /// more than `max` bytes arrive. Returns the hex SHA-256 and the size. The blob only becomes visible in the store
    /// once complete: small bodies are written with one `put`, larger ones with a multipart upload (staged file +
    /// rename for the local filesystem, a multipart upload completed at the end for S3), aborted on failure.
    pub(crate) async fn write_blob<S, E>(&self, id: DocId, mut body: S, max: u64) -> Result<(String, u64), WriteError>
    where
        S: Stream<Item = Result<Bytes, E>> + Unpin,
        E: fmt::Display,
    {
        let path = self.blob_path(&id.encode());
        let mut hasher = Sha256::new();
        let mut size = 0u64;
        let mut buffered: Vec<Bytes> = Vec::new();
        let mut buffered_len = 0usize;
        let mut upload: Option<WriteMultipart> = None;
        let result = async {
            while let Some(chunk) = body.next().await {
                let chunk = chunk.map_err(|e| WriteError::Body(e.to_string()))?;
                size += chunk.len() as u64;
                if size > max {
                    return Err(WriteError::TooLarge);
                }
                hasher.update(&chunk);
                match &mut upload {
                    Some(w) => {
                        w.wait_for_capacity(PART_CONCURRENCY).await.map_err(|e| WriteError::Store(e.into()))?;
                        w.put(chunk);
                    }
                    None => {
                        buffered_len += chunk.len();
                        buffered.push(chunk);
                        if buffered_len >= SINGLE_PUT_MAX {
                            let parts = self.store.put_multipart(&path).await.map_err(|e| WriteError::Store(e.into()))?;
                            let mut w = WriteMultipart::new_with_chunk_size(parts, PART_SIZE);
                            for b in buffered.drain(..) {
                                w.put(b);
                            }
                            upload = Some(w);
                        }
                    }
                }
            }
            Ok(())
        }
        .await;
        match (result, upload) {
            (Ok(()), Some(w)) => {
                w.finish().await.map_err(|e| WriteError::Store(e.into()))?;
            }
            (Ok(()), None) => {
                let payload: PutPayload = buffered.into_iter().collect();
                self.store.put(&path, payload).await.map_err(|e| WriteError::Store(e.into()))?;
            }
            (Err(e), Some(w)) => {
                if let Err(abort) = w.abort().await {
                    tracing::warn!("storage: aborting the upload of {id} failed: {abort}");
                }
                return Err(e);
            }
            (Err(e), None) => return Err(e),
        }
        Ok((HEXLOWER.encode(&hasher.finalize()), size))
    }

    /// Read a blob, optionally a byte range of it.
    pub(crate) async fn read_blob(&self, id: &str, range: Option<std::ops::Range<u64>>) -> object_store::Result<GetResult> {
        let options = GetOptions { range: range.map(GetRange::Bounded), ..Default::default() };
        self.store.get_opts(&self.blob_path(id), options).await
    }

    /// Delete a blob and its cached thumbnails (missing ones are fine).
    pub(crate) async fn delete_blob(&self, id: &str) -> crate::Result<()> {
        match self.store.delete(&self.blob_path(id)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => {}
            Err(e) => return Err(e).with_context(|| format!("deleting the blob of file {id}")),
        }
        self.delete_thumbnails(id).await
    }

    /// Delete a blob in the background (after the deleting transaction committed). A failure is logged; the sweep
    /// removes the blob later.
    pub(crate) fn delete_blob_later(self: &Arc<Self>, id: String) {
        let this = self.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    if let Err(e) = this.delete_blob(&id).await {
                        tracing::warn!("storage: {e:#} (the orphan sweep will retry)");
                    }
                });
            }
            Err(_) => tracing::warn!("storage: no runtime to delete the blob of {id}; the orphan sweep will remove it"),
        }
    }

    // -----------------------------------------------------------------------------------------------------------------
    // Documents

    /// Insert the `_storage` document of a blob that has been written, in its own transaction. With `token`, the
    /// token is marked used in the same transaction (failing with [`TokenProblem::Used`] if it already was).
    pub(crate) async fn insert_file(
        &self,
        app: &App,
        id: DocId,
        file: &StoredFile,
        token: Option<&UploadToken>,
    ) -> crate::Result<Result<Id<StoredFile>, TokenProblem>> {
        let table = app.db().catalog().table(StoredFile::NAME)?.clone();
        let Value::Object(fields) = serde_json::to_value(file)? else { anyhow::bail!("StoredFile is not an object") };
        let txn = app.db().begin_write().await?;
        let inserted = txn.with_state(|s| {
            if let Some(token) = token {
                let now = now_s() as i64;
                s.conn().execute(&format!("DELETE FROM {USED_TOKENS_TABLE} WHERE expires_at < ?1"), [now])?;
                let fresh = s.conn().execute(
                    &format!("INSERT OR IGNORE INTO {USED_TOKENS_TABLE} (nonce, expires_at) VALUES (?1, ?2)"),
                    rusqlite::params![token.n, token.e as i64],
                )?;
                if fresh == 0 {
                    return Ok(Err(TokenProblem::Used));
                }
            }
            s.insert_with_id(&table, id, fields, |doc| {
                crate::private::validate_doc::<StoredFile>(&Value::Object(doc.clone())).map_err(|e| {
                    anyhow::anyhow!("inserted document {id} does not match table `{}`: {e}", StoredFile::NAME)
                })
            })?;
            Ok(Ok(()))
        })?;
        match inserted {
            Ok(()) => {
                txn.commit_with(|_, _| Ok(())).context("committing the file's document")?;
                Ok(Ok(Id::from_doc_id(id)))
            }
            Err(problem) => {
                txn.rollback();
                Ok(Err(problem))
            }
        }
    }

    /// Write `bytes` as a new file (no upload URL involved): the blob, then its document.
    pub(crate) async fn store_bytes(
        &self,
        app: &App,
        bytes: Bytes,
        content_type: Option<String>,
        created_by: Option<String>,
        filename: Option<String>,
    ) -> crate::Result<Id<StoredFile>> {
        let (id, _guard) = self.new_id(app)?;
        let stream = futures::stream::iter([Ok::<_, std::convert::Infallible>(bytes)]);
        let (sha256, size) = match self.write_blob(id, stream, u64::MAX).await {
            Ok(v) => v,
            Err(WriteError::Store(e)) => return Err(e.context("storing a file")),
            Err(e) => anyhow::bail!("storing a file failed: {e:?}"),
        };
        let file = StoredFile { sha256, size, content_type, created_by, filename };
        match self.insert_file(app, id, &file, None).await {
            Ok(Ok(id)) => Ok(id),
            Ok(Err(problem)) => anyhow::bail!("{problem}"),
            Err(e) => {
                self.delete_blob(&id.encode()).await.ok();
                Err(e)
            }
        }
    }

    // -----------------------------------------------------------------------------------------------------------------
    // Sweep

    /// Delete blobs that no `_storage` document refers to: left behind by a crash between an upload and its
    /// document, or by a failed post-commit delete; and cached thumbnails (`_thumbs/<id>/…`) whose file is gone.
    /// Blobs and thumbnails younger than `config.orphan_grace`, blobs being uploaded and keys that aren't storage ids
    /// are left alone. Runs as the system cron `_storage_sweep`.
    pub async fn sweep(&self, app: &App) -> crate::Result<SweepStats> {
        let prefix = (!self.prefix.is_empty()).then(|| ObjectPath::from(self.prefix.as_str()));
        let table_number = app.db().catalog().table(StoredFile::NAME)?.number;
        // The app's clock (the wall clock, moved forward by `TestApp::advance_time` in tests).
        let cutoff_ms = app.db().clock().now_ms() as i64 - self.config.orphan_grace.as_millis() as i64;
        let mut stats = SweepStats::default();
        let mut listing = self.store.list(prefix.as_ref());
        // (file id, key, is a thumbnail)
        let mut candidates: Vec<(DocId, ObjectPath, bool)> = Vec::new();
        let thumbs_root = format!("{}/", super::thumbs::THUMBS_DIR);
        while let Some(meta) = listing.next().await {
            let meta = meta.context("listing stored blobs")?;
            stats.scanned += 1;
            // Direct children of the prefix named like a storage id, and `_thumbs/<storage id>/<variant>`.
            let location = meta.location.as_ref();
            let relative =
                if self.prefix.is_empty() { Some(location) } else { location.strip_prefix(&format!("{}/", self.prefix)) };
            let Some(relative) = relative else { continue };
            let (name, thumbnail) = match relative.strip_prefix(&thumbs_root) {
                Some(rest) => match rest.split_once('/') {
                    Some((id, variant)) if !variant.contains('/') => (id, true),
                    _ => continue,
                },
                None if !relative.contains('/') => (relative, false),
                None => continue,
            };
            let Ok(id) = DocId::decode(name) else { continue };
            if id.table_number() != table_number || meta.last_modified.timestamp_millis() > cutoff_ms {
                continue;
            }
            candidates.push((id, meta.location, thumbnail));
        }
        for chunk in candidates.chunks(500) {
            let snapshot = app.db().snapshot().await?;
            let orphans: Vec<&(DocId, ObjectPath, bool)> = snapshot.with_conn(|c| {
                let mut out = Vec::new();
                for entry in chunk {
                    if crate::db::storage::get_document(c, &entry.0.0)?.is_none() {
                        out.push(entry);
                    }
                }
                Ok(out)
            })?;
            drop(snapshot);
            for (id, path, thumbnail) in orphans {
                if self.blobs_in_flight.lock().unwrap_or_else(|e| e.into_inner()).contains(id) {
                    continue;
                }
                match self.store.delete(path).await {
                    Ok(()) | Err(object_store::Error::NotFound { .. }) if *thumbnail => stats.thumbnails += 1,
                    Ok(()) | Err(object_store::Error::NotFound { .. }) => stats.deleted += 1,
                    Err(e) => tracing::warn!("storage sweep: deleting {path} failed: {e}"),
                }
            }
        }
        if stats.deleted > 0 || stats.thumbnails > 0 {
            tracing::info!(
                "storage sweep: deleted {} orphaned blob(s) and {} thumbnail(s) of {} scanned",
                stats.deleted,
                stats.thumbnails,
                stats.scanned
            );
        }
        Ok(stats)
    }
}

fn token_used(conn: &Connection, nonce: &str) -> crate::Result<bool> {
    let found: Option<i64> = conn
        .query_row(&format!("SELECT 1 FROM {USED_TOKENS_TABLE} WHERE nonce = ?1"), [nonce], |r| r.get(0))
        .optional()?;
    Ok(found.is_some())
}

/// Create the raw table of used upload tokens (idempotent). Runs once when the app opens.
pub(crate) async fn ensure_schema(app: &App) -> crate::Result<()> {
    let txn = app.db().begin_write().await?;
    txn.with_state(|s| {
        s.conn().execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS {USED_TOKENS_TABLE} (nonce TEXT PRIMARY KEY, expires_at INTEGER NOT NULL) \
             WITHOUT ROWID"
        ))?;
        Ok(())
    })?;
    txn.commit_with(|_, _| Ok(()))?;
    Ok(())
}

/// The upload rate limit check for `ip`: `Err(retry_after)` when exhausted.
pub(crate) fn check_upload_rate(service: &StorageService, ip: &str) -> Result<(), Duration> {
    match upload_rule(&service.config) {
        Some(rule) => service.limiter.check("storage_upload", ip, rule),
        None => Ok(()),
    }
}

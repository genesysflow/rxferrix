//! File storage (PLAN.md §7.2): blobs in a local directory or an S3-compatible bucket (via `object_store`), one
//! `_storage` document per file, upload URLs, serving, and `ctx.storage`.
//!
//! **Upload flow**:
//! 1. A mutation (or action) returns `ctx.storage.generate_upload_url().await?`: a signed URL, valid for an hour, that
//!    works once.
//! 2. The client `POST`s the raw file to it with a `Content-Type` (`uploadFile()` in `@ferrix/client`). The server
//!    streams it into the store while hashing it, refuses more than `max_upload_size` (413), inserts the `_storage`
//!    document and answers `{ "storageId": "…" }`.
//! 3. The app saves the `Id<StoredFile>` in its own documents with another mutation.
//! 4. `ctx.storage.get_url(id)` gives the serving URL, `GET /api/storage/<id>` (range requests, ETag, safe headers).
//!
//! **Layout.** A file's blob is named by its storage id (`<prefix>/<id>`). The id is chosen before the upload starts;
//! the blob only becomes visible when complete (one `put`, or a multipart upload that is completed at the end), and
//! the document is inserted after that, so a reader never sees a partial file. Deleting a file deletes its document
//! (in a mutation: transactionally) and then its blob after the commit. Blobs without a document (a crash in
//! between, a failed delete) are removed by a periodic sweep ([`StorageService::sweep`]).
//!
//! - [`config`]: [`StorageConfig`] (backend, size limit, URL lifetime, rate limit, sweep).
//! - [`service`]: [`StorageService`] (`app.storage()`), blobs, upload tokens and the sweep.
//! - [`ctx`]: `ctx.storage` for queries, mutations and actions.
//! - [`http`]: the upload and serving endpoints.
//!
//! The functions below are the dashboard's admin-only Files page backend (`_system/storage:*`).

pub mod config;
pub mod ctx;
pub mod http;
pub mod service;
pub mod thumbs;

pub use axum::body::Bytes;
pub use config::{StorageBackend, StorageConfig};
pub use ctx::{ActionStorage, MutationStorage, QueryStorage};
pub use object_store;
pub use service::{StorageService, SweepStats};

use serde::{Deserialize, Serialize};

use crate::prelude::*;

/// A stored file (`_storage`): one per uploaded or stored blob. The creation time is the document's
/// `_creationTime`; `createdBy` is who generated the upload URL (or ran the storing action), as `"user:<id>"` or
/// `"admin:<id>"`. Reference files from app tables as `Id<StoredFile>` (`Id<"_storage">` in TS). (No field doc
/// comments: ts-rs would inline them into the generated `SystemDataModel`.)
#[table(name = "_storage")]
#[index(by_sha256 = [sha256])]
pub struct StoredFile {
    pub sha256: String,
    #[ts(type = "number")]
    pub size: u64,
    pub content_type: Option<String>,
    pub created_by: Option<String>,
    pub filename: Option<String>,
}

/// The id of a stored file.
pub type StorageId = Id<StoredFile>;

// ---------------------------------------------------------------------------------------------------------------------
// Dashboard (admin-only) system functions: `_system/storage:*`
// ---------------------------------------------------------------------------------------------------------------------

/// A file as the dashboard's Files page lists it.
#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct FileSummary {
    #[serde(rename = "_id")]
    #[ts(rename = "_id")]
    pub id: String,
    #[serde(rename = "_creationTime")]
    #[ts(rename = "_creationTime")]
    pub creation_time: f64,
    pub sha256: String,
    #[ts(type = "number")]
    pub size: u64,
    pub content_type: Option<String>,
    pub created_by: Option<String>,
    pub filename: Option<String>,
    /// The serving URL (`get_url`).
    pub url: String,
}

fn summary(ctx: &QueryCtx, doc: Doc<StoredFile>) -> FileSummary {
    let id = doc.id.as_str();
    FileSummary {
        url: ctx.storage.url_unchecked(&id),
        id,
        creation_time: doc.creation_time,
        sha256: doc.value.sha256,
        size: doc.value.size,
        content_type: doc.value.content_type,
        created_by: doc.value.created_by,
        filename: doc.value.filename,
    }
}

fn parse_file_id(id: &str) -> Result<Id<StoredFile>> {
    id.parse().map_err(|_| UserError::new("invalid_id", format!("`{id}` is not a file id")).into())
}

/// Every stored file, newest first (live and paginated).
#[query]
pub async fn files(ctx: &QueryCtx, pagination_opts: PaginationOpts) -> Result<PaginationResult<FileSummary>> {
    ctx.auth.require_admin()?;
    let page = ctx.db.query::<StoredFile>().order(Order::Desc).paginate(pagination_opts).await?;
    Ok(page.map(|doc| summary(ctx, doc)))
}

/// One file, or null.
#[query]
pub async fn file(ctx: &QueryCtx, id: String) -> Result<Option<FileSummary>> {
    ctx.auth.require_admin()?;
    let id = parse_file_id(&id)?;
    Ok(ctx.db.get(id).await?.map(|doc| summary(ctx, doc)))
}

/// Delete a file (its document now, its blob after the commit).
#[mutation]
pub async fn delete_file(ctx: &MutationCtx, id: String) -> Result<()> {
    ctx.auth.require_admin()?;
    ctx.storage.delete(parse_file_id(&id)?).await
}

/// An upload URL for the dashboard's upload button.
#[mutation]
pub async fn generate_upload_url(ctx: &MutationCtx) -> Result<String> {
    ctx.auth.require_admin()?;
    ctx.storage.generate_upload_url().await
}

/// The orphan sweep ([`StorageService::sweep`]), run by the system cron `_storage_sweep` every
/// `StorageConfig::sweep_interval`.
#[internal_action]
pub async fn sweep_orphans(ctx: &ActionCtx) -> Result<SweepStats> {
    let app = ctx.app.clone();
    app.storage().clone().sweep(&app).await
}

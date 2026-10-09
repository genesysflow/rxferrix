//! `ctx.storage` in queries ([`QueryStorage`]), mutations ([`MutationStorage`]) and actions ([`ActionStorage`]).

use std::sync::Arc;

use axum::body::Bytes;

use super::StoredFile;
use super::service::StorageService;
use crate::db::query::{MutationDb, QueryDb};
use crate::id::Id;
use crate::runtime::{App, Identity};
use crate::schema::Doc;
use crate::{Result, UserError};

fn not_found(id: Id<StoredFile>) -> crate::Error {
    UserError::new("not_found", format!("File {id} does not exist")).into()
}

/// `ctx.storage` in a query: deterministic reads only. Both reads are tracked like `ctx.db` reads, so a query using
/// them reruns when the file is deleted.
pub struct QueryStorage {
    db: QueryDb,
    service: Arc<StorageService>,
}

impl QueryStorage {
    pub(crate) fn new(db: QueryDb, service: Arc<StorageService>) -> Self {
        QueryStorage { db, service }
    }

    /// The URL serving the file (`<public url>/api/storage/<id>`), or `None` if it doesn't exist. Stable for a given
    /// id: the id itself is the unguessable capability.
    pub async fn get_url(&self, id: Id<StoredFile>) -> Result<Option<String>> {
        get_url(&self.db, &self.service, id).await
    }

    /// The file's `_storage` document (sha256, size, content type, ...), or `None`.
    pub async fn get_metadata(&self, id: Id<StoredFile>) -> Result<Option<Doc<StoredFile>>> {
        self.db.get(id).await
    }

    /// The serving URL of `id` without checking that the file exists.
    pub(crate) fn url_unchecked(&self, id: &str) -> String {
        self.service.file_url(id)
    }
}

async fn get_url(db: &QueryDb, service: &StorageService, id: Id<StoredFile>) -> Result<Option<String>> {
    Ok(db.get(id).await?.map(|_| service.file_url(&id.as_str())))
}

/// `ctx.storage` in a mutation. `delete` is transactional: the document goes away with the mutation's transaction,
/// and the blob is deleted after it commits (never if it rolls back).
pub struct MutationStorage {
    db: MutationDb,
    service: Arc<StorageService>,
    identity: Identity,
}

impl MutationStorage {
    pub(crate) fn new(db: MutationDb, service: Arc<StorageService>, identity: Identity) -> Self {
        MutationStorage { db, service, identity }
    }

    /// A short-lived (`upload_url_ttl`, default 1 hour), single-use, signed URL to `POST` a file to. The response is
    /// `{ "storageId": "<Id<"_storage">>" }`; the file's `createdBy` is the caller of this mutation.
    pub async fn generate_upload_url(&self) -> Result<String> {
        Ok(self.service.upload_url(self.identity.log_summary()))
    }

    /// See [`QueryStorage::get_url`].
    pub async fn get_url(&self, id: Id<StoredFile>) -> Result<Option<String>> {
        get_url(&self.db, &self.service, id).await
    }

    /// See [`QueryStorage::get_metadata`].
    pub async fn get_metadata(&self, id: Id<StoredFile>) -> Result<Option<Doc<StoredFile>>> {
        self.db.get(id).await
    }

    /// Delete the file: its document now (in this transaction), its blob once the transaction has committed. Fails
    /// with the user error `not_found` if it doesn't exist.
    pub async fn delete(&self, id: Id<StoredFile>) -> Result<()> {
        if self.db.get(id).await?.is_none() {
            return Err(not_found(id));
        }
        self.db.delete(id).await?;
        let service = self.service.clone();
        self.db.txn().on_commit(move || service.delete_blob_later(id.as_str()));
        Ok(())
    }
}

/// `ctx.storage` in an action: everything a mutation can do (each write in its own transaction), plus storing and
/// reading file contents.
pub struct ActionStorage {
    app: App,
    identity: Identity,
}

impl ActionStorage {
    pub(crate) fn new(app: App, identity: Identity) -> Self {
        ActionStorage { app, identity }
    }

    fn service(&self) -> &Arc<StorageService> {
        self.app.storage()
    }

    /// See [`MutationStorage::generate_upload_url`].
    pub async fn generate_upload_url(&self) -> Result<String> {
        Ok(self.service().upload_url(self.identity.log_summary()))
    }

    async fn metadata(&self, id: Id<StoredFile>) -> Result<Option<Doc<StoredFile>>> {
        let snapshot = self.app.db().snapshot().await?;
        let db = QueryDb::new(crate::db::query::Source::Snapshot(snapshot), self.app.db().catalog().clone());
        db.get(id).await
    }

    /// See [`QueryStorage::get_url`] (read at the latest snapshot).
    pub async fn get_url(&self, id: Id<StoredFile>) -> Result<Option<String>> {
        Ok(self.metadata(id).await?.map(|_| self.service().file_url(&id.as_str())))
    }

    /// See [`QueryStorage::get_metadata`] (read at the latest snapshot).
    pub async fn get_metadata(&self, id: Id<StoredFile>) -> Result<Option<Doc<StoredFile>>> {
        self.metadata(id).await
    }

    /// Store `bytes` as a new file and return its id. `createdBy` is the action's caller.
    pub async fn store(&self, bytes: impl Into<Bytes>, content_type: Option<&str>) -> Result<Id<StoredFile>> {
        let content_type = match content_type {
            Some(ct) => Some(super::http::validate_content_type(ct).map_err(|m| UserError::new("invalid_content_type", m))?),
            None => None,
        };
        self.service().store_bytes(&self.app, bytes.into(), content_type, self.identity.log_summary(), None).await
    }

    /// The contents of a file, or `None` if it doesn't exist.
    pub async fn get(&self, id: Id<StoredFile>) -> Result<Option<Bytes>> {
        if self.metadata(id).await?.is_none() {
            return Ok(None);
        }
        match self.service().read_blob(&id.as_str(), None).await {
            Ok(result) => Ok(Some(result.bytes().await?)),
            // Deleted between the read of its document and of its blob.
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(anyhow::Error::from(e).context(format!("reading file {id}"))),
        }
    }

    /// Delete the file (its document in its own transaction, then its blob). Fails with `not_found` if it doesn't
    /// exist.
    pub async fn delete(&self, id: Id<StoredFile>) -> Result<()> {
        let txn = self.app.db().begin_write().await?;
        let db = MutationDb::new(txn.clone());
        if db.get(id).await?.is_none() {
            txn.rollback();
            return Err(not_found(id));
        }
        db.delete(id).await?;
        txn.commit_with(|_, _| Ok(()))?;
        self.service().delete_blob(&id.as_str()).await
    }
}

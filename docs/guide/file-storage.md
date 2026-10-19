# File storage

Files live in `<data dir>/storage/` by default, or in an S3-compatible bucket (`--storage s3://bucket/prefix`, with
the standard `AWS_*` variables for credentials, region and endpoint, e.g. R2 or MinIO). Uploads take three steps:

```rust
use std::time::Duration;
use rxferrix::storage::{DownloadOptions, StoredFile};

#[mutation]
pub async fn generate_upload_url(ctx: &MutationCtx) -> Result<String> {
    ctx.storage.generate_upload_url().await        // signed, single-use, valid for an hour
}

#[mutation]
pub async fn set_avatar(ctx: &MutationCtx, file: Id<StoredFile>) -> Result<()> { /* store the id in a document */ }

#[query]
pub async fn avatar_url(ctx: &QueryCtx, file: Id<StoredFile>) -> Result<Option<String>> {
    ctx.storage.get_url(file).await                // ".../api/storage/<id>", live: null once the file is deleted
}
```

```ts
const url = await client.mutation(api.files.generate_upload_url, {});
const storageId = await uploadFile(url, file, { onProgress: ({ loaded, total }) => (progress = loaded / total) });
await client.mutation(api.files.set_avatar, { file: storageId });
```

`GET /api/storage/<id>` serves files with ETags, range requests (several ranges as `multipart/byteranges`) and
headers that keep uploads from running script on your origin. In `--dev`, file and upload URLs are root-relative
(`/api/storage/...`) unless a public URL is set explicitly (`--public-url`), so they go through a dev server's
`/api` proxy (Vite) like every other request. Add `?thumb=200x200` (and `&fit=cover`) to an image's
URL for a thumbnail (JPEG, PNG, WebP, GIF; generated once and cached; oversized images are refused from their header,
before decoding; new sizes are rate limited per client, cached ones aren't). Mutations can also `delete` files (transactionally) and read their metadata; actions can `store`
and `get` bytes directly. Uploads are capped at 100 MB (`--max-upload-size`). File URLs are unguessable capability
URLs: anyone with the URL can read the file. For private files, use expiring signed URLs and turn on private mode
(`StorageConfig::private(true)` or `--storage-private`), which refuses unsigned URLs:

```rust
#[query]   // in a query the expiry is rounded up to a multiple of the TTL, so the result is stable for that long
pub async fn avatar_url(ctx: &QueryCtx, file: Id<StoredFile>) -> Result<Option<String>> {
    ctx.storage.get_signed_url(file, Duration::from_secs(3600)).await   // ".../api/storage/<id>?exp=…&sig=…"
}
```

With S3, `get_download_url` hands out a **presigned URL to the bucket**: the file downloads straight from S3 (not
through the server), the bucket stays private, and S3 refuses the URL once it expires:

```rust
#[mutation]
pub async fn download_report(ctx: &MutationCtx, file: Id<StoredFile>) -> Result<Option<String>> {
    ctx.auth.require_user()?;   // decide who may see it
    ctx.storage.get_download_url_with(file, Duration::from_secs(15 * 60), DownloadOptions::default().attachment()).await
}
```

S3 serves it with the file's content type and name (unsafe types as attachments). The URL lasts at most 7 days, and
`?thumb=` doesn't work on it (use `get_signed_url` for thumbnails). With local storage the same call returns a signed
URL of the server, so the code works in development too.

Blobs left without a document (a crash mid-upload) and stale thumbnails are removed by the `_storage_sweep` system
cron (every 6 hours). Details: [docs/CONTRACT.md](../CONTRACT.md) §1.6 and [docs/PROTOCOL.md](../PROTOCOL.md)
("File storage"). The todo example attaches images to todos.

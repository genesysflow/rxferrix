# File storage

Store uploaded files separately from documents, then save an `Id<StoredFile>` in your app data. Local files live in `<data dir>/storage/`; an S3-compatible backend is optional.

An upload has three app-level steps: authorize an upload URL, upload the bytes, and attach the returned file ID to a document.

## Authorize and upload

Define an upload function in a backend module such as `src/files.rs`:

```rust
use rxferrix::prelude::*;

#[mutation]
pub async fn generate_upload_url(ctx: &MutationCtx) -> Result<String> {
    ctx.auth.require_user()?;
    ctx.storage.generate_upload_url().await
}
```

Then use the existing client and a browser `File`:

```ts
import { uploadFile } from "@rxferrix/client";
import { api } from "./rxferrix";

const url = await client.mutation(api.files.generate_upload_url, {});
const storageId = await uploadFile(url, file, {
  onProgress: ({ loaded, total }) => {
    console.log(loaded / total);
  },
});
```

The default upload URL is signed, single-use, and valid for one hour. Uploads are limited to 100 MB by default (`--max-upload-size`). The backend records file metadata in `_storage`.

After uploading, pass `storageId` to your own mutation that attaches it to a note, avatar, or message. In that mutation, check access to the destination document and validate the file metadata, including any ownership, size, and content-type requirements. Uploading a file does not attach it automatically.

See the [todo example](https://github.com/genesysflow/rxferrix/blob/master/examples/todo/server/src/todos.rs) for `set_image` and deletion of a replaced image.

## Return a public file URL

```rust
use rxferrix::storage::StoredFile;

#[query]
pub async fn file_url(ctx: &QueryCtx, file: Id<StoredFile>) -> Result<Option<String>> {
    ctx.storage.get_url(file).await
}
```

The URL serves the file through `/api/storage/<id>` and becomes `None` when the metadata is deleted. In `--dev`, URLs are root-relative unless `--public-url` is set, so Vite's `/api` proxy handles them.

A public file URL is a capability: anyone who has it can read the file. Checking auth in the query controls who receives the URL, not who can use a leaked URL.

Serving supports ETags, byte ranges, and safe content headers. For an image thumbnail, append `?thumb=200x200`, optionally with `&fit=cover`. JPEG, PNG, WebP, and GIF inputs are supported; generated thumbnails are cached.

## Serve private files

Start the server with `--storage-private`, or configure `StorageConfig::private(true)`, to reject unsigned file URLs. In the authorized function, use:

```rust
use std::time::Duration;

// After checking the caller's access to the document containing this file:
let url = ctx.storage.get_signed_url(file, Duration::from_secs(3600)).await?;
```

`get_url` fails in private mode. A signed URL remains usable by anyone who has it until expiry.

In queries, expiry is rounded up to a TTL boundary, giving a URL valid for between one and two TTLs. **The subscription does not renew it when time passes.** Choose a TTL suitable for the view, or request fresh URLs through a mutation/action before they expire. Mutations and actions use an exact TTL from issuance.

## Use S3-compatible storage

```sh
./my-app serve --storage s3://my-bucket/uploads
```

Configure credentials, region, and any custom endpoint through the standard `AWS_*` settings for your backend. File metadata stays in the app database.

`get_download_url(file, ttl)` returns a presigned bucket URL with S3, so downloads bypass the app server. For local storage it returns a signed server URL. `get_download_url_with(..., DownloadOptions::default().attachment())` requests an attachment download.

Presigned S3 URLs last at most seven days and can stop working earlier when temporary credentials expire. Do not append thumbnail options to them; use a signed server URL for thumbnails. With deleted-file retention, an already issued S3 URL can remain usable until the underlying blob is removed.

## Delete files and manage retention

Mutations can read metadata with `get_metadata` and delete with `ctx.storage.delete(file)`. Metadata deletion is transactional; bytes are removed after commit or after the configured retention window. A rolled-back mutation leaves the file intact. Actions can store and read bytes directly.

The `_storage_sweep` system cron removes orphaned blobs and stale thumbnails, subject to grace and retention settings. It does not decide whether an uploaded file is still useful to your app: delete unwanted `_storage` documents through app logic.

Database backups do not include all file bytes by default. See [Backups](/operations/backups) for local files, S3, deleted-file retention, and restores. The [storage reference](/CONTRACT#_1-6-file-storage) lists the full API.

//! File storage over real HTTP: the upload flow (hash, size, content type), single-use and expiring upload URLs,
//! the size limit (413, streamed), serving (ETag/304, ranges, safe headers), transactional deletes, actions'
//! `store`/`get`, reactivity of `get_url`, the dashboard's admin-only functions, the orphan sweep, rate limits, and
//! an in-memory `object_store` backend under a prefix (the same code path as S3).

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use ferrix::FunctionRef;
use ferrix::auth::AuthConfig;
use ferrix::runtime::{AppOptions, Identity};
use ferrix::storage::object_store::memory::InMemory;
use ferrix::storage::object_store::path::Path as ObjectPath;
use ferrix::storage::object_store::{ObjectStore, ObjectStoreExt};
use ferrix::storage::{StorageConfig, StoredFile};
use ferrix::testing::TestApp;
use ferrix::{Id, UserError};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_tungstenite::tungstenite::Message;

mod app {
    use ferrix::prelude::*;
    use ferrix::storage::StoredFile;

    #[table(name = "attachments")]
    pub struct Attachment {
        pub name: String,
        pub file: Id<StoredFile>,
    }

    #[mutation]
    pub async fn upload_url(ctx: &MutationCtx) -> Result<String> {
        ctx.storage.generate_upload_url().await
    }

    #[mutation]
    pub async fn attach(ctx: &MutationCtx, name: String, file: Id<StoredFile>) -> Result<Id<Attachment>> {
        ctx.db.insert(Attachment { name, file }).await
    }

    #[derive(serde::Serialize, serde::Deserialize, ts_rs::TS)]
    #[serde(rename_all = "camelCase")]
    #[ts(crate = "::ferrix::__private::ts_rs")]
    pub struct AttachmentView {
        pub name: String,
        pub url: Option<String>,
        pub size: Option<u32>,
    }

    /// Every attachment with its file's URL and size (both `None` once the file is gone).
    #[query]
    pub async fn list(ctx: &QueryCtx) -> Result<Vec<AttachmentView>> {
        let mut out = Vec::new();
        for a in ctx.db.query::<Attachment>().collect().await? {
            let url = ctx.storage.get_url(a.file).await?;
            let size = ctx.storage.get_metadata(a.file).await?.map(|m| m.size as u32);
            out.push(AttachmentView { name: a.name.clone(), url, size });
        }
        Ok(out)
    }

    #[query]
    pub async fn url_of(ctx: &QueryCtx, file: Id<StoredFile>) -> Result<Option<String>> {
        ctx.storage.get_url(file).await
    }

    /// Delete a file, then fail when `fail` (rolling the delete back).
    #[mutation]
    pub async fn delete_file(ctx: &MutationCtx, file: Id<StoredFile>, fail: bool) -> Result<()> {
        ctx.storage.delete(file).await?;
        if fail {
            return Err(UserError::new("nope", "rolled back").into());
        }
        Ok(())
    }

    #[action]
    pub async fn store_text(ctx: &ActionCtx, text: String) -> Result<Id<StoredFile>> {
        ctx.storage.store(text.into_bytes(), Some("text/plain; charset=utf-8")).await
    }

    #[action]
    pub async fn read_text(ctx: &ActionCtx, file: Id<StoredFile>) -> Result<Option<String>> {
        Ok(ctx.storage.get(file).await?.map(|b| String::from_utf8_lossy(&b).into_owned()))
    }

    #[action]
    /// `[url, content type]` (codegen can't inline tuples).
    pub async fn action_meta(ctx: &ActionCtx, file: Id<StoredFile>) -> Result<Vec<Option<String>>> {
        let url = ctx.storage.get_url(file).await?;
        let ct = ctx.storage.get_metadata(file).await?.and_then(|m| m.value.content_type);
        Ok(vec![url, ct])
    }

    #[action]
    pub async fn action_delete(ctx: &ActionCtx, file: Id<StoredFile>) -> Result<()> {
        ctx.storage.delete(file).await
    }

    #[action]
    pub async fn action_upload_url(ctx: &ActionCtx) -> Result<String> {
        ctx.storage.generate_upload_url().await
    }
}

struct Server {
    t: TestApp,
    base: String,
    addr: SocketAddr,
    client: reqwest::Client,
}

async fn server_with(storage: StorageConfig) -> Server {
    let options = AppOptions { dev: true, storage, ..Default::default() };
    let t = TestApp::with_options(options).await.unwrap();
    let addr = t.serve().await.unwrap();
    Server { t, base: format!("http://{addr}"), addr, client: reqwest::Client::new() }
}

async fn server() -> Server {
    server_with(StorageConfig::default()).await
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

impl Server {
    /// A URL from the server (root-relative without a public URL) made absolute.
    fn abs(&self, url: &str) -> String {
        if url.starts_with('/') { format!("{}{url}", self.base) } else { url.to_string() }
    }

    async fn upload_url(&self) -> String {
        self.abs(&self.t.mutation::<app::upload_url>(()).await.unwrap())
    }

    async fn post(&self, url: &str, body: impl Into<reqwest::Body>, content_type: Option<&str>) -> (u16, Value) {
        let mut req = self.client.post(url).body(body);
        if let Some(ct) = content_type {
            req = req.header("content-type", ct);
        }
        let res = req.send().await.unwrap();
        (res.status().as_u16(), res.json().await.unwrap_or(Value::Null))
    }

    /// Upload through a fresh URL; returns the storage id.
    async fn upload(&self, bytes: &[u8], content_type: &str, filename: Option<&str>) -> Id<StoredFile> {
        let mut url = self.upload_url().await;
        if let Some(name) = filename {
            url = format!("{url}&filename={}", urlencode(name));
        }
        let (s, b) = self.post(&url, bytes.to_vec(), Some(content_type)).await;
        assert_eq!(s, 200, "{b}");
        b["storageId"].as_str().unwrap().parse().unwrap()
    }

    async fn get(&self, id: Id<StoredFile>, headers: &[(&str, &str)]) -> reqwest::Response {
        self.get_url(&format!("{}/api/storage/{id}", self.base), headers).await
    }

    async fn get_url(&self, url: &str, headers: &[(&str, &str)]) -> reqwest::Response {
        let mut req = self.client.get(url);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        req.send().await.unwrap()
    }

    async fn metadata(&self, id: Id<StoredFile>) -> ferrix::Doc<StoredFile> {
        blob_doc(&self.t, id).await.expect("document exists")
    }

    async fn blob_exists(&self, id: Id<StoredFile>) -> bool {
        let storage = self.t.app().storage();
        storage.object_store().head(&storage.blob_path(&id.to_string())).await.is_ok()
    }
}

/// The `_storage` document of `id` (through `ctx.storage.get_metadata` in an internal query).
async fn blob_doc(t: &TestApp, id: Id<StoredFile>) -> Option<ferrix::Doc<StoredFile>> {
    t.query::<meta_q::metadata>((id,)).await.unwrap()
}

mod meta_q {
    use ferrix::prelude::*;
    use ferrix::storage::StoredFile;

    #[internal_query]
    pub async fn metadata(ctx: &QueryCtx, file: Id<StoredFile>) -> Result<Option<Doc<StoredFile>>> {
        ctx.storage.get_metadata(file).await
    }
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

fn code_of(v: &Value) -> &str {
    v["errorData"]["code"].as_str().unwrap_or("")
}

async fn eventually(mut f: impl AsyncFnMut() -> bool) {
    for _ in 0..200 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not reached");
}

// ---------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn upload_flow_end_to_end() {
    let s = server().await;
    // The upload URL is created by a signed-in user: that user becomes `createdBy`.
    let user = Identity { user_id: Some("uploader".into()), ..Default::default() };
    let url = s.abs(&s.t.mutation_as::<app::upload_url>(&user, ()).await.unwrap());
    assert!(url.contains("/api/storage/upload?token="), "{url}");

    let bytes: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
    let (status, body) = s.post(&format!("{url}&filename=photo%20%C3%A9t%C3%A9.png"), bytes.clone(), Some("image/png")).await;
    assert_eq!(status, 200, "{body}");
    let id: Id<StoredFile> = body["storageId"].as_str().unwrap().parse().unwrap();

    let doc = s.metadata(id).await;
    assert_eq!(doc.sha256, sha256_hex(&bytes));
    assert_eq!(doc.size, bytes.len() as u64);
    assert_eq!(doc.content_type.as_deref(), Some("image/png"));
    assert_eq!(doc.filename.as_deref(), Some("photo été.png"));
    assert_eq!(doc.created_by.as_deref(), Some("user:uploader"));
    assert!(doc.creation_time > 0.0);

    // The app stores the id in its own document; the query resolves the URL.
    s.t.mutation::<app::attach>(("avatar".to_string(), id)).await.unwrap();
    let list = s.t.query::<app::list>(()).await.unwrap();
    assert_eq!(list.len(), 1);
    let file_url = list[0].url.clone().unwrap();
    assert_eq!(file_url, format!("/api/storage/{id}"));
    assert_eq!(list[0].size, Some(bytes.len() as u32));

    // Served back byte for byte with the right headers.
    let res = s.get_url(&s.abs(&file_url), &[]).await;
    assert_eq!(res.status(), 200);
    let h = res.headers().clone();
    assert_eq!(h["content-type"], "image/png");
    assert_eq!(h["content-length"], bytes.len().to_string().as_str());
    assert_eq!(h["etag"], format!("\"{}\"", sha256_hex(&bytes)).as_str());
    assert_eq!(h["x-content-type-options"], "nosniff");
    assert_eq!(h["accept-ranges"], "bytes");
    assert!(h["cache-control"].to_str().unwrap().starts_with("private, max-age="), "{h:?}");
    assert_eq!(h["content-disposition"], "inline; filename=\"photo _t_.png\"; filename*=UTF-8''photo%20%C3%A9t%C3%A9.png");
    assert_eq!(res.bytes().await.unwrap().as_ref(), bytes.as_slice());

    // No content type: stored as none, served as octet-stream (an attachment).
    let url = s.upload_url().await;
    let (status, body) = s.post(&url, b"raw".to_vec(), None).await;
    assert_eq!(status, 200, "{body}");
    let raw: Id<StoredFile> = body["storageId"].as_str().unwrap().parse().unwrap();
    assert_eq!(s.metadata(raw).await.content_type, None);
    let res = s.get(raw, &[]).await;
    assert_eq!(res.headers()["content-type"], "application/octet-stream");
    assert_eq!(res.headers()["content-disposition"], "attachment");

    // An empty file works too.
    let empty = s.upload(b"", "text/plain", None).await;
    let res = s.get(empty, &[]).await;
    assert_eq!((res.status().as_u16(), res.headers()["content-length"].to_str().unwrap()), (200, "0"));
    assert_eq!(s.metadata(empty).await.sha256, sha256_hex(b""));

    // A bad content type is refused.
    let url = s.upload_url().await;
    let (status, body) = s.post(&url, b"x".to_vec(), Some("not a type")).await;
    assert_eq!((status, code_of(&body)), (400, "invalid_content_type"), "{body}");

    // Unknown ids, ids of other tables and garbage are 404s.
    let attachment_table_id = s.t.mutation::<app::attach>(("x".to_string(), id)).await.unwrap();
    for path in [format!("/api/storage/{attachment_table_id}"), "/api/storage/nope".to_string()] {
        let res = s.get_url(&format!("{}{path}", s.base), &[]).await;
        assert_eq!(res.status(), 404, "{path}");
    }
    assert_eq!(s.metadata(id).await.size, bytes.len() as u64);
}

#[tokio::test]
async fn upload_urls_are_single_use_signed_and_expire() {
    let s = server_with(StorageConfig::default().upload_url_ttl(Duration::from_secs(3)).max_upload_size(1000)).await;
    let url = s.upload_url().await;

    // A failed upload (too large) doesn't use the URL up.
    let (status, body) = s.post(&url, vec![0u8; 2000], Some("text/plain")).await;
    assert_eq!((status, code_of(&body)), (413, "file_too_large"), "{body}");
    let (status, body) = s.post(&url, b"first".to_vec(), Some("text/plain")).await;
    assert_eq!(status, 200, "{body}");
    // The second use is refused.
    let (status, body) = s.post(&url, b"second".to_vec(), Some("text/plain")).await;
    assert_eq!((status, code_of(&body)), (403, "upload_url_used"), "{body}");

    // A tampered or missing token is refused.
    let fresh = s.upload_url().await;
    let tampered = fresh.replacen("token=e", "token=f", 1).replacen("token=", "token=x", 1);
    let (status, body) = s.post(&tampered, b"x".to_vec(), None).await;
    assert_eq!((status, code_of(&body)), (403, "invalid_upload_url"), "{body}");
    let (status, body) = s.post(&format!("{}/api/storage/upload", s.base), b"x".to_vec(), None).await;
    assert_eq!((status, code_of(&body)), (403, "invalid_upload_url"), "{body}");
    // Neither is a token signed with another server's secret.
    let other = server().await;
    let foreign = other.upload_url().await.replace(&other.base, &s.base);
    let (status, body) = s.post(&foreign, b"x".to_vec(), None).await;
    assert_eq!((status, code_of(&body)), (403, "invalid_upload_url"), "{body}");

    // Expired after the TTL.
    tokio::time::sleep(Duration::from_millis(3100)).await;
    let (status, body) = s.post(&fresh, b"late".to_vec(), None).await;
    assert_eq!((status, code_of(&body)), (403, "upload_url_expired"), "{body}");

    // Actions can create upload URLs too.
    let url = s.abs(&s.t.action::<app::action_upload_url>(()).await.unwrap());
    let (status, body) = s.post(&url, b"from an action".to_vec(), None).await;
    assert_eq!(status, 200, "{body}");
}

#[tokio::test]
async fn oversized_uploads_get_413_while_streaming() {
    const MAX: u64 = 1 << 20;
    let s = server_with(StorageConfig::default().max_upload_size(MAX)).await;

    // A declared Content-Length above the limit is refused before reading the body.
    let url = s.upload_url().await;
    let (status, body) = s.post(&url, vec![7u8; MAX as usize + 1], Some("application/octet-stream")).await;
    assert_eq!((status, code_of(&body)), (413, "file_too_large"), "{body}");

    // A chunked body (no Content-Length) of 64 MiB: the server stops reading once past the limit. Sent over a raw
    // socket so the response can be read while the body is still being written.
    let url = s.upload_url().await;
    let path = url.trim_start_matches(&s.base).to_string();
    let produced = Arc::new(AtomicUsize::new(0));
    let mut sock = tokio::net::TcpStream::connect(s.addr).await.unwrap();
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/octet-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
        s.addr
    );
    sock.write_all(head.as_bytes()).await.unwrap();
    let (mut rd, mut wr) = sock.into_split();
    let writer = {
        let produced = produced.clone();
        tokio::spawn(async move {
            let chunk = vec![1u8; 64 * 1024];
            for _ in 0..1024 {
                let mut frame = format!("{:x}\r\n", chunk.len()).into_bytes();
                frame.extend_from_slice(&chunk);
                frame.extend_from_slice(b"\r\n");
                if wr.write_all(&frame).await.is_err() {
                    return;
                }
                produced.fetch_add(chunk.len(), Ordering::Relaxed);
            }
            let _ = wr.write_all(b"0\r\n\r\n").await;
        })
    };
    let mut response = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match tokio::time::timeout(Duration::from_secs(10), rd.read(&mut buf)).await.expect("response in time") {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                response.extend_from_slice(&buf[..n]);
                if response.windows(4).any(|w| w == b"\r\n\r\n") && String::from_utf8_lossy(&response).contains("file_too_large") {
                    break;
                }
            }
        }
    }
    let text = String::from_utf8_lossy(&response);
    assert!(text.starts_with("HTTP/1.1 413"), "{text}");
    assert!(text.contains("file_too_large"), "{text}");
    writer.abort();
    // Far less than the whole body was consumed (socket buffers allow a few MiB beyond the limit).
    assert!(produced.load(Ordering::Relaxed) < 32 << 20, "the whole body was read: {}", produced.load(Ordering::Relaxed));

    // Nothing was stored: no documents and no blobs.
    let blobs: Vec<_> = s.t.app().storage().object_store().list(None).collect().await;
    assert!(blobs.is_empty(), "{blobs:?}");

    // A body of exactly the limit is fine, and multipart-sized bodies (beyond the single-put buffer) work.
    let big = server_with(StorageConfig::default().max_upload_size(20 << 20)).await;
    let bytes: Vec<u8> = (0..(20u32 << 20)).map(|i| (i.wrapping_mul(2654435761) >> 24) as u8).collect();
    let id = big.upload(&bytes, "application/octet-stream", None).await;
    assert_eq!(big.metadata(id).await.sha256, sha256_hex(&bytes));
    let served = big.get(id, &[]).await.bytes().await.unwrap();
    assert!(served.as_ref() == bytes.as_slice(), "multipart blob round trip");
}

#[tokio::test]
async fn serving_etags_ranges_and_safe_headers() {
    let s = server().await;
    let data = b"0123456789abcdefghij".to_vec();
    let id = s.upload(&data, "video/mp4", Some("clip.mp4")).await;
    let etag = format!("\"{}\"", sha256_hex(&data));

    // ETag / If-None-Match.
    let res = s.get(id, &[("if-none-match", &etag)]).await;
    assert_eq!(res.status(), 304);
    assert_eq!(res.headers()["etag"], etag.as_str());
    let res = s.get(id, &[("if-none-match", &format!("\"other\", W/{etag}"))]).await;
    assert_eq!(res.status(), 304);
    let res = s.get(id, &[("if-none-match", "\"other\"")]).await;
    assert_eq!(res.status(), 200);

    // Ranges.
    let res = s.get(id, &[("range", "bytes=2-5")]).await;
    assert_eq!(res.status(), 206);
    assert_eq!(res.headers()["content-range"], "bytes 2-5/20");
    assert_eq!(res.headers()["content-length"], "4");
    assert_eq!(res.bytes().await.unwrap().as_ref(), b"2345");
    let res = s.get(id, &[("range", "bytes=-3")]).await;
    assert_eq!((res.status().as_u16(), res.headers()["content-range"].to_str().unwrap()), (206, "bytes 17-19/20"));
    assert_eq!(res.bytes().await.unwrap().as_ref(), b"hij");
    let res = s.get(id, &[("range", "bytes=15-")]).await;
    assert_eq!(res.bytes().await.unwrap().as_ref(), b"fghij");
    let res = s.get(id, &[("range", "bytes=20-")]).await;
    assert_eq!(res.status(), 416);
    assert_eq!(res.headers()["content-range"], "bytes */20");
    // Multiple ranges aren't supported: the whole file.
    let res = s.get(id, &[("range", "bytes=0-1,4-5")]).await;
    assert_eq!(res.status(), 200);
    assert_eq!(res.bytes().await.unwrap().len(), 20);
    // If-Range with another ETag: the whole file; with ours: the range.
    let res = s.get(id, &[("range", "bytes=0-1"), ("if-range", "\"stale\"")]).await;
    assert_eq!(res.status(), 200);
    let res = s.get(id, &[("range", "bytes=0-1"), ("if-range", &etag)]).await;
    assert_eq!(res.status(), 206);

    // HEAD: headers only.
    let res = s.client.head(format!("{}/api/storage/{id}", s.base)).send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["content-length"], "20");
    assert_eq!(res.bytes().await.unwrap().len(), 0);

    // Media is inline, sandboxed; ?download makes it an attachment.
    let res = s.get(id, &[]).await;
    assert_eq!(res.headers()["content-disposition"], "inline; filename=\"clip.mp4\"; filename*=UTF-8''clip.mp4");
    assert!(res.headers()["content-security-policy"].to_str().unwrap().contains("sandbox"));
    let res = s.get_url(&format!("{}/api/storage/{id}?download=1", s.base), &[]).await;
    assert!(res.headers()["content-disposition"].to_str().unwrap().starts_with("attachment;"));

    // Active content (HTML, SVG, XML) is never rendered inline on the app's origin.
    for (ct, body) in [
        ("text/html", "<script>alert(document.cookie)</script>"),
        ("image/svg+xml", "<svg xmlns='http://www.w3.org/2000/svg'><script>alert(1)</script></svg>"),
        ("application/xhtml+xml", "<html/>"),
        ("text/javascript", "alert(1)"),
    ] {
        let id = s.upload(body.as_bytes(), ct, Some("evil.html")).await;
        let res = s.get(id, &[]).await;
        let h = res.headers();
        assert_eq!(h["content-type"], ct);
        assert!(h["content-disposition"].to_str().unwrap().starts_with("attachment;"), "{ct}");
        assert_eq!(h["content-security-policy"], "default-src 'none'; style-src 'unsafe-inline'; sandbox");
        assert_eq!(h["x-content-type-options"], "nosniff");
    }
    // Images and text are inline.
    for ct in ["image/png", "text/plain; charset=utf-8", "audio/ogg"] {
        let id = s.upload(b"x", ct, None).await;
        assert_eq!(s.get(id, &[]).await.headers()["content-disposition"], "inline", "{ct}");
    }
}

#[tokio::test]
async fn deletes_in_mutations_are_transactional() {
    let s = server().await;
    let id = s.upload(b"keep me", "text/plain", None).await;

    // A mutation that deletes the file and then fails rolls the delete back: the file is still served.
    let err = s.t.mutation::<app::delete_file>((id, true)).await.unwrap_err();
    assert_eq!(err.downcast_ref::<UserError>().unwrap().code, "nope");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(s.blob_exists(id).await);
    let res = s.get(id, &[]).await;
    assert_eq!(res.status(), 200);
    assert_eq!(res.bytes().await.unwrap().as_ref(), b"keep me");

    // A committed delete removes the document at once and the blob right after the commit.
    s.t.mutation::<app::delete_file>((id, false)).await.unwrap();
    assert_eq!(s.get(id, &[]).await.status(), 404);
    assert_eq!(s.t.query::<app::url_of>((id,)).await.unwrap(), None);
    eventually(async || !s.blob_exists(id).await).await;

    // Deleting a missing file is a user error.
    let err = s.t.mutation::<app::delete_file>((id, false)).await.unwrap_err();
    assert_eq!(err.downcast_ref::<UserError>().unwrap().code, "not_found");
}

#[tokio::test]
async fn actions_store_and_get() {
    let s = server().await;
    let caller = Identity { user_id: Some("robot".into()), ..Default::default() };
    let id = s.t.action_as::<app::store_text>(&caller, ("hello from an action".to_string(),)).await.unwrap();
    let doc = s.metadata(id).await;
    assert_eq!(doc.size, 20);
    assert_eq!(doc.sha256, sha256_hex(b"hello from an action"));
    assert_eq!(doc.content_type.as_deref(), Some("text/plain; charset=utf-8"));
    assert_eq!(doc.created_by.as_deref(), Some("user:robot"));
    assert_eq!(s.t.action::<app::read_text>((id,)).await.unwrap().as_deref(), Some("hello from an action"));
    let meta = s.t.action::<app::action_meta>((id,)).await.unwrap();
    assert_eq!(meta, vec![Some(format!("/api/storage/{id}")), Some("text/plain; charset=utf-8".to_string())]);
    // Served over HTTP like an upload.
    assert_eq!(s.get(id, &[]).await.text().await.unwrap(), "hello from an action");

    // An uploaded file is readable from an action.
    let up = s.upload(b"uploaded", "text/plain", None).await;
    assert_eq!(s.t.action::<app::read_text>((up,)).await.unwrap().as_deref(), Some("uploaded"));

    // Delete from an action: the document and the blob are gone; get returns None.
    s.t.action::<app::action_delete>((id,)).await.unwrap();
    assert!(!s.blob_exists(id).await);
    assert_eq!(s.t.action::<app::read_text>((id,)).await.unwrap(), None);
    assert_eq!(s.t.action::<app::action_meta>((id,)).await.unwrap(), vec![None, None]);
    let err = s.t.action::<app::action_delete>((id,)).await.unwrap_err();
    assert!(format!("{err:#}").contains("does not exist"), "{err:#}");
}

#[tokio::test]
async fn queries_with_get_url_are_reactive() {
    let s = server().await;
    let id = s.upload(b"reactive", "text/plain", None).await;
    s.t.mutation::<app::attach>(("doc".to_string(), id)).await.unwrap();

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/api/sync", s.addr)).await.unwrap();
    let send = |v: Value| Message::text(v.to_string());
    ws.send(send(json!({ "type": "Connect", "sessionId": "s", "connectionCount": 0, "lastCloseReason": null }))).await.unwrap();
    ws.send(send(json!({ "type": "ModifyQuerySet", "baseVersion": 0, "newVersion": 1, "modifications": [
        { "type": "Add", "queryId": 0, "udfPath": app::list::path(), "args": {} }
    ] })))
    .await
    .unwrap();
    async fn next_value(ws: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin)) -> Value {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(10), ws.next()).await.expect("timeout").unwrap().unwrap();
            let Message::Text(text) = msg else { continue };
            let v: Value = serde_json::from_str(text.as_str()).unwrap();
            if v["type"] != "Transition" {
                continue;
            }
            if let Some(m) = v["modifications"].as_array().unwrap().iter().find(|m| m["queryId"] == 0) {
                return m["value"].clone();
            }
        }
    }
    let first = next_value(&mut ws).await;
    assert_eq!(first[0]["url"], format!("/api/storage/{id}"));
    assert_eq!(first[0]["size"], 8);

    // Deleting the file (only `_storage` changes) reruns the query: the URL is gone.
    s.t.mutation::<app::delete_file>((id, false)).await.unwrap();
    let after = next_value(&mut ws).await;
    assert_eq!(after[0]["url"], Value::Null);
    assert_eq!(after[0]["size"], Value::Null);
}

#[tokio::test]
async fn dashboard_file_functions_are_admin_only() {
    let t = TestApp::with_auth(AuthConfig::password().rate_limits(None)).await.unwrap();
    let app = t.app().clone();
    let addr = t.serve().await.unwrap();
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();
    let post = async |path: &str, token: Option<&str>, body: Value| -> (u16, Value) {
        let mut req = client.post(format!("{base}{path}")).json(&body);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let res = req.send().await.unwrap();
        (res.status().as_u16(), res.json().await.unwrap_or(Value::Null))
    };
    app.auth().create_admin(&app, "admin@example.com", "admin password".into()).await.unwrap();
    let (_, admin) = post("/api/auth/admin/signin", None, json!({ "email": "admin@example.com", "password": "admin password" })).await;
    let (_, user) = post("/api/auth/signup", None, json!({ "email": "user@example.com", "password": "user password" })).await;
    let admin = admin["accessToken"].as_str().unwrap().to_string();
    let user = user["accessToken"].as_str().unwrap().to_string();
    let call = async |kind: &str, name: &str, args: Value, token: Option<&str>| {
        post(&format!("/api/{kind}"), token, json!({ "path": format!("_system/storage:{name}"), "args": args })).await
    };

    let some_id = "0000000000000000000000000000"[..26].to_string();
    let calls = [
        ("query", "files", json!({ "paginationOpts": { "numItems": 10, "cursor": null } })),
        ("query", "file", json!({ "id": some_id })),
        ("mutation", "delete_file", json!({ "id": some_id })),
        ("mutation", "generate_upload_url", json!({})),
    ];
    for (kind, name, args) in &calls {
        let (s, b) = call(kind, name, args.clone(), None).await;
        assert_eq!((s, code_of(&b)), (400, "unauthenticated"), "anonymous {name}: {b}");
        let (s, b) = call(kind, name, args.clone(), Some(&user)).await;
        assert_eq!((s, code_of(&b)), (400, "forbidden"), "user {name}: {b}");
    }

    // An admin uploads through their own upload URL, sees it listed, and deletes it.
    let (s, b) = call("mutation", "generate_upload_url", json!({}), Some(&admin)).await;
    assert_eq!(s, 200, "{b}");
    let url = b["value"].as_str().unwrap();
    let res = client.post(format!("{base}{url}&filename=notes.txt")).header("content-type", "text/plain").body("admin file").send().await.unwrap();
    assert_eq!(res.status(), 200);
    let id = res.json::<Value>().await.unwrap()["storageId"].as_str().unwrap().to_string();
    let (s, b) = call("query", "files", json!({ "paginationOpts": { "numItems": 10, "cursor": null } }), Some(&admin)).await;
    assert_eq!(s, 200, "{b}");
    let f = &b["value"]["page"][0];
    assert_eq!(f["_id"], id.as_str());
    assert_eq!(f["filename"], "notes.txt");
    assert_eq!(f["contentType"], "text/plain");
    assert_eq!(f["size"], 10);
    assert_eq!(f["url"], format!("/api/storage/{id}"));
    assert!(f["createdBy"].as_str().unwrap().starts_with("admin:"), "{f}");
    let (_, b) = call("query", "file", json!({ "id": id }), Some(&admin)).await;
    assert_eq!(b["value"]["_id"], id.as_str());
    let (_, b) = call("query", "file", json!({ "id": "nope" }), Some(&admin)).await;
    assert_eq!(code_of(&b), "invalid_id");

    // `_storage` is read-only through the generic dashboard writes.
    let (_, b) = post("/api/mutation", Some(&admin), json!({ "path": "_system/dashboard:delete_documents", "args": { "ids": [id] } })).await;
    assert_eq!(code_of(&b), "read_only_table", "{b}");

    let (s, b) = call("mutation", "delete_file", json!({ "id": id }), Some(&admin)).await;
    assert_eq!(s, 200, "{b}");
    let (_, b) = call("query", "files", json!({ "paginationOpts": { "numItems": 10, "cursor": null } }), Some(&admin)).await;
    assert_eq!(b["value"]["page"], json!([]));
    let (_, b) = call("mutation", "delete_file", json!({ "id": id }), Some(&admin)).await;
    assert_eq!(code_of(&b), "not_found");
    assert_eq!(client.get(format!("{base}/api/storage/{id}")).send().await.unwrap().status(), 404);
}

#[tokio::test]
async fn object_store_backend_under_a_prefix() {
    // An in-memory object store stands in for S3: the same code path (object_store + a key prefix), no network.
    let memory: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let s = server_with(StorageConfig::object_store(memory.clone(), "/apps/demo/").orphan_grace(Duration::ZERO)).await;
    let data = b"stored in memory".to_vec();
    let id = s.upload(&data, "text/plain", None).await;

    let key = ObjectPath::from(format!("apps/demo/{id}"));
    assert_eq!(memory.get(&key).await.unwrap().bytes().await.unwrap().as_ref(), data.as_slice());
    let res = s.get(id, &[("range", "bytes=10-")]).await;
    assert_eq!(res.status(), 206);
    assert_eq!(res.bytes().await.unwrap().as_ref(), b"memory");

    // Actions read and write it too; the 8 MiB+ multipart path works against it.
    let stored = s.t.action::<app::store_text>(("via action".to_string(),)).await.unwrap();
    assert!(memory.head(&ObjectPath::from(format!("apps/demo/{stored}"))).await.is_ok());
    let big: Vec<u8> = (0..(9u32 << 20)).map(|i| (i % 253) as u8).collect();
    let big_id = s.upload(&big, "application/octet-stream", None).await;
    assert_eq!(s.metadata(big_id).await.sha256, sha256_hex(&big));

    // The sweep deletes orphaned blobs (a storage id without a document) and leaves everything else alone.
    let orphan = {
        s.t.mutation::<app::delete_file>((stored, false)).await.unwrap();
        stored
    };
    let orphan_key = ObjectPath::from(format!("apps/demo/{orphan}"));
    // Put the blob back as if the post-commit delete had failed.
    eventually(async || memory.head(&orphan_key).await.is_err()).await;
    memory.put(&orphan_key, b"orphan".to_vec().into()).await.unwrap();
    let foreign = ObjectPath::from("apps/demo/not-a-storage-id");
    let outside = ObjectPath::from(format!("elsewhere/{orphan}"));
    memory.put(&foreign, b"x".to_vec().into()).await.unwrap();
    memory.put(&outside, b"x".to_vec().into()).await.unwrap();
    let stats = s.t.app().storage().sweep(s.t.app()).await.unwrap();
    assert_eq!(stats.deleted, 1, "{stats:?}");
    assert!(memory.head(&orphan_key).await.is_err());
    assert!(memory.head(&foreign).await.is_ok() && memory.head(&outside).await.is_ok());
    assert!(memory.head(&key).await.is_ok(), "files with documents stay");

    // Deleting removes the blob from the store.
    s.t.mutation::<app::delete_file>((id, false)).await.unwrap();
    eventually(async || memory.head(&key).await.is_err()).await;
}

#[tokio::test]
async fn local_sweep_respects_the_grace_period() {
    let s = server().await;
    let kept = s.upload(b"kept", "text/plain", None).await;
    // Delete the document behind the blob's back, leaving an orphan (as a crash after the upload would).
    let orphan = s.upload(b"orphan", "text/plain", None).await;
    let storage = s.t.app().storage().clone();
    // Recreate the blob after a normal delete.
    s.t.mutation::<app::delete_file>((orphan, false)).await.unwrap();
    eventually(async || !s.blob_exists(orphan).await).await;
    storage.object_store().put(&storage.blob_path(&orphan.to_string()), b"orphan".to_vec().into()).await.unwrap();
    // Within the grace period (default one hour) nothing is swept.
    assert_eq!(storage.sweep(s.t.app()).await.unwrap().deleted, 0);
    assert!(s.blob_exists(orphan).await);
    // The files live in <data dir>/storage/<id>.
    assert!(s.t.dir().join("storage").join(kept.to_string()).is_file());
}

#[tokio::test]
async fn uploads_are_rate_limited() {
    let s = server_with(StorageConfig::default().upload_rate_limit(Some((2, 1)))).await;
    for _ in 0..2 {
        let url = s.upload_url().await;
        assert_eq!(s.post(&url, b"x".to_vec(), None).await.0, 200);
    }
    let url = s.upload_url().await;
    let res = s.client.post(&url).body("x").send().await.unwrap();
    assert_eq!(res.status(), 429);
    assert!(res.headers().contains_key("retry-after"));
    assert_eq!(res.json::<Value>().await.unwrap()["errorData"]["code"], "rate_limited");
}

#[tokio::test]
async fn public_url_makes_absolute_urls() {
    let t = TestApp::with_options(AppOptions {
        dev: true,
        storage: StorageConfig::default().public_url("https://files.example.com/"),
        ..Default::default()
    })
    .await
    .unwrap();
    let url = t.mutation::<app::upload_url>(()).await.unwrap();
    assert!(url.starts_with("https://files.example.com/api/storage/upload?token="), "{url}");
    let id = t.action::<app::store_text>(("x".to_string(),)).await.unwrap();
    assert_eq!(t.query::<app::url_of>((id,)).await.unwrap(), Some(format!("https://files.example.com/api/storage/{id}")));
}

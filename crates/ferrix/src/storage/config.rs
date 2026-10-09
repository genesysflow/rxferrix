//! [`StorageConfig`]: where file blobs live and the limits of the upload endpoint.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use object_store::ObjectStore;


/// Default [`StorageConfig::max_upload_size`]: 100 MiB.
pub const DEFAULT_MAX_UPLOAD_SIZE: u64 = 100 * 1024 * 1024;
/// Default [`StorageConfig::upload_url_ttl`]: one hour.
pub const DEFAULT_UPLOAD_URL_TTL: Duration = Duration::from_secs(3600);
/// Default [`StorageConfig::cache_max_age`]: 30 days (the content of a storage id never changes).
pub const DEFAULT_CACHE_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 3600);
/// Default [`StorageConfig::max_signed_url_ttl`]: 7 days.
pub const DEFAULT_MAX_SIGNED_URL_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
/// Default [`StorageConfig::thumbnail_max_dimension`].
pub const DEFAULT_THUMBNAIL_MAX_DIMENSION: u32 = 1024;
/// Default [`StorageConfig::thumbnail_max_source_pixels`]: 40 megapixels.
pub const DEFAULT_THUMBNAIL_MAX_SOURCE_PIXELS: u64 = 40_000_000;
/// Default [`StorageConfig::thumbnail_max_source_bytes`]: 32 MiB.
pub const DEFAULT_THUMBNAIL_MAX_SOURCE_BYTES: u64 = 32 * 1024 * 1024;

/// Where blobs are stored.
#[derive(Clone)]
pub enum StorageBackend {
    /// A directory on the local filesystem. `None` = `<data dir>/storage`.
    Local(Option<PathBuf>),
    /// An S3-compatible bucket (AWS S3, Cloudflare R2, MinIO, ...), configured from the standard `AWS_*` environment
    /// variables (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_REGION`, `AWS_ENDPOINT_URL`, `AWS_ALLOW_HTTP`,
    /// ...). Needs the `s3` cargo feature (default).
    S3 { bucket: String, prefix: String },
    /// Any `object_store` implementation (e.g. `object_store::memory::InMemory` in tests, or a store built by hand),
    /// with blobs under `prefix`.
    Custom { store: Arc<dyn ObjectStore>, prefix: String },
}

impl fmt::Debug for StorageBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StorageBackend::Local(dir) => f.debug_tuple("Local").field(dir).finish(),
            StorageBackend::S3 { bucket, prefix } => {
                f.debug_struct("S3").field("bucket", bucket).field("prefix", prefix).finish()
            }
            StorageBackend::Custom { store, prefix } => {
                f.debug_struct("Custom").field("store", &store.to_string()).field("prefix", prefix).finish()
            }
        }
    }
}

impl StorageBackend {
    /// Parse a `--storage` value: `s3://bucket[/prefix]`, `file:///abs/dir`, or a plain directory path.
    pub fn parse(s: &str) -> crate::Result<StorageBackend> {
        let s = s.trim();
        if s.is_empty() {
            bail!("empty storage location");
        }
        if let Some(rest) = s.strip_prefix("s3://") {
            let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
            if bucket.is_empty() {
                bail!("`{s}`: missing bucket name (expected s3://bucket/prefix)");
            }
            return Ok(StorageBackend::S3 { bucket: bucket.to_string(), prefix: prefix.trim_matches('/').to_string() });
        }
        if let Some(path) = s.strip_prefix("file://") {
            return Ok(StorageBackend::Local(Some(PathBuf::from(path))));
        }
        if s.contains("://") {
            bail!("`{s}`: unsupported storage URL (use s3://bucket/prefix, file:///dir or a directory path)");
        }
        Ok(StorageBackend::Local(Some(PathBuf::from(s))))
    }
}

/// File storage settings (`Ferrix::new().storage(..)`, `AppOptions::storage`, `--storage`, ...).
#[derive(Debug, Clone)]
pub struct StorageConfig {
    pub backend: StorageBackend,
    /// Uploads larger than this are refused with 413. Default 100 MiB (`--max-upload-size`).
    pub max_upload_size: u64,
    /// How long an upload URL stays valid. Default one hour.
    pub upload_url_ttl: Duration,
    /// `Cache-Control: private, max-age=<this>` on served files. Default 30 days.
    pub cache_max_age: Duration,
    /// Base URL for upload and file URLs (default: the auth public URL, `--public-url`; without one, URLs are
    /// root-relative, e.g. `/api/storage/<id>`).
    pub public_url: Option<String>,
    /// Uploads per client IP: burst and refill per minute. `None` disables the limit. Default 60 / 120 per minute.
    pub upload_rate_limit: Option<(u32, u32)>,
    /// How often blobs without a `_storage` document are looked for and deleted. Default every 6 hours (the first
    /// sweep runs 5 minutes after startup). `None` disables the background sweep (`StorageService::sweep` still works).
    pub sweep_interval: Option<Duration>,
    /// A blob is only swept once it is older than this (an upload's document is inserted right after its blob is
    /// complete). Default one hour.
    pub orphan_grace: Duration,
    /// Private mode (`--storage-private`): `GET /api/storage/{id}` without a valid signature is refused (403), so
    /// files are only reachable through `ctx.storage.get_signed_url(id, ttl)`, and `get_url` fails. Default off
    /// (file URLs are unguessable capability URLs).
    pub private: bool,
    /// The longest TTL `get_signed_url` accepts. Default 7 days.
    pub max_signed_url_ttl: Duration,
    /// Largest thumbnail width and height (`?thumb=WxH`), when [`StorageConfig::thumbnail_sizes`] is not set.
    /// Default 1024.
    pub thumbnail_max_dimension: u32,
    /// If set, the only thumbnail sizes served (`(width, height)` pairs); others get 400. Default: any size up to
    /// `thumbnail_max_dimension`. Every variant served is cached, so an allowlist also bounds the cache.
    pub thumbnail_sizes: Option<Vec<(u32, u32)>>,
    /// Images with more pixels are refused for thumbnails before being decoded (a decompression-bomb guard).
    /// Default 40 megapixels.
    pub thumbnail_max_source_pixels: u64,
    /// Larger files get no thumbnails (the source is read into memory). Default 32 MiB.
    pub thumbnail_max_source_bytes: u64,
}

impl Default for StorageConfig {
    fn default() -> Self {
        StorageConfig {
            backend: StorageBackend::Local(None),
            max_upload_size: DEFAULT_MAX_UPLOAD_SIZE,
            upload_url_ttl: DEFAULT_UPLOAD_URL_TTL,
            cache_max_age: DEFAULT_CACHE_MAX_AGE,
            public_url: None,
            upload_rate_limit: Some((60, 120)),
            sweep_interval: Some(Duration::from_secs(6 * 3600)),
            orphan_grace: Duration::from_secs(3600),
            private: false,
            max_signed_url_ttl: DEFAULT_MAX_SIGNED_URL_TTL,
            thumbnail_max_dimension: DEFAULT_THUMBNAIL_MAX_DIMENSION,
            thumbnail_sizes: None,
            thumbnail_max_source_pixels: DEFAULT_THUMBNAIL_MAX_SOURCE_PIXELS,
            thumbnail_max_source_bytes: DEFAULT_THUMBNAIL_MAX_SOURCE_BYTES,
        }
    }
}

impl StorageConfig {
    /// Blobs in a local directory.
    pub fn local(dir: impl Into<PathBuf>) -> Self {
        StorageConfig { backend: StorageBackend::Local(Some(dir.into())), ..Default::default() }
    }

    /// Blobs in an S3-compatible bucket, under `prefix` (may be empty). Credentials, region and endpoint come from
    /// the `AWS_*` environment variables.
    pub fn s3(bucket: impl Into<String>, prefix: impl Into<String>) -> Self {
        let prefix = prefix.into().trim_matches('/').to_string();
        StorageConfig { backend: StorageBackend::S3 { bucket: bucket.into(), prefix }, ..Default::default() }
    }

    /// Blobs in any `object_store` implementation, under `prefix` (may be empty).
    pub fn object_store(store: Arc<dyn ObjectStore>, prefix: impl Into<String>) -> Self {
        let prefix = prefix.into().trim_matches('/').to_string();
        StorageConfig { backend: StorageBackend::Custom { store, prefix }, ..Default::default() }
    }

    /// Parse a `--storage` location (see [`StorageBackend::parse`]).
    pub fn parse(location: &str) -> crate::Result<Self> {
        Ok(StorageConfig { backend: StorageBackend::parse(location)?, ..Default::default() })
    }

    pub fn max_upload_size(mut self, bytes: u64) -> Self {
        self.max_upload_size = bytes;
        self
    }

    pub fn upload_url_ttl(mut self, ttl: Duration) -> Self {
        self.upload_url_ttl = ttl;
        self
    }

    pub fn cache_max_age(mut self, max_age: Duration) -> Self {
        self.cache_max_age = max_age;
        self
    }

    pub fn public_url(mut self, url: impl Into<String>) -> Self {
        self.public_url = Some(url.into());
        self
    }

    /// `Some((burst, per_minute))` per client IP, or `None` for no limit.
    pub fn upload_rate_limit(mut self, limit: Option<(u32, u32)>) -> Self {
        self.upload_rate_limit = limit;
        self
    }

    pub fn sweep_interval(mut self, interval: Option<Duration>) -> Self {
        self.sweep_interval = interval;
        self
    }

    pub fn orphan_grace(mut self, grace: Duration) -> Self {
        self.orphan_grace = grace;
        self
    }

    /// Private mode: see [`StorageConfig::private`].
    pub fn private(mut self, private: bool) -> Self {
        self.private = private;
        self
    }

    pub fn max_signed_url_ttl(mut self, ttl: Duration) -> Self {
        self.max_signed_url_ttl = ttl;
        self
    }

    pub fn thumbnail_max_dimension(mut self, max: u32) -> Self {
        self.thumbnail_max_dimension = max;
        self
    }

    /// Serve only these thumbnail sizes (`(width, height)`).
    pub fn thumbnail_sizes(mut self, sizes: &[(u32, u32)]) -> Self {
        self.thumbnail_sizes = Some(sizes.to_vec());
        self
    }

    pub fn thumbnail_max_source_pixels(mut self, pixels: u64) -> Self {
        self.thumbnail_max_source_pixels = pixels;
        self
    }

    pub fn thumbnail_max_source_bytes(mut self, bytes: u64) -> Self {
        self.thumbnail_max_source_bytes = bytes;
        self
    }

    /// Build the object store and the key prefix blobs live under.
    pub(crate) fn build_store(&self, data_dir: &std::path::Path) -> crate::Result<(Arc<dyn ObjectStore>, String)> {
        match &self.backend {
            StorageBackend::Local(dir) => {
                let dir = dir.clone().unwrap_or_else(|| data_dir.join("storage"));
                std::fs::create_dir_all(&dir).with_context(|| format!("creating storage dir {}", dir.display()))?;
                let store = object_store::local::LocalFileSystem::new_with_prefix(&dir)
                    .with_context(|| format!("opening storage dir {}", dir.display()))?;
                Ok((Arc::new(store), String::new()))
            }
            StorageBackend::S3 { bucket, prefix } => Ok((build_s3(bucket)?, prefix.clone())),
            StorageBackend::Custom { store, prefix } => Ok((store.clone(), prefix.clone())),
        }
    }
}

#[cfg(feature = "s3")]
fn build_s3(bucket: &str) -> crate::Result<Arc<dyn ObjectStore>> {
    // `from_env` reads AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY, AWS_SESSION_TOKEN, AWS_REGION / AWS_DEFAULT_REGION,
    // AWS_ENDPOINT_URL (R2, MinIO), AWS_ALLOW_HTTP, AWS_VIRTUAL_HOSTED_STYLE_REQUEST, ...
    let mut builder = object_store::aws::AmazonS3Builder::from_env().with_bucket_name(bucket);
    if std::env::var_os("AWS_REGION").is_none()
        && let Ok(region) = std::env::var("AWS_DEFAULT_REGION")
    {
        builder = builder.with_region(region);
    }
    let store = builder.build().with_context(|| format!("configuring S3 bucket `{bucket}`"))?;
    Ok(Arc::new(store))
}

#[cfg(not(feature = "s3"))]
fn build_s3(bucket: &str) -> crate::Result<Arc<dyn ObjectStore>> {
    bail!("S3 storage (bucket `{bucket}`) needs ferrix's `s3` cargo feature")
}

/// The rate limit rule for uploads.
pub(crate) fn upload_rule(config: &StorageConfig) -> Option<crate::auth::rate_limit::Rule> {
    config.upload_rate_limit.map(|(burst, per_minute)| crate::auth::rate_limit::Rule { burst, per_minute })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_locations() {
        assert!(matches!(StorageBackend::parse("s3://bucket/a/b/").unwrap(),
            StorageBackend::S3 { ref bucket, ref prefix } if bucket == "bucket" && prefix == "a/b"));
        assert!(matches!(StorageBackend::parse("s3://bucket").unwrap(),
            StorageBackend::S3 { ref bucket, ref prefix } if bucket == "bucket" && prefix.is_empty()));
        assert!(matches!(StorageBackend::parse("file:///srv/files").unwrap(),
            StorageBackend::Local(Some(ref p)) if p == std::path::Path::new("/srv/files")));
        assert!(matches!(StorageBackend::parse("./files").unwrap(),
            StorageBackend::Local(Some(ref p)) if p == std::path::Path::new("./files")));
        assert!(StorageBackend::parse("s3://").is_err());
        assert!(StorageBackend::parse("gs://bucket").is_err());
        assert!(StorageBackend::parse("").is_err());
    }

    #[cfg(feature = "s3")]
    #[test]
    fn s3_store_builds_offline() {
        // Building the client makes no request; credentials are resolved lazily.
        let config = StorageConfig::s3("my-bucket", "/uploads/");
        let (store, prefix) = config.build_store(std::path::Path::new("/nonexistent")).unwrap();
        assert_eq!(prefix, "uploads");
        assert!(store.to_string().contains("my-bucket"), "{store}");
    }
}

//! A global cache to be shared between different symbolicator instances.
//!
//! The goal of this cache is to have a faster warm-up time when starting a new symbolicator
//! instance by reducing the cost of populating its cache via an additional caching layer that
//! lives closer to symbolicator. Expensive computations related to the computation of derived
//! caches may also be saved via this shared cache.

use std::collections::BTreeMap;
use std::convert::TryInto;
use std::fmt;
use std::io::SeekFrom;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context as _, Error, Result};
use futures::{Future, TryStreamExt};
use gcp_auth::Token;
use reqwest::{Body, Client, StatusCode};
use sentry::protocol::Context;
use sentry::{Hub, SentryFutureExt};
use tempfile::NamedTempFile;
use tokio::fs::{self, File};
use tokio::io::{self, AsyncSeekExt, AsyncWrite};
use tokio::sync::{mpsc, oneshot, RwLock};
use tokio_util::io::{ReaderStream, StreamReader};
use url::Url;

use crate::cache::{
    CacheName, FilesystemSharedCacheConfig, GcsSharedCacheConfig, SharedCacheBackendConfig,
    SharedCacheConfig,
};
use crate::services::download::MeasureSourceDownloadGuard;
use crate::utils::gcs::{self, GcsError};

use super::cacher::CacheKey;

// TODO: get timeouts from global config?
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const STORE_TIMEOUT: Duration = Duration::from_secs(60);

/// Errors using the cache backend.
///
/// This exists since some special cache errors should not be logged since they are
/// considered to be normal at scale, as long as their ratio stays low.
#[derive(thiserror::Error, Debug)]
enum CacheError {
    #[error("timeout connecting to cache service")]
    ConnectTimeout,
    #[error(transparent)]
    Other(#[from] Error),
}

struct GcsState {
    config: GcsSharedCacheConfig,
    client: Client,
    auth_manager: gcp_auth::AuthenticationManager,
}

impl fmt::Debug for GcsState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GcsState")
            .field("config", &self.config)
            .field("client", &self.client)
            .field("auth_manager", &"<AuthenticationManager>")
            .finish()
    }
}

pub fn measure_download_time<'a, F, T, E>(
    metric_prefix: &'a str,
    source_name: &'a str,
    f: F,
) -> impl Future<Output = F::Output> + 'a
where
    F: 'a + Future<Output = Result<T, E>>,
{
    let guard = MeasureSourceDownloadGuard::new(metric_prefix, source_name);
    async move {
        let output = f.await;
        guard.done(&output);
        output
    }
}

impl GcsState {
    pub async fn try_new(config: GcsSharedCacheConfig) -> Result<Self> {
        let auth_manager = match config.service_account_path {
            Some(ref path) => gcp_auth::from_credentials_file(&path).await?,
            None => {
                // For fresh k8s pods the GKE metadata server may not accept connections
                // yet, we we need to retry this for a bit.
                const MAX_DELAY: Duration = Duration::from_secs(60);
                const RETRY_INTERVAL: Duration = Duration::from_millis(500);
                let start = Instant::now();
                loop {
                    let future = async move {
                        gcp_auth::init()
                            .await
                            .context("Failed to initialise authentication token")
                    };
                    match tokio::time::timeout(Duration::from_secs(1), future)
                        .await
                        .unwrap_or_else(|_elapsed| {
                            Err(Error::msg("Timeout initialising GCS authentication token"))
                        }) {
                        Ok(auth_manager) => break auth_manager,
                        Err(err) if start.elapsed() > MAX_DELAY => return Err(err),
                        Err(err) => {
                            let remaining = MAX_DELAY - start.elapsed();
                            tracing::warn!("Error initialising GCS authentication token: {}", &err);
                            match err.downcast_ref::<gcp_auth::Error>() {
                                Some(gcp_auth::Error::NoAuthMethod(custom, gcloud, svc, user)) => {
                                    tracing::error!(
                                        "No GCP auth: custom: {}, gcloud: {}, svc: {}, user: {}",
                                        custom,
                                        gcloud,
                                        svc,
                                        user,
                                    );
                                }
                                _ => tracing::warn!(
                                    "Error initialising GCS authentication token: {}",
                                    &err
                                ),
                            }
                            tracing::info!(
                                "Waiting for GKE metadata server, {}s remaining",
                                remaining.as_secs(),
                            );
                            tokio::time::sleep(RETRY_INTERVAL).await;
                        }
                    }
                }
            }
        };
        Ok(Self {
            config,
            client: Client::new(),
            auth_manager,
        })
    }

    /// Returns a GCP authentication token, with timeout and error handling.
    ///
    /// Refreshing tokens involves talking to services over networks, this might fail.
    async fn get_token(&self) -> Result<Token> {
        let future = async {
            self.auth_manager
                .get_token(&["https://www.googleapis.com/auth/devstorage.read_write"])
                .await
                .context("Failed to get authentication token")
        };
        tokio::time::timeout(Duration::from_millis(300), future)
            .await
            .unwrap_or_else(|_| Err(Error::msg("Timeout refreshing GCS authentication token")))
    }

    /// Fetches item from shared cache if available and copies them to the writer.
    ///
    /// # Returns
    ///
    /// If successful the number of bytes written to the writer are returned.
    async fn fetch<W>(
        &self,
        key: &SharedCacheKey,
        writer: &mut W,
    ) -> Result<Option<u64>, CacheError>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        sentry::configure_scope(|scope| {
            let mut map = BTreeMap::new();
            map.insert("bucket".to_string(), self.config.bucket.clone().into());
            map.insert("key".to_string(), key.gcs_bucket_key().into());
            scope.set_context("GCS Shared Cache", Context::Other(map));
        });
        let token = self.get_token().await?;
        let url = gcs::download_url(&self.config.bucket, key.gcs_bucket_key().as_ref())
            .context("URL construction failed")?;
        let request = self.client.get(url).bearer_auth(token.as_str()).send();
        let request = tokio::time::timeout(CONNECT_TIMEOUT, request);
        let request = measure_download_time("services.shared_cache.fetch.connect", "gcs", request);

        match request.await {
            Ok(Ok(response)) => {
                let status = response.status();
                match status {
                    _ if status.is_success() => {
                        tracing::trace!(
                            "Success hitting shared_cache GCS {}",
                            key.gcs_bucket_key()
                        );
                        let stream = response
                            .bytes_stream()
                            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e));
                        let mut stream = StreamReader::new(stream);
                        let res = io::copy(&mut stream, writer)
                            .await
                            .context("IO Error streaming HTTP bytes to writer")
                            .map_err(CacheError::Other);
                        Some(res).transpose()
                    }
                    StatusCode::NOT_FOUND => Ok(None),
                    StatusCode::FORBIDDEN => Err(anyhow!(
                        "Insufficient permissions for bucket {}",
                        self.config.bucket
                    )
                    .into()),
                    StatusCode::UNAUTHORIZED => Err(anyhow!("Invalid credentials").into()),
                    _ => Err(anyhow!("Error response from GCS: {}", status).into()),
                }
            }
            Ok(Err(e)) => {
                tracing::trace!(
                    "Error in shared_cache GCS response for {}",
                    key.gcs_bucket_key()
                );
                Err(e).context("Bad GCS response for shared_cache")?
            }
            Err(_) => Err(CacheError::ConnectTimeout),
        }
    }

    async fn exists(&self, key: &SharedCacheKey) -> Result<bool, CacheError> {
        let token = self.get_token().await?;
        let url = gcs::object_url(&self.config.bucket, key.gcs_bucket_key().as_ref())
            .context("failed to build object url")?;
        let request = self.client.get(url).bearer_auth(token.as_str()).send();
        let request = tokio::time::timeout(CONNECT_TIMEOUT, request);

        let ret = match request.await {
            Ok(Ok(response)) => {
                // Consume the response body to be nice to the server, it is only a bit of JSON.
                let status = response.status();
                response.bytes().await.ok();

                match status {
                    StatusCode::OK => Ok(true),
                    StatusCode::NOT_FOUND => Ok(false),
                    status => Err(anyhow!("Unexpected status code from GCS: {}", status).into()),
                }
            }
            Ok(Err(err)) => Err(err).context("Error connecting to GCS")?,
            Err(_) => Err(CacheError::ConnectTimeout),
        };
        let status = match ret {
            Ok(_) => "ok",
            Err(CacheError::ConnectTimeout) => "connect-timeout",
            Err(_) => "error",
        };
        metric!(
            counter("services.shared_cache.exists") += 1,
            "cache" => key.name.as_ref(),
            "status" => status
        );
        ret
    }

    /// Stores a file on GCS.
    ///
    /// Because we use a very dumb API to upload files we always upload the data over the
    /// network even if the file already exists.  To reduce this, when `reason` is given as
    /// [`CacheStoreReason::Refresh`] this first fetches the metadata to check if the file
    /// exists.  This is racy, but reduces the number of times we spend sending data across
    /// for no reason.
    async fn store(
        &self,
        key: SharedCacheKey,
        mut src: File,
        reason: CacheStoreReason,
    ) -> Result<SharedCacheStoreResult, CacheError> {
        sentry::configure_scope(|scope| {
            let mut map = BTreeMap::new();
            map.insert("bucket".to_string(), self.config.bucket.clone().into());
            map.insert("key".to_string(), key.gcs_bucket_key().into());
            scope.set_context("GCS Shared Cache", Context::Other(map));
        });
        if reason == CacheStoreReason::Refresh {
            match self
                .exists(&key)
                .await
                .context("Failed fetching GCS object metadata from shared cache")
            {
                Ok(true) => return Ok(SharedCacheStoreResult::Skipped),
                Ok(false) => (),
                Err(err) => match err.downcast_ref::<CacheError>() {
                    Some(CacheError::ConnectTimeout) => (),
                    _ => {
                        sentry::capture_error(&*err);
                    }
                },
            }
        }

        let total_bytes = src
            .seek(SeekFrom::End(0))
            .await
            .context("failed to seek to end")?;
        src.rewind().await.context("failed to rewind")?;
        let token = self.get_token().await?;
        let mut url =
            Url::parse("https://storage.googleapis.com/upload/storage/v1/b?uploadType=media")
                .map_err(|_| GcsError::InvalidUrl)
                .context("failed to parse url")?;
        // Append path segments manually for proper encoding
        url.path_segments_mut()
            .map_err(|_| GcsError::InvalidUrl)
            .context("failed to build url")?
            .extend(&[&self.config.bucket, "o"]);
        url.query_pairs_mut()
            .append_pair("name", &key.gcs_bucket_key())
            // Upload only if it's not already there
            .append_pair("ifGenerationMatch", "0");

        let stream = ReaderStream::new(src);
        let body = Body::wrap_stream(stream);
        let request = self
            .client
            .post(url.clone())
            .bearer_auth(token.as_str())
            .body(body)
            .send();
        let request = tokio::time::timeout(STORE_TIMEOUT, request);
        let request = measure_download_time("services.shared_cache.store.upload", "gcs", request);

        match request.await {
            Ok(Ok(response)) => {
                let status = response.status();
                match status {
                    successful if successful.is_success() => {
                        tracing::trace!(
                            "Success hitting shared_cache GCS {}",
                            key.gcs_bucket_key()
                        );
                        Ok(SharedCacheStoreResult::Written(total_bytes))
                    }
                    StatusCode::PRECONDITION_FAILED => Ok(SharedCacheStoreResult::Skipped),
                    StatusCode::FORBIDDEN => Err(anyhow!(
                        "Insufficient permissions for bucket {}",
                        self.config.bucket
                    )
                    .into()),
                    StatusCode::UNAUTHORIZED => Err(anyhow!("Invalid credentials").into()),
                    _ => Err(anyhow!("Error response from GCS: {}", status).into()),
                }
            }
            Ok(Err(err)) => {
                tracing::trace!(
                    "Error in shared_cache GCS response for {}",
                    key.gcs_bucket_key()
                );
                Err(err).context("Bad GCS response for shared_cache")?
            }
            Err(_) => Err(CacheError::ConnectTimeout),
        }
    }
}

impl FilesystemSharedCacheConfig {
    /// Fetches item from shared cache if available and copies them to the writer.
    ///
    /// # Returns
    ///
    /// If successful the number of bytes written to the writer are returned.
    async fn fetch<W>(
        &self,
        key: &SharedCacheKey,
        writer: &mut W,
    ) -> Result<Option<u64>, CacheError>
    where
        W: AsyncWrite + Unpin,
    {
        let abspath = self.path.join(key.relative_path());
        tracing::debug!("Fetching debug file from {}", abspath.display());
        let mut file = match File::open(abspath).await {
            Ok(file) => file,
            Err(err) => match err.kind() {
                io::ErrorKind::NotFound => return Ok(None),
                _ => return Err(err).context("Failed to open file in shared cache")?,
            },
        };
        match io::copy(&mut file, writer).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) => Err(err).context("Failed to copy file from shared cache")?,
        }
    }

    async fn store(
        &self,
        key: SharedCacheKey,
        mut src: File,
    ) -> Result<SharedCacheStoreResult, CacheError> {
        let abspath = self.path.join(key.relative_path());
        let parent_dir = abspath
            .parent()
            .ok_or_else(|| Error::msg("Shared cache directory not found"))?;
        fs::create_dir_all(parent_dir)
            .await
            .context("Failed to create parent directories")?;
        if abspath.as_path().exists() {
            return Ok(SharedCacheStoreResult::Skipped);
        }

        let temp_dir = parent_dir.join(".tmp");
        fs::create_dir_all(&temp_dir)
            .await
            .context("failed to create tempdir")?;
        let temp_file = NamedTempFile::new_in(&temp_dir).context("failed to create tempfile")?;
        let dup_file = temp_file.reopen().context("failed to dup filedescriptor")?;
        let mut dest = File::from_std(dup_file);

        src.rewind().await.context("failed to rewind")?;
        let bytes = io::copy(&mut src, &mut dest)
            .await
            .context("Failed to copy data into file")?;

        temp_file
            .persist(abspath)
            .context("Failed to save file in shared cache")?;
        Ok(SharedCacheStoreResult::Written(bytes))
    }
}

/// The result of an attempt to write an entry to the shared cache.
#[derive(Debug, Clone, Copy)]
enum SharedCacheStoreResult {
    /// Successfully written to the cache as a new entry, contains number of bytes written.
    Written(u64),
    /// Skipped writing the item as it was already on the cache.
    Skipped,
}

impl AsRef<str> for SharedCacheStoreResult {
    fn as_ref(&self) -> &str {
        match self {
            SharedCacheStoreResult::Written(_) => "written",
            SharedCacheStoreResult::Skipped => "skipped",
        }
    }
}

impl fmt::Display for SharedCacheStoreResult {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.as_ref())
    }
}

/// Key for a shared cache item.
#[derive(Debug, Clone)]
pub struct SharedCacheKey {
    /// The name of the cache.
    pub name: CacheName,
    /// The cache version.
    pub version: u32,
    /// The local cache key.
    pub local_key: CacheKey,
}

impl SharedCacheKey {
    /// The relative path of this cache key within a shared cache.
    fn relative_path(&self) -> PathBuf {
        // Note that this always pushes the version into the path, this is fine since we do
        // not need any backwards compatibility with existing caches for the shared cache.
        let mut path = PathBuf::new();
        path.push(self.name.to_string());
        path.push(self.version.to_string());
        path.push(self.local_key.relative_path());
        path
    }

    /// The [`SharedCacheKey::relative_path`] as a GCS bucket key.
    fn gcs_bucket_key(&self) -> String {
        // All our paths should be UTF-8, we don't construct non-UTF-8 paths.
        match self.relative_path().to_str() {
            Some(s) => s.to_owned(),
            None => {
                tracing::error!(
                    "Non UTF-8 path in SharedCacheKey: {}",
                    self.relative_path().display()
                );
                self.relative_path().to_string_lossy().into_owned()
            }
        }
    }
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
enum SharedCacheBackend {
    Gcs(GcsState),
    Fs(FilesystemSharedCacheConfig),
}

impl SharedCacheBackend {
    /// Creates the backend.
    ///
    /// If the backend can not be created the error will already be reported.
    async fn maybe_new(cfg: SharedCacheBackendConfig) -> Option<Self> {
        match cfg {
            SharedCacheBackendConfig::Gcs(cfg) => {
                match GcsState::try_new(cfg)
                    .await
                    .context("Failed to initialise GCS backend for shared cache")
                {
                    Ok(state) => Some(SharedCacheBackend::Gcs(state)),
                    Err(err) => {
                        sentry::capture_error(&*err);
                        None
                    }
                }
            }
            // TODO: We could check if we can write in the configured directory here, but
            // this is only test backend so not very important.
            SharedCacheBackendConfig::Filesystem(cfg) => Some(SharedCacheBackend::Fs(cfg)),
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Gcs(_) => "GCS",
            Self::Fs(_) => "filesystem",
        }
    }
}

/// Message to send upload tasks across the [`InnerSharedCacheService::upload_queue_tx`].
#[derive(Debug)]
struct UploadMessage {
    /// The cache key to store the data at.
    key: SharedCacheKey,
    /// The [`File`] to read the cache data from.
    src: File,
    /// A channel to notify completion of storage.
    done_tx: oneshot::Sender<()>,
    /// The reason to store this item.
    reason: CacheStoreReason,
}

/// Reasons to store items in the shared cache.
///
/// This is used for reporting metrics only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheStoreReason {
    /// The item was newly fetched and never encountered before.
    New,
    /// The item was already found in the local cache, but we extended its lifetime.
    Refresh,
}

impl AsRef<str> for CacheStoreReason {
    fn as_ref(&self) -> &str {
        match self {
            CacheStoreReason::New => "new",
            CacheStoreReason::Refresh => "refresh",
        }
    }
}

/// A shared cache service.
///
/// For simplicity in the rest of the application this service always exists, regardless of
/// whether it is configured or not.  If it is not configured calls to it's methods become a
/// no-op.
///
/// Initialising is asynchronous since it may take some time.
#[derive(Debug, Clone)]
pub struct SharedCacheService {
    inner: Arc<RwLock<Option<InnerSharedCacheService>>>,
}

#[derive(Debug)]
struct InnerSharedCacheService {
    backend: Arc<SharedCacheBackend>,
    upload_queue_tx: mpsc::Sender<UploadMessage>,
}

impl SharedCacheService {
    pub async fn new(config: Option<SharedCacheConfig>) -> Self {
        let inner = Arc::new(RwLock::new(None));
        let slf = Self {
            inner: inner.clone(),
        };
        if let Some(cfg) = config {
            tokio::spawn(Self::init(inner, cfg));
        }
        slf
    }

    async fn init(inner: Arc<RwLock<Option<InnerSharedCacheService>>>, config: SharedCacheConfig) {
        let (tx, rx) = mpsc::channel(config.max_upload_queue_size);
        if let Some(backend) = SharedCacheBackend::maybe_new(config.backend).await {
            let backend = Arc::new(backend);
            tokio::spawn(
                Self::upload_worker(rx, backend.clone(), config.max_concurrent_uploads)
                    .bind_hub(Hub::new_from_top(Hub::current())),
            );
            *inner.write().await = Some(InnerSharedCacheService {
                backend,
                upload_queue_tx: tx,
            });
        }
    }

    /// Long running task managing concurrent uploads to the shared cache.
    async fn upload_worker(
        mut work_rx: mpsc::Receiver<UploadMessage>,
        backend: Arc<SharedCacheBackend>,
        max_concurrent_uploads: usize,
    ) {
        let (done_tx, mut done_rx) = mpsc::channel::<()>(max_concurrent_uploads);
        let mut uploads_counter = max_concurrent_uploads;
        loop {
            tokio::select! {
                Some(message) = work_rx.recv(), if uploads_counter > 0 => {
                    uploads_counter -= 1;
                    tokio::spawn(
                        Self::single_uploader(done_tx.clone(), backend.clone(), message)
                            .bind_hub(Hub::new_from_top(Hub::current()))
                    );
                    let uploads_in_flight: u64 = (max_concurrent_uploads - uploads_counter) as u64;
                    metric!(gauge("services.shared_cache.uploads_in_flight") = uploads_in_flight);
                }
                Some(_) = done_rx.recv() => {
                    uploads_counter += 1;
                }
                else => break,
            }
        }
        tracing::info!("Shared cache upload worker terminated");
    }

    /// Does a single upload to the shared cache backend.
    ///
    /// Handles metrics and error reporting.
    async fn single_uploader(
        done_tx: mpsc::Sender<()>,
        backend: Arc<SharedCacheBackend>,
        message: UploadMessage,
    ) {
        let UploadMessage {
            key,
            src,
            done_tx: complete_tx,
            reason,
        } = message;

        let _guard = Hub::current().push_scope();
        sentry::configure_scope(|scope| {
            let mut map = BTreeMap::new();
            map.insert("backend".to_string(), backend.name().into());
            map.insert("cache".to_string(), key.name.as_ref().into());
            map.insert(
                "path".to_string(),
                key.relative_path().to_string_lossy().into(),
            );
            scope.set_context("Shared Cache", Context::Other(map));
        });

        let cache_name = key.name;
        let res = match *backend {
            SharedCacheBackend::Gcs(ref state) => state.store(key, src, reason).await,
            SharedCacheBackend::Fs(ref cfg) => cfg.store(key, src).await,
        };
        match res {
            Ok(op) => {
                metric!(
                    counter("services.shared_cache.store") += 1,
                    "cache" => cache_name.as_ref(),
                    "write" => op.as_ref(),
                    "status" => "ok",
                    "reason" => reason.as_ref(),
                );
                if let SharedCacheStoreResult::Written(bytes) = op {
                    let bytes: i64 = bytes.try_into().unwrap_or(i64::MAX);
                    metric!(
                        counter("services.shared_cache.store.bytes") += bytes,
                        "cache" => cache_name.as_ref(),
                    );
                }
            }
            Err(outer_err) => {
                let errdetails = match outer_err {
                    CacheError::ConnectTimeout => "connect-timeout",
                    CacheError::Other(_) => "other",
                };
                if let CacheError::Other(err) = outer_err {
                    let stderr: &dyn std::error::Error = &*err;
                    tracing::error!(
                        stderr,
                        "Error storing file on {} shared cache",
                        backend.name(),
                    );
                }
                metric!(
                    counter("services.shared_cache.store") += 1,
                    "cache" => cache_name.as_ref(),
                    "status" => "error",
                    "reason" => reason.as_ref(),
                    "errdetails" => errdetails,
                );
            }
        }

        // Tell the work coordinator we're done.
        done_tx.send(()).await.unwrap_or_else(|err| {
            let stderr: &dyn std::error::Error = &err;
            tracing::error!(
                stderr,
                "Shared cache single_uploader failed to send done message",
            );
        });

        // Tell the original work submitter we're done, if they dropped this we don't care.
        complete_tx.send(()).ok();
    }

    /// Returns the name of the backend configured.
    async fn backend_name(&self) -> &'static str {
        match self.inner.read().await.as_ref() {
            Some(inner) => inner.backend.name(),
            None => "<not-configured>",
        }
    }

    /// Retrieve a file from the shared cache.
    ///
    /// Looks up the `key` in the shared cache, if found the cache contents will be written
    /// to `writer`.
    ///
    /// Returns `true` if the shared cache was found and written to the `writer`.  If the
    /// shared cache was not found nothing will have been written to `writer`.
    ///
    /// Errors are transparently hidden, either a cache item is available or it is not.
    pub async fn fetch<W>(&self, key: &SharedCacheKey, writer: &mut W) -> bool
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        let _guard = Hub::current().push_scope();
        let backend_name = self.backend_name().await;
        sentry::configure_scope(|scope| {
            let mut map = BTreeMap::new();
            map.insert("backend".to_string(), backend_name.into());
            map.insert("cache".to_string(), key.name.as_ref().into());
            map.insert(
                "path".to_string(),
                key.relative_path().to_string_lossy().into(),
            );
            scope.set_context("Shared Cache", Context::Other(map));
        });
        let res = match self.inner.read().await.as_ref() {
            Some(inner) => match inner.backend.as_ref() {
                SharedCacheBackend::Gcs(state) => state.fetch(key, writer).await,
                SharedCacheBackend::Fs(cfg) => cfg.fetch(key, writer).await,
            },
            None => return false,
        };
        match res {
            Ok(Some(bytes)) => {
                metric!(
                    counter("services.shared_cache.fetch") += 1,
                    "cache" => key.name.as_ref(),
                    "hit" => "true",
                    "status" => "ok",
                );
                let bytes: i64 = bytes.try_into().unwrap_or(i64::MAX);
                metric!(
                    counter("services.shared_cache.fetch.bytes") += bytes,
                    "cache" => key.name.as_ref(),
                );
                true
            }
            Ok(None) => {
                metric!(
                    counter("services.shared_cache.fetch") += 1,
                    "cache" => key.name.as_ref(),
                    "hit" => "false",
                    "status" => "ok",
                );
                false
            }
            Err(outer_err) => {
                let errdetails = match outer_err {
                    CacheError::ConnectTimeout => "connect-timeout",
                    CacheError::Other(_) => "other",
                };
                if let CacheError::Other(err) = outer_err {
                    let backend_name = self.backend_name().await;
                    let stderr: &dyn std::error::Error = &*err;
                    tracing::error!(stderr, "Error fetching from {} shared cache", backend_name);
                }
                metric!(
                    counter("services.shared_cache.fetch") += 1,
                    "cache" => key.name.as_ref(),
                    "status" => "error",
                    "errdetails" => errdetails,
                );
                false
            }
        }
    }

    /// Place a file on the shared cache, if it does not yet exist there.
    ///
    /// Errors are transparently hidden, this service handles any errors itself.
    ///
    /// # Return
    ///
    /// If the shared cache is enabled a [`oneshot::Receiver`] is returned which will
    /// receive a value once the file has been stored in the shared cache.  Due to
    /// backpressure it is possible that the file is never stored, in which case the
    /// corresponding [`oneshot::Sender`] is dropped and awaiting the receiver will resolve
    /// into an [`Err`].
    ///
    /// This [`oneshot::Receiver`] can also be safely ignored if you do not need to know
    /// when the file is stored.  This mostly exists to enable testing.
    ///
    /// If [`CacheStoreReason::Refresh`] is used the implementation will trade off an extra
    /// request to check if the file already exists before uploading.  This is racy but a
    /// good tradeoff for refreshed stores.
    pub async fn store(
        &self,
        key: SharedCacheKey,
        src: File,
        reason: CacheStoreReason,
    ) -> Option<oneshot::Receiver<()>> {
        let inner_guard = self.inner.read().await;
        match inner_guard.as_ref() {
            Some(inner) => {
                metric!(
                    gauge("services.shared_cache.uploads_queue_capacity") =
                        inner.upload_queue_tx.capacity() as u64
                );
                let (done_tx, done_rx) = oneshot::channel::<()>();
                inner
                    .upload_queue_tx
                    .try_send(UploadMessage {
                        key,
                        src,
                        done_tx,
                        reason,
                    })
                    .unwrap_or_else(|_| {
                        metric!(counter("services.shared_cache.store.dropped") += 1);
                        tracing::error!("Shared cache upload queue full");
                    });
                Some(done_rx)
            }
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use tempfile::NamedTempFile;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use uuid::Uuid;

    use crate::test::{self, TestGcsCredentials};
    use crate::types::Scope;

    use super::*;

    impl From<TestGcsCredentials> for GcsSharedCacheConfig {
        fn from(source: TestGcsCredentials) -> Self {
            Self {
                bucket: source.bucket,
                service_account_path: source.credentials_file,
            }
        }
    }

    async fn wait_init(service: &SharedCacheService) {
        const MAX_DELAY: Duration = Duration::from_secs(3);
        let start = Instant::now();
        loop {
            if start.elapsed() > MAX_DELAY {
                break;
            }
            if service.inner.read().await.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn test_noop_fetch() {
        test::setup();
        let svc = SharedCacheService::new(None).await;
        let key = SharedCacheKey {
            name: CacheName::Objects,
            version: 0,
            local_key: CacheKey {
                cache_key: "some_item".to_string(),
                scope: Scope::Global,
            },
        };
        let mut writer = Vec::new();

        let ret = svc.fetch(&key, &mut writer).await;
        assert!(!ret);
    }

    #[tokio::test]
    async fn test_noop_store() {
        test::setup();
        let svc = SharedCacheService::new(None).await;
        let key = SharedCacheKey {
            name: CacheName::Objects,
            version: 0,
            local_key: CacheKey {
                cache_key: "some_item".to_string(),
                scope: Scope::Global,
            },
        };
        let stdfile = tempfile::tempfile().unwrap();
        let file = File::from_std(stdfile);

        svc.store(key, file, CacheStoreReason::New).await;
    }

    #[tokio::test]
    async fn test_filesystem_fetch_found() {
        test::setup();
        let dir = test::tempdir();

        let key = SharedCacheKey {
            name: CacheName::Objects,
            version: 0,
            local_key: CacheKey {
                cache_key: "some_item".to_string(),
                scope: Scope::Global,
            },
        };
        let cache_path = dir.path().join(key.relative_path());
        fs::create_dir_all(cache_path.parent().unwrap())
            .await
            .unwrap();
        fs::write(&cache_path, b"cache data").await.unwrap();

        let cfg = SharedCacheConfig {
            max_concurrent_uploads: 10,
            max_upload_queue_size: 10,
            backend: SharedCacheBackendConfig::Filesystem(FilesystemSharedCacheConfig {
                path: dir.path().to_path_buf(),
            }),
        };
        let svc = SharedCacheService::new(Some(cfg)).await;
        wait_init(&svc).await;

        // This mimics how Cacher::compute creates this file.
        let temp_file = NamedTempFile::new_in(&dir).unwrap();
        let stdfile = temp_file.reopen().unwrap();
        let mut file = File::from_std(stdfile);

        let ret = svc.fetch(&key, &mut file).await;

        assert!(ret);
        file.rewind().await.unwrap();
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).await.unwrap();
        assert_eq!(buf, b"cache data");
    }

    #[tokio::test]
    async fn test_filesystem_fetch_not_found() {
        test::setup();
        let dir = test::tempdir();

        let key = SharedCacheKey {
            name: CacheName::Objects,
            version: 0,
            local_key: CacheKey {
                cache_key: "some_item".to_string(),
                scope: Scope::Global,
            },
        };

        let cfg = SharedCacheConfig {
            max_concurrent_uploads: 10,
            max_upload_queue_size: 10,
            backend: SharedCacheBackendConfig::Filesystem(FilesystemSharedCacheConfig {
                path: dir.path().to_path_buf(),
            }),
        };
        let svc = SharedCacheService::new(Some(cfg)).await;
        wait_init(&svc).await;

        let mut writer = Vec::new();

        let ret = svc.fetch(&key, &mut writer).await;

        assert!(!ret);
        assert_eq!(writer, b"");
    }

    #[tokio::test]
    async fn test_filesystem_store() {
        test::setup();
        let dir = test::tempdir();

        let key = SharedCacheKey {
            name: CacheName::Objects,
            version: 0,
            local_key: CacheKey {
                cache_key: "some_item".to_string(),
                scope: Scope::Global,
            },
        };
        let cache_path = dir.path().join(key.relative_path());

        let cfg = SharedCacheConfig {
            max_concurrent_uploads: 10,
            max_upload_queue_size: 10,
            backend: SharedCacheBackendConfig::Filesystem(FilesystemSharedCacheConfig {
                path: dir.path().to_path_buf(),
            }),
        };
        let svc = SharedCacheService::new(Some(cfg)).await;
        wait_init(&svc).await;

        // This mimics how the downloader and Cacher::compute write the cache data.
        let temp_file = NamedTempFile::new_in(&dir).unwrap();
        let dup_file = temp_file.reopen().unwrap();
        let temp_fd = File::from_std(dup_file);
        {
            let mut file = File::create(temp_file.path()).await.unwrap();
            file.write_all(b"cache data").await.unwrap();
            file.flush().await.unwrap();
        }

        if let Some(recv) = svc.store(key, temp_fd, CacheStoreReason::New).await {
            // Wait for storing to complete.
            recv.await.unwrap();
        }

        let data = fs::read(&cache_path)
            .await
            .context("Failed to read written cache file")
            .unwrap();
        assert_eq!(data, b"cache data");
    }

    #[tokio::test]
    async fn test_gcs_fetch_not_found() {
        test::setup();
        let credentials = test::gcs_credentials!();

        let key = SharedCacheKey {
            name: CacheName::Objects,
            version: 0,
            local_key: CacheKey {
                cache_key: "some_item".to_string(),
                scope: Scope::Scoped(Uuid::new_v4().to_string()),
            },
        };

        let cfg = SharedCacheConfig {
            max_concurrent_uploads: 10,
            max_upload_queue_size: 10,
            backend: SharedCacheBackendConfig::Gcs(GcsSharedCacheConfig::from(credentials)),
        };
        let svc = SharedCacheService::new(Some(cfg)).await;
        wait_init(&svc).await;

        let mut writer = Vec::new();

        let ret = svc.fetch(&key, &mut writer).await;

        assert!(!ret);
        assert_eq!(writer, b"");
    }

    #[tokio::test]
    async fn test_gcs_state_fetch_not_found() {
        test::setup();
        let credentials = test::gcs_credentials!();

        let key = SharedCacheKey {
            name: CacheName::Objects,
            version: 0,
            local_key: CacheKey {
                cache_key: "some_item".to_string(),
                scope: Scope::Scoped(Uuid::new_v4().to_string()),
            },
        };

        let state = GcsState::try_new(GcsSharedCacheConfig::from(credentials))
            .await
            .unwrap();

        let mut writer = Vec::new();

        let ret = state.fetch(&key, &mut writer).await.unwrap();

        assert!(ret.is_none());
        assert_eq!(writer, b"");
    }

    #[tokio::test]
    async fn test_gcs_svc_store_fetch() {
        test::setup();
        let dir = test::tempdir();

        let key = SharedCacheKey {
            name: CacheName::Objects,
            version: 0,
            local_key: CacheKey {
                cache_key: "some_item".to_string(),
                scope: Scope::Scoped(Uuid::new_v4().to_string()),
            },
        };

        let credentials = test::gcs_credentials!();
        let cfg = SharedCacheConfig {
            max_concurrent_uploads: 10,
            max_upload_queue_size: 10,
            backend: SharedCacheBackendConfig::Gcs(GcsSharedCacheConfig::from(credentials)),
        };
        let svc = SharedCacheService::new(Some(cfg)).await;
        wait_init(&svc).await;

        // This mimics how the downloader and Cacher::compute write the cache data.
        let temp_file = NamedTempFile::new_in(&dir).unwrap();
        let dup_file = temp_file.reopen().unwrap();
        let temp_fd = File::from_std(dup_file);
        {
            let mut file = File::create(temp_file.path()).await.unwrap();
            file.write_all(b"cache data").await.unwrap();
            file.flush().await.unwrap();
        }

        if let Some(recv) = svc.store(key.clone(), temp_fd, CacheStoreReason::New).await {
            // Wait for storing to complete.
            recv.await.unwrap();
        }

        let mut writer = Vec::new();

        let ret = svc.fetch(&key, &mut writer).await;

        assert!(ret);
        assert_eq!(writer, b"cache data");
    }

    #[tokio::test]
    async fn test_gcs_state_store_twice() {
        test::setup();
        let credentials = test::gcs_credentials!();

        let key = SharedCacheKey {
            name: CacheName::Objects,
            version: 0,
            local_key: CacheKey {
                cache_key: "some_item".to_string(),
                scope: Scope::Scoped(Uuid::new_v4().to_string()),
            },
        };

        let state = GcsState::try_new(GcsSharedCacheConfig::from(credentials))
            .await
            .unwrap();

        // This mimics how the downloader and Cacher::compute write the cache data.
        let temp_file = NamedTempFile::new().unwrap();
        let dup_file = temp_file.reopen().unwrap();
        let temp_fd = File::from_std(dup_file);
        {
            let mut file = File::create(temp_file.path()).await.unwrap();
            file.write_all(b"cache data").await.unwrap();
            file.flush().await.unwrap();
        }

        let ret = state
            .store(key.clone(), temp_fd, CacheStoreReason::New)
            .await
            .unwrap();

        assert!(matches!(ret, SharedCacheStoreResult::Written(_)));

        let dup_file = temp_file.reopen().unwrap();
        let temp_fd = File::from_std(dup_file);

        let ret = state
            .store(key, temp_fd, CacheStoreReason::New)
            .await
            .unwrap();

        assert!(matches!(ret, SharedCacheStoreResult::Skipped));
    }

    #[tokio::test]
    async fn test_gcs_exists() {
        test::setup();
        let credentials = test::gcs_credentials!();
        let state = GcsState::try_new(GcsSharedCacheConfig::from(credentials))
            .await
            .unwrap();

        let key = SharedCacheKey {
            name: CacheName::Objects,
            version: 0,
            local_key: CacheKey {
                cache_key: "some_item".to_string(),
                scope: Scope::Scoped(Uuid::new_v4().to_string()),
            },
        };

        assert!(!state.exists(&key).await.unwrap());

        let fd = tempfile::tempfile().unwrap();
        let mut fd = File::from_std(fd);
        fd.write_all(b"cache data").await.unwrap();
        fd.flush().await.unwrap();
        state
            .store(key.clone(), fd, CacheStoreReason::New)
            .await
            .unwrap();

        assert!(state.exists(&key).await.unwrap());
    }
}

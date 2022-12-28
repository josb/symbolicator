//! Support to download from S3 buckets.
//!
//! Specifically this supports the [`S3SourceConfig`] source.

use futures::TryStreamExt;
use std::any::type_name;
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use aws_config::meta::credentials::lazy_caching::LazyCachingCredentialsProvider;
use aws_sdk_s3::error::GetObjectErrorKind;
use aws_sdk_s3::types::SdkError::*;
use aws_sdk_s3::{Client, ErrorExt};
use aws_types::credentials::{Credentials, ProvideCredentials};
use aws_types::region::Region;

use symbolicator_sources::{
    AwsCredentialsProvider, FileType, ObjectId, S3SourceConfig, S3SourceKey,
};

use super::locations::SourceLocation;
use super::{content_length_timeout, DownloadError, DownloadStatus, RemoteDif, RemoteDifUri};

type ClientCache = moka::future::Cache<Arc<S3SourceKey>, Arc<Client>>;

/// The S3-specific [`RemoteDif`].
#[derive(Debug, Clone)]
pub struct S3RemoteDif {
    pub source: Arc<S3SourceConfig>,
    pub location: SourceLocation,
}

impl From<S3RemoteDif> for RemoteDif {
    fn from(source: S3RemoteDif) -> Self {
        Self::S3(source)
    }
}

impl From<&S3RemoteDif> for RemoteDif {
    fn from(source: &S3RemoteDif) -> Self {
        Self::S3(source.clone())
    }
}

impl S3RemoteDif {
    pub fn new(source: Arc<S3SourceConfig>, location: SourceLocation) -> Self {
        Self { source, location }
    }

    /// Returns the S3 key.
    ///
    /// This is equivalent to the pathname within the bucket.
    pub fn key(&self) -> String {
        self.location.prefix(&self.source.prefix)
    }

    /// Returns the S3 bucket name.
    pub fn bucket(&self) -> String {
        self.source.bucket.clone()
    }

    /// Returns the `s3://` URI from which to download this object file.
    pub fn uri(&self) -> RemoteDifUri {
        RemoteDifUri::from_parts("s3", &self.source.bucket, &self.key())
    }
}

/// Downloader implementation that supports the [`S3SourceConfig`] source.
pub struct S3Downloader {
    client_cache: ClientCache,
    connect_timeout: Duration,
    streaming_timeout: Duration,
}

impl fmt::Debug for S3Downloader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct(type_name::<Self>())
            .field("connect_timeout", &self.connect_timeout)
            .field("streaming_timeout", &self.streaming_timeout)
            .finish()
    }
}

pub type S3Error = aws_sdk_s3::Error;

impl S3Downloader {
    pub fn new(
        connect_timeout: Duration,
        streaming_timeout: Duration,
        s3_client_capacity: u64,
    ) -> Self {
        Self {
            client_cache: ClientCache::new(s3_client_capacity),
            connect_timeout,
            streaming_timeout,
        }
    }

    async fn get_s3_client(&self, key: &Arc<S3SourceKey>) -> Arc<Client> {
        if self.client_cache.contains_key(key) {
            metric!(counter("source.s3.client.cached") += 1);
        }

        self.client_cache
            .get_with_by_ref(key, async {
                metric!(counter("source.s3.client.create") += 1);

                let region = key.region.clone();
                tracing::debug!(
                    "Using AWS credentials provider: {:?}",
                    key.aws_credentials_provider
                );
                Arc::new(match key.aws_credentials_provider {
                    AwsCredentialsProvider::Container => {
                        let provider = LazyCachingCredentialsProvider::builder()
                            .load(aws_config::ecs::EcsCredentialsProvider::builder().build())
                            .build();
                        self.create_s3_client(provider, region).await
                    }
                    AwsCredentialsProvider::Static => {
                        let provider = Credentials::from_keys(
                            key.access_key.clone(),
                            key.secret_key.clone(),
                            None,
                        );
                        self.create_s3_client(provider, region).await
                    }
                })
            })
            .await
    }

    async fn create_s3_client(
        &self,
        provider: impl ProvideCredentials + Send + Sync + 'static,
        region: Region,
    ) -> Client {
        let shared_config = aws_config::from_env()
            .credentials_provider(provider)
            .region(region)
            .load()
            .await;
        Client::new(&shared_config)
    }

    /// Downloads a source hosted on an S3 bucket.
    ///
    /// # Directly thrown errors
    /// - [`DownloadError::Io`]
    /// - [`DownloadError::Canceled`]
    pub async fn download_source(
        &self,
        file_source: S3RemoteDif,
        destination: &Path,
    ) -> Result<DownloadStatus, DownloadError> {
        let key = file_source.key();
        let bucket = file_source.bucket();
        tracing::debug!("Fetching from s3: {} (from {})", &key, &bucket);

        let source_key = &file_source.source.source_key;
        let client = self.get_s3_client(source_key).await;
        let request = client.get_object().bucket(&bucket).key(&key).send();

        let source = RemoteDif::from(&file_source);
        let request = tokio::time::timeout(self.connect_timeout, request);
        let request = super::measure_download_time(source.source_metric_key(), request);

        let response = match request.await {
            Ok(Ok(response)) => response,
            Ok(Err(err1)) => {
                tracing::debug!("Skipping response from s3://{}/{}: {}", &bucket, &key, err1);
                match err1 {
                    ConstructionFailure(err) => {
                        println!("ERROR: ConstructionFailure: {:?}", err);
                        return Ok(DownloadStatus::NotFound)
                    }
                    DispatchFailure(err) => {
                        println!("ERROR: DispatchFailure: {:?}", err);
                        return Ok(DownloadStatus::NotFound)
                    }
                    TimeoutError(err) => {
                        println!("ERROR: TimeoutError: {:?}", err);
                        return Ok(DownloadStatus::NotFound)
                    }
                    ResponseError(err) => {
                        println!("ERROR: ResponseError: {:?}", err);
                        return Ok(DownloadStatus::NotFound)
                    }
                    ServiceError(ref err) => {
                        let raw = err.raw();
                        println!("ERROR: ServiceError: raw: {:?}", raw);
                        let http_response = raw.http();
                        let body = http_response.body();
                        let status = http_response.status();
                        println!("HTTP status: {:?}", status);

                        let get_object_error = err.err();
                        let meta = get_object_error.meta();
                        let message = meta.message();
                        let code = meta.code();
                        println!("GetObjectError meta: {:?}", meta);
                        println!("GetObjectError meta.code: {:?}", code.unwrap());
                        println!("GetObjectError meta.message: {:?}", message.unwrap());
                        println!(
                            "GetObjectError meta.request_id: {:?}",
                            meta.request_id().unwrap()
                        );
                        println!(
                            "GetObjectError meta.s3_extended_request_id: {:?}",
                            meta.extended_request_id().unwrap()
                        );
                        match &get_object_error.kind {
                            GetObjectErrorKind::InvalidObjectState(err) => {
                                println!("InvalidObjectState: {:?}", err);
                            }
                            GetObjectErrorKind::NoSuchKey(err) => {
                                println!("NoSuchKey: {:?}", err);
                                return Ok(DownloadStatus::NotFound)
                            }
                            GetObjectErrorKind::Unhandled(err) => {
                                println!("Unhandled: {:?}", err);
                            }
                            err => {
                                println!("Unknown: {:?}", err);
                            }
                        }
                        sentry::configure_scope(|scope| {
                            let body_text = body.bytes();
                            if let Some(text) = body_text {
                                scope.set_extra("AWS body:", text.into());
                            }
                            if let Some(message) = message {
                                scope.set_extra("AWS message", message.into());
                            }
                            if let Some(code) = code {
                                scope.set_extra("AWS code", code.into());
                            }
                        });
                        if let Some(code) = code {
                            return Err(DownloadError::S3WithCode(
                                status,
                                code.to_string(),
                            ));
                        } else {
                            return Err(DownloadError::S3(err1.into()));
                        }
                    }
                    err => {
                        println!("ERROR: Unhandled: {:?}", err);
                        return Err(DownloadError::S3(err.into()))
                    }
                }
            }
            Err(_) => {
                // TODO Verify this is still the correct action to take with aws-sdk-rust
                // Timed out
                return Err(DownloadError::Canceled)
            }
        };

        let timeout = Some(content_length_timeout(
            response.content_length(),
            self.streaming_timeout,
        ));

        let stream = if response.content_length == 0 {
            tracing::debug!("Empty response from s3:{}{}", &bucket, &key);
            return Ok(DownloadStatus::NotFound);
        } else {
            response.body.map_err(DownloadError::S3Sdk)
        };

        super::download_stream(&source, stream, destination, timeout).await
    }

    pub fn list_files(
        &self,
        source: Arc<S3SourceConfig>,
        filetypes: &[FileType],
        object_id: &ObjectId,
    ) -> Vec<RemoteDif> {
        super::SourceLocationIter {
            filetypes: filetypes.iter(),
            filters: &source.files.filters,
            object_id,
            layout: source.files.layout,
            next: Vec::new(),
        }
        .map(|loc| S3RemoteDif::new(source.clone(), loc).into())
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::Path;

    use symbolicator_sources::{CommonSourceConfig, DirectoryLayoutType, ObjectType, SourceId};

    use crate::test;

    use aws_sdk_s3::client::Client;
    use aws_sdk_s3::error::{HeadBucketError, HeadBucketErrorKind};
    use aws_sdk_s3::error::{HeadObjectError, HeadObjectErrorKind};
    use aws_smithy_http::byte_stream::ByteStream;
    use sha1::{Digest as _, Sha1};

    /// Name of the bucket to create for testing.
    const S3_BUCKET: &str = "symbolicator-test";

    fn s3_source_key() -> Option<S3SourceKey> {
        let access_key = std::env::var("SENTRY_SYMBOLICATOR_TEST_AWS_ACCESS_KEY_ID").ok()?;
        let secret_key = std::env::var("SENTRY_SYMBOLICATOR_TEST_AWS_SECRET_ACCESS_KEY").ok()?;

        if access_key.is_empty() || secret_key.is_empty() {
            None
        } else {
            Some(S3SourceKey {
                region: Region::from_static("us-east-1"),
                aws_credentials_provider: AwsCredentialsProvider::Static,
                access_key,
                secret_key,
            })
        }
    }

    macro_rules! s3_source_key {
        () => {
            match s3_source_key() {
                Some(key) => key,
                None => {
                    println!("Skipping due to missing SENTRY_SYMBOLICATOR_TEST_AWS_ACCESS_KEY_ID or SENTRY_SYMBOLICATOR_TEST_AWS_SECRET_ACCESS_KEY");
                    return;
                }
            }
        }
    }

    fn s3_source(source_key: S3SourceKey) -> Arc<S3SourceConfig> {
        Arc::new(S3SourceConfig {
            id: SourceId::new("s3-test"),
            bucket: S3_BUCKET.to_owned(),
            prefix: String::new(),
            source_key: Arc::new(source_key),
            files: CommonSourceConfig::with_layout(DirectoryLayoutType::Unified),
        })
    }

    /// Creates an S3 bucket if it does not exist.
    async fn ensure_bucket(s3_client: &Client) {
        let head_result = s3_client
            .head_bucket()
            .bucket(S3_BUCKET.to_owned())
            .send()
            .await;

        match head_result {
            Ok(_) => return,
            Err(err) => match err.into_service_error() {
                HeadBucketError {
                    kind: HeadBucketErrorKind::NotFound(err),
                    ..
                } => {
                    println!("{:?}", err.message())
                }
                HeadBucketError {
                    kind: HeadBucketErrorKind::Unhandled(err),
                    ..
                } => {
                    println!("{:?}", err)
                }
                err => {
                    panic!("failed to check S3 bucket: {:?}", err)
                }
            },
        }

        s3_client
            .create_bucket()
            .bucket(S3_BUCKET.to_owned())
            .send()
            .await
            .unwrap();
    }

    /// Loads a mock fixture into the S3 bucket if it does not exist.
    async fn ensure_fixture(s3_client: &Client, fixture: impl AsRef<Path>, key: impl Into<String>) {
        let key = key.into();
        let head_result = s3_client
            .head_object()
            .bucket(S3_BUCKET.to_owned())
            .key(key.clone())
            .send()
            .await;

        match head_result {
            Ok(_) => return,
            Err(err) => match err.into_service_error() {
                HeadObjectError {
                    kind: HeadObjectErrorKind::NotFound(err),
                    ..
                } => println!("{:?}", err.message()),
                HeadObjectError {
                    kind: HeadObjectErrorKind::Unhandled(err),
                    ..
                } => println!("{:?}", err),
                err => panic!("failed to check S3 object: {:?}", err),
            },
        }

        s3_client
            .put_object()
            .bucket(S3_BUCKET.to_owned())
            .body(ByteStream::from(test::read_fixture(fixture)))
            .key(key)
            .send()
            .await
            .unwrap();
    }

    /// Loads mock data into the S3 test bucket.
    ///
    /// This performs a series of actions:
    ///  - If the bucket does not exist, it will be created.
    ///  - Each file is checked and created if it does not exist.
    ///
    /// On error, the function panics with the error message.
    async fn setup_bucket(source_key: S3SourceKey) {
        let provider = Credentials::from_keys(
            source_key.access_key.clone(),
            source_key.secret_key.clone(),
            None,
        );
        let shared_config = aws_config::from_env()
            .credentials_provider(provider)
            .region(source_key.region)
            .load()
            .await;
        let s3_client = Client::new(&shared_config);

        ensure_bucket(&s3_client).await;
        ensure_fixture(
            &s3_client,
            "symbols/502F/C0A5/1EC1/3E47/9998/684FA139DCA7",
            "50/2fc0a51ec13e479998684fa139dca7/debuginfo",
        )
        .await;
    }

    #[test]
    fn test_list_files() {
        test::setup();

        let source = s3_source(s3_source_key!());
        let downloader = S3Downloader::new(Duration::from_secs(30), Duration::from_secs(30), 100);

        let object_id = ObjectId {
            code_id: Some("502fc0a51ec13e479998684fa139dca7".parse().unwrap()),
            code_file: Some("Foo.app/Contents/Foo".to_owned()),
            debug_id: Some("502fc0a5-1ec1-3e47-9998-684fa139dca7".parse().unwrap()),
            debug_file: Some("Foo".to_owned()),
            object_type: ObjectType::Macho,
        };

        let list = downloader.list_files(source, &[FileType::MachDebug], &object_id);
        assert_eq!(list.len(), 1);

        assert!(list[0]
            .uri()
            .to_string()
            .ends_with("50/2fc0a51ec13e479998684fa139dca7/debuginfo"));
    }

    #[tokio::test]
    async fn test_download_complete() {
        test::setup();

        let source_key = s3_source_key!();
        setup_bucket(source_key.clone()).await;

        let source = s3_source(source_key);
        let downloader = S3Downloader::new(Duration::from_secs(30), Duration::from_secs(30), 100);

        let tempdir = test::tempdir();
        let target_path = tempdir.path().join("myfile");

        let source_location = SourceLocation::new("50/2fc0a51ec13e479998684fa139dca7/debuginfo");
        let file_source = S3RemoteDif::new(source, source_location);

        let download_status = downloader
            .download_source(file_source, &target_path)
            .await
            .unwrap();

        assert_eq!(download_status, DownloadStatus::Completed);
        assert!(target_path.exists());

        let hash = Sha1::digest(std::fs::read(target_path).unwrap());
        let hash = format!("{:x}", hash);
        assert_eq!(hash, "e0195c064783997b26d6e2e625da7417d9f63677");
    }

    #[tokio::test]
    async fn test_download_missing() {
        test::setup();

        let source_key = s3_source_key!();
        setup_bucket(source_key.clone()).await;

        let source = s3_source(source_key);
        let downloader = S3Downloader::new(Duration::from_secs(30), Duration::from_secs(30), 100);

        let tempdir = test::tempdir();
        let target_path = tempdir.path().join("myfile");

        let source_location = SourceLocation::new("does/not/exist");
        let file_source = S3RemoteDif::new(source, source_location);

        let download_status = downloader
            .download_source(file_source, &target_path)
            .await
            .unwrap();

        assert_eq!(download_status, DownloadStatus::NotFound);
        assert!(!target_path.exists());
    }

    #[tokio::test]
    async fn test_download_invalid_credentials() {
        test::setup();

        let broken_key = S3SourceKey {
            region: Region::from_static("us-east-1"),
            aws_credentials_provider: AwsCredentialsProvider::Static,
            access_key: "".to_owned(),
            secret_key: "".to_owned(),
        };
        let source = s3_source(broken_key);
        let downloader = S3Downloader::new(Duration::from_secs(30), Duration::from_secs(30), 100);

        let tempdir = test::tempdir();
        let target_path = tempdir.path().join("myfile");

        let source_location = SourceLocation::new("does/not/exist");
        let file_source = S3RemoteDif::new(source, source_location);

        downloader
            .download_source(file_source, &target_path)
            .await
            .expect_err("authentication should fail");

        assert!(!target_path.exists());
    }

    #[test]
    fn test_s3_remote_dif_uri() {
        let source_key = Arc::new(S3SourceKey {
            region: Region::from_static("us-east-1"),
            aws_credentials_provider: AwsCredentialsProvider::Static,
            access_key: String::from("abc"),
            secret_key: String::from("123"),
        });
        let source = Arc::new(S3SourceConfig {
            id: SourceId::new("s3-id"),
            bucket: String::from("bucket"),
            prefix: String::from("prefix"),
            source_key,
            files: CommonSourceConfig::with_layout(DirectoryLayoutType::Unified),
        });
        let location = SourceLocation::new("a/key/with spaces");

        let dif = S3RemoteDif::new(source, location);
        assert_eq!(
            dif.uri(),
            RemoteDifUri::new("s3://bucket/prefix/a/key/with%20spaces")
        );
    }
}

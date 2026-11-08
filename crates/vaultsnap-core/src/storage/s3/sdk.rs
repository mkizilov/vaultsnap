//! The adapter between [`ObjectApi`] and the AWS SDK for Rust: the only code in VaultSnap that
//! talks to the network for S3.
//!
//! * **Credentials and region** come from the SDK's default provider chain: environment
//!   variables, the shared config and credentials files (and the profile `AWS_PROFILE` or the
//!   configuration names), SSO, web identity, ECS and EC2 roles. This file never reads, stores,
//!   or prints a credential.
//! * **TLS** is the SDK's rustls client with the platform's root certificates, or with exactly the
//!   certificates of a PEM bundle when one is configured. Certificate and host name verification
//!   cannot be switched off.
//! * **Retries** inside the SDK are disabled: [`RetryingBackend`](crate::storage::RetryingBackend)
//!   retries instead, and must see every attempt to recognize a conditional write whose answer
//!   was lost.
//! * **Checksums.** Since early 2025 the SDK adds a checksum trailer to uploads by default, which
//!   many S3-compatible stores do not understand. Checksums are calculated only where S3 requires
//!   them; VaultSnap's objects are authenticated by their own encryption anyway.
//! * **Threads.** The SDK is asynchronous and the backend is not. The adapter owns a small Tokio
//!   runtime (two threads), starts each call on it, and waits for the answer on a channel, so it
//!   works from any thread, inside or outside another runtime.

use std::fmt;
use std::future::Future;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aws_config::meta::region::RegionProviderChain;
use aws_config::{BehaviorVersion, Region};
use aws_sdk_s3::error::{DisplayErrorContext, ProvideErrorMetadata, SdkError};
use aws_sdk_s3::primitives::{ByteStream, DateTime};
use aws_sdk_s3::Client;
use aws_smithy_http_client::tls::{self, rustls_provider::CryptoMode};
use aws_smithy_types::checksum_config::{RequestChecksumCalculation, ResponseChecksumValidation};
use tokio::runtime::{Builder, Handle, Runtime};

use super::api::{
    classify_response, ApiError, ApiErrorKind, ApiResult, Condition, GetOutput, HeadInfo,
    ListPage, ListedObject, ObjectApi,
};
use super::{redact, S3Config};
use crate::error::{BackendErrorKind, Error, Result};

/// A client for one bucket, with the runtime it runs on.
pub(super) struct SdkApi {
    runtime: Option<Runtime>,
    handle: Handle,
    client: Client,
    bucket: String,
}

impl fmt::Debug for SdkApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SdkApi")
            .field("bucket", &self.bucket)
            .finish_non_exhaustive()
    }
}

impl Drop for SdkApi {
    fn drop(&mut self) {
        // Dropping a runtime waits for its threads and panics inside asynchronous code; this does
        // neither.
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

impl SdkApi {
    /// Starts the runtime and builds the client. Does not contact the service.
    pub(super) fn new(config: &S3Config) -> Result<Self> {
        let fail = |message: String| {
            Error::backend("connect", &config.bucket, BackendErrorKind::Other, message)
        };
        let ca_pem = match &config.ca_bundle {
            Some(path) => Some(read_ca_bundle(path).map_err(&fail)?),
            None => None,
        };
        let runtime = Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("vaultsnap-s3")
            .enable_all()
            .build()
            .map_err(|err| fail(format!("cannot start the S3 runtime: {err}")))?;
        let handle = runtime.handle().clone();

        let (sender, receiver) = mpsc::channel();
        let settings = config.clone();
        handle.spawn(async move {
            let _ = sender.send(build_client(settings, ca_pem).await);
        });
        let client = match receiver.recv() {
            Ok(Ok(client)) => client,
            Ok(Err(message)) => {
                runtime.shutdown_background();
                return Err(fail(message));
            }
            Err(_) => {
                runtime.shutdown_background();
                return Err(fail("the S3 client could not be set up".to_owned()));
            }
        };
        Ok(Self {
            runtime: Some(runtime),
            handle,
            client,
            bucket: config.bucket.clone(),
        })
    }

    /// Runs `future` on the runtime and waits for its result.
    fn call<T: Send + 'static>(
        &self,
        future: impl Future<Output = ApiResult<T>> + Send + 'static,
    ) -> ApiResult<T> {
        let (sender, receiver) = mpsc::channel();
        self.handle.spawn(async move {
            let _ = sender.send(future.await);
        });
        receiver.recv().unwrap_or_else(|_| {
            Err(ApiError::new(
                ApiErrorKind::Transient,
                "the S3 client stopped before it answered",
            ))
        })
    }
}

fn read_ca_bundle(path: &Path) -> std::result::Result<Vec<u8>, String> {
    let pem = std::fs::read(path)
        .map_err(|err| format!("cannot read the CA bundle {}: {err}", path.display()))?;
    if !String::from_utf8_lossy(&pem).contains("-----BEGIN CERTIFICATE-----") {
        return Err(format!(
            "{} does not contain a PEM encoded certificate",
            path.display()
        ));
    }
    Ok(pem)
}

/// Loads the AWS configuration (credentials chain, region, profile) and builds the S3 client.
#[allow(deprecated)]
async fn build_client(
    config: S3Config,
    ca_pem: Option<Vec<u8>>,
) -> std::result::Result<Client, String> {
    let region = match &config.region {
        Some(name) => RegionProviderChain::first_try(Region::new(name.clone())),
        None => RegionProviderChain::default_provider().or_else("us-east-1"),
    };
    let timeouts = aws_config::timeout::TimeoutConfig::builder()
        .connect_timeout(config.connect_timeout)
        .read_timeout(Duration::from_secs(60))
        .operation_attempt_timeout(config.request_timeout)
        .build();
    let mut loader = aws_config::defaults(BehaviorVersion::latest())
        .region(region)
        .retry_config(aws_config::retry::RetryConfig::disabled())
        .timeout_config(timeouts);
    if let Some(profile) = &config.profile {
        loader = loader.profile_name(profile.clone());
    }
    if let Some(pem) = ca_pem {
        let trust_store = tls::TrustStore::empty().with_pem_certificate(pem);
        let context = tls::TlsContext::builder()
            .with_trust_store(trust_store)
            .build()
            .map_err(|err| format!("the CA bundle cannot be used: {err}"))?;
        let http_client = aws_smithy_http_client::Builder::new()
            .tls_provider(tls::Provider::Rustls(CryptoMode::AwsLc))
            .tls_context(context)
            .build_https();
        loader = loader.http_client(http_client);
    }
    let shared = loader.load().await;

    let path_style = config.path_style.unwrap_or(config.endpoint.is_some());
    let mut builder = aws_sdk_s3::config::Builder::from(&shared)
        .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
        .response_checksum_validation(ResponseChecksumValidation::WhenRequired)
        .force_path_style(path_style);
    if let Some(endpoint) = &config.endpoint {
        builder = builder.endpoint_url(endpoint.clone());
    }
    Ok(Client::from_conf(builder.build()))
}

/// Turns an SDK error into what the backend decides on. Failures to reach the service at all
/// (no route, refused connection, timeout, a response that breaks off) are transient; a failure
/// to find credentials is reported as a refusal, because waiting will not produce any.
fn convert<E>(error: &SdkError<E>) -> ApiError
where
    E: ProvideErrorMetadata + std::error::Error + 'static,
{
    let status = error
        .raw_response()
        .map(|response| response.status().as_u16());
    let code = ProvideErrorMetadata::code(error).map(str::to_owned);
    let message = redact(&DisplayErrorContext(error).to_string());
    let is_service_error = matches!(error, SdkError::ServiceError(_));
    let kind = if is_service_error {
        classify_response(status, code.as_deref())
    } else if message.to_ascii_lowercase().contains("credential") {
        ApiErrorKind::Denied
    } else if matches!(error, SdkError::ConstructionFailure(_)) {
        ApiErrorKind::Other
    } else {
        ApiErrorKind::Transient
    };
    ApiError {
        kind,
        status,
        code,
        message,
    }
}

fn length(value: Option<i64>) -> u64 {
    value.and_then(|value| u64::try_from(value).ok()).unwrap_or(0)
}

fn etag(value: Option<&str>) -> ApiResult<String> {
    match value {
        Some(tag) if !tag.is_empty() => Ok(tag.to_owned()),
        _ => Err(ApiError::new(
            ApiErrorKind::Other,
            "the service did not return an ETag, which VaultSnap needs to tell versions apart",
        )),
    }
}

fn system_time(value: Option<&DateTime>) -> SystemTime {
    value
        .and_then(|time| {
            let seconds = u64::try_from(time.secs()).ok()?;
            Some(UNIX_EPOCH + Duration::new(seconds, time.subsec_nanos()))
        })
        .unwrap_or(UNIX_EPOCH)
}

impl ObjectApi for SdkApi {
    fn head_bucket(&self) -> ApiResult<()> {
        let (client, bucket) = (self.client.clone(), self.bucket.clone());
        self.call(async move {
            client
                .head_bucket()
                .bucket(bucket)
                .send()
                .await
                .map(|_| ())
                .map_err(|err| convert(&err))
        })
    }

    fn head(&self, key: &str) -> ApiResult<HeadInfo> {
        let (client, bucket, key) = (self.client.clone(), self.bucket.clone(), key.to_owned());
        self.call(async move {
            let output = client
                .head_object()
                .bucket(bucket)
                .key(key)
                .send()
                .await
                .map_err(|err| convert(&err))?;
            Ok(HeadInfo {
                len: length(output.content_length()),
                etag: etag(output.e_tag())?,
                modified: system_time(output.last_modified()),
            })
        })
    }

    fn get(&self, key: &str, range: Option<(u64, u64)>, max_len: u64) -> ApiResult<GetOutput> {
        let (client, bucket, key) = (self.client.clone(), self.bucket.clone(), key.to_owned());
        self.call(async move {
            let mut request = client.get_object().bucket(bucket).key(key);
            if let Some((first, last)) = range {
                request = request.range(format!("bytes={first}-{last}"));
            }
            let output = request.send().await.map_err(|err| convert(&err))?;
            let content_length = length(output.content_length());
            if range.is_none() && content_length > max_len {
                return Err(ApiError::new(
                    ApiErrorKind::TooLarge,
                    format!(
                        "the object is {content_length} bytes, more than the allowed {max_len}"
                    ),
                ));
            }
            let total_len = output
                .content_range()
                .and_then(|range| range.rsplit('/').next())
                .and_then(|total| total.parse::<u64>().ok())
                .unwrap_or(content_length);
            let tag = etag(output.e_tag())?;
            let modified = system_time(output.last_modified());
            let data = output
                .body
                .collect()
                .await
                .map_err(|err| {
                    ApiError::new(
                        ApiErrorKind::Transient,
                        redact(&format!(
                            "reading the response failed: {}",
                            DisplayErrorContext(&err)
                        )),
                    )
                })?
                .into_bytes()
                .to_vec();
            Ok(GetOutput {
                data,
                etag: tag,
                modified,
                total_len,
            })
        })
    }

    fn put(&self, key: &str, data: &[u8], condition: &Condition) -> ApiResult<String> {
        let (client, bucket, key) = (self.client.clone(), self.bucket.clone(), key.to_owned());
        let body = ByteStream::from(data.to_vec());
        let condition = condition.clone();
        self.call(async move {
            let mut request = client.put_object().bucket(bucket).key(key).body(body);
            match condition {
                Condition::None => {}
                Condition::IfNoneMatchAny => request = request.if_none_match("*"),
                Condition::IfMatch(tag) => request = request.if_match(tag),
            }
            let output = request.send().await.map_err(|err| convert(&err))?;
            etag(output.e_tag())
        })
    }

    fn delete(&self, key: &str) -> ApiResult<()> {
        let (client, bucket, key) = (self.client.clone(), self.bucket.clone(), key.to_owned());
        self.call(async move {
            client
                .delete_object()
                .bucket(bucket)
                .key(key)
                .send()
                .await
                .map(|_| ())
                .map_err(|err| convert(&err))
        })
    }

    fn list(&self, prefix: &str, delimiter: bool, token: Option<&str>) -> ApiResult<ListPage> {
        let (client, bucket) = (self.client.clone(), self.bucket.clone());
        let (prefix, token) = (prefix.to_owned(), token.map(str::to_owned));
        self.call(async move {
            let mut request = client.list_objects_v2().bucket(bucket).prefix(prefix);
            if delimiter {
                request = request.delimiter("/");
            }
            if let Some(token) = token {
                request = request.continuation_token(token);
            }
            let output = request.send().await.map_err(|err| convert(&err))?;
            let objects = output
                .contents()
                .iter()
                .filter_map(|object| {
                    Some(ListedObject {
                        key: object.key()?.to_owned(),
                        len: length(object.size()),
                    })
                })
                .collect();
            let prefixes = output
                .common_prefixes()
                .iter()
                .filter_map(|group| group.prefix().map(str::to_owned))
                .collect();
            let next_token = if output.is_truncated() == Some(false) {
                None
            } else {
                output.next_continuation_token().map(str::to_owned)
            };
            Ok(ListPage {
                objects,
                prefixes,
                next_token,
            })
        })
    }
}

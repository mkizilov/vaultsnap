//! S3-compatible remote repositories: a [`Backend`] that keeps a repository in a bucket.
//!
//! ```text
//! vaultsnap --repo s3://my-bucket/laptop/backups backup ~/Documents
//! ```
//!
//! [`S3Backend`] turns VaultSnap's storage model into S3 requests: an object name becomes the key
//! `<prefix>/<name>`, a directory listing becomes `ListObjectsV2` with a `/` delimiter,
//! [`PutMode::Create`] is a `PUT` with `If-None-Match: *`, [`PutMode::Update`] a `PUT` with
//! `If-Match`, a read of a prefix is a ranged `GET`. It speaks to the service through the narrow
//! [`ObjectApi`] trait; the AWS SDK adapter (feature `s3`) is the one implementation that touches
//! the network, and [`FakeS3`] is an in-memory one for tests.
//!
//! What this layer adds to the interface of [`crate::storage`]:
//!
//! * **Encryption stays above it.** The backend is only ever handed sealed bytes (chunk objects,
//!   snapshot files, key files); there is no code path that uploads anything else.
//! * **Bounded resources.** At most [`S3Config::max_concurrent_requests`] calls are in flight, so a
//!   large `--jobs` cannot open hundreds of connections, and memory is bounded by that many
//!   objects.
//! * **Retries.** [`S3Backend::open`] returns the backend wrapped in a [`RetryingBackend`]:
//!   exponential backoff with jitter for transient failures, and writes whose answer was lost are
//!   recognized instead of being reported as conflicts. The AWS SDK's own retries are switched off
//!   so that the two layers do not multiply, and so that this one sees every attempt.
//! * **Conditional writes are checked, not assumed.** Some S3-compatible stores ignore
//!   `If-None-Match` and overwrite. On connecting, the backend creates an object twice and tries a
//!   stale `If-Match`, and declares in its [`Capabilities`] only what it saw work; a store that
//!   fails the first is refused by the repository.
//! * **Credentials are never handled here.** They come from the AWS credential chain (environment,
//!   shared config and credentials files, SSO, web identity, container and instance roles), never
//!   from VaultSnap's options, and nothing in this module formats, stores, or logs them.

mod api;
mod fake;
#[cfg(feature = "s3")]
mod sdk;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

pub use api::{
    ApiError, ApiErrorKind, ApiResult, Condition, GetOutput, HeadInfo, ListPage, ListedObject,
    ObjectApi,
};
pub use fake::FakeS3;

use super::{
    validate_dir, validate_name, Backend, Capabilities, DirEntryInfo, DirEntryKind, Guarantee,
    LocationState, Object, ObjectInfo, PutMode, RetryPolicy, RetryingBackend, Sleeper, Version,
};
use crate::error::{BackendErrorKind, Error, Result};
use crate::util::random_hex;

/// The most requests that may be in flight at once, whatever is configured.
pub const MAX_REQUESTS_LIMIT: usize = 256;

/// How to reach a repository in an S3-compatible bucket.
///
/// There is deliberately no field for credentials: they come from the AWS credential chain.
#[derive(Clone)]
pub struct S3Config {
    /// The bucket. It must exist; VaultSnap does not create buckets.
    pub bucket: String,
    /// The folder inside the bucket that holds the repository: no leading or trailing `/`, empty
    /// for the bucket root. Several repositories can share a bucket under different prefixes.
    pub prefix: String,
    /// The service URL for S3-compatible stores such as MinIO. `None` means AWS (or whatever
    /// `AWS_ENDPOINT_URL_S3` / `AWS_ENDPOINT_URL` in the environment says, which the SDK reads).
    pub endpoint: Option<String>,
    /// The region. `None` means the region from the environment or the shared config, or
    /// `us-east-1`, which S3-compatible stores accept.
    pub region: Option<String>,
    /// The named profile of the shared AWS config and credentials files. `None` means the
    /// default, or `AWS_PROFILE`.
    pub profile: Option<String>,
    /// Address buckets in the path (`https://host/bucket/key`) rather than the host name
    /// (`https://bucket.host/key`). `None` means path style with a custom endpoint, and
    /// virtual-hosted style for AWS.
    pub path_style: Option<bool>,
    /// A PEM file of certificate authorities to trust instead of the platform's, for a store with
    /// a private CA. Certificates are always verified; there is no way to turn that off.
    pub ca_bundle: Option<PathBuf>,
    /// Accept an `http://` endpoint that is not on this machine. Without it plain HTTP is
    /// accepted only for `localhost`, `127.0.0.0/8` and `[::1]`.
    pub allow_insecure_http: bool,
    /// The most requests in flight at once (1 to [`MAX_REQUESTS_LIMIT`]).
    pub max_concurrent_requests: usize,
    /// How long to wait for a connection.
    pub connect_timeout: Duration,
    /// How long one request may take in all.
    pub request_timeout: Duration,
    /// Retries of transient failures.
    pub retry: RetryPolicy,
}

impl S3Config {
    /// A configuration for the root of `bucket` on AWS with the defaults.
    pub fn new(bucket: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            prefix: String::new(),
            endpoint: None,
            region: None,
            profile: None,
            path_style: None,
            ca_bundle: None,
            allow_insecure_http: false,
            max_concurrent_requests: 8,
            connect_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(300),
            retry: RetryPolicy::standard(),
        }
    }

    /// A configuration for `bucket/prefix`, the text after `s3://`.
    pub fn from_location(location: &str) -> Result<Self> {
        let (bucket, prefix) = parse_location(location)?;
        Ok(Self {
            prefix,
            ..Self::new(bucket)
        })
    }

    /// Checks everything that can be checked without a network.
    pub fn validate(&self) -> Result<()> {
        validate_bucket(&self.bucket)?;
        validate_prefix(&self.prefix)?;
        if let Some(endpoint) = &self.endpoint {
            validate_endpoint(endpoint, self.allow_insecure_http)?;
        }
        if self.max_concurrent_requests == 0 || self.max_concurrent_requests > MAX_REQUESTS_LIMIT {
            return Err(invalid(format!(
                "the number of concurrent requests must be from 1 to {MAX_REQUESTS_LIMIT}"
            )));
        }
        if self.connect_timeout.is_zero() || self.request_timeout.is_zero() {
            return Err(invalid("timeouts must not be zero".to_owned()));
        }
        Ok(())
    }
}

impl fmt::Debug for S3Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Config")
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .field("endpoint", &self.endpoint.as_deref().map(redact_endpoint))
            .field("region", &self.region)
            .field("profile", &self.profile)
            .field("path_style", &self.path_style)
            .field("ca_bundle", &self.ca_bundle)
            .field("allow_insecure_http", &self.allow_insecure_http)
            .field("max_concurrent_requests", &self.max_concurrent_requests)
            .finish_non_exhaustive()
    }
}

fn invalid(reason: String) -> Error {
    Error::InvalidPath(format!("invalid S3 repository: {reason}"))
}

/// Splits `bucket/prefix` (the text after `s3://`) into the bucket and the prefix without
/// leading or trailing slashes.
pub fn parse_location(location: &str) -> Result<(String, String)> {
    let trimmed = location.trim_matches('/');
    let (bucket, prefix) = trimmed.split_once('/').unwrap_or((trimmed, ""));
    validate_bucket(bucket)?;
    let prefix = prefix.trim_matches('/').to_owned();
    validate_prefix(&prefix)?;
    Ok((bucket.to_owned(), prefix))
}

/// S3 bucket names: 3 to 63 characters of lowercase letters, digits, hyphens, and dots, starting
/// and ending with a letter or digit.
fn validate_bucket(bucket: &str) -> Result<()> {
    let edge_ok = |c: Option<char>| c.is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let valid = (3..=63).contains(&bucket.len())
        && bucket
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
        && edge_ok(bucket.chars().next())
        && edge_ok(bucket.chars().next_back());
    if valid {
        Ok(())
    } else {
        Err(invalid(format!(
            "the bucket name {bucket:?} must be 3 to 63 characters of lowercase letters, digits, \
             hyphens and dots, starting and ending with a letter or digit"
        )))
    }
}

/// A prefix is `/`-separated segments of `[A-Za-z0-9._-]`, none empty, none `.` or `..`.
fn validate_prefix(prefix: &str) -> Result<()> {
    if prefix.is_empty() {
        return Ok(());
    }
    let valid = prefix.split('/').all(|segment| {
        !segment.is_empty()
            && segment != "."
            && segment != ".."
            && segment.len() <= 255
            && segment
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    });
    if valid {
        Ok(())
    } else {
        Err(invalid(format!(
            "the folder {prefix:?} must be `/`-separated names of letters, digits, `.`, `_` and `-`"
        )))
    }
}

/// The scheme, whether it is `https`, and the host of an endpoint URL, or why it is unusable.
fn endpoint_parts(endpoint: &str) -> Result<(bool, String)> {
    let Some((scheme, rest)) = endpoint.split_once("://") else {
        return Err(invalid("the endpoint must be a URL such as https://s3.example.com".to_owned()));
    };
    let secure = match scheme.to_ascii_lowercase().as_str() {
        "https" => true,
        "http" => false,
        _ => return Err(invalid("the endpoint must start with https:// or http://".to_owned())),
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return Err(invalid(
            "the endpoint must not contain a user name or password; credentials come from the \
             AWS credential chain"
                .to_owned(),
        ));
    }
    let host = if authority.starts_with('[') {
        authority.split_inclusive(']').next().unwrap_or("")
    } else {
        authority.split(':').next().unwrap_or("")
    };
    if host.is_empty() {
        return Err(invalid("the endpoint has no host".to_owned()));
    }
    Ok((secure, host.to_ascii_lowercase()))
}

fn is_loopback_host(host: &str) -> bool {
    host == "localhost"
        || host == "[::1]"
        || host
            .parse::<std::net::Ipv4Addr>()
            .is_ok_and(|address| address.is_loopback())
}

/// Accepts `https://` endpoints, and `http://` ones only on this machine unless the caller has
/// said that plain HTTP is acceptable. Credentials in the URL are always refused.
pub fn validate_endpoint(endpoint: &str, allow_insecure_http: bool) -> Result<()> {
    let (secure, host) = endpoint_parts(endpoint)?;
    if secure || allow_insecure_http || is_loopback_host(&host) {
        Ok(())
    } else {
        Err(invalid(format!(
            "the endpoint {} uses plain HTTP, which exposes object names, sizes, and timing to \
             the network and lets it withhold data; use https://, or allow it explicitly",
            redact_endpoint(endpoint)
        )))
    }
}

/// The endpoint with anything between `://` and `@` removed, safe to show.
pub fn redact_endpoint(endpoint: &str) -> String {
    let Some((scheme, rest)) = endpoint.split_once("://") else {
        return "<invalid endpoint>".to_owned();
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let host = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    format!("{scheme}://{host}{tail}")
}

/// Makes a message from a service or a network safe to show: very long text is cut, and anything
/// that looks like an AWS access key id is replaced. Credentials are never put into messages in
/// the first place; this is the second line of defense for text we did not write.
pub fn redact(message: &str) -> String {
    const LIMIT: usize = 600;
    const KEY_PREFIXES: [&str; 6] = ["AKIA", "ASIA", "AIDA", "AROA", "AGPA", "ANPA"];
    let mut out = String::with_capacity(message.len().min(LIMIT + 3));
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        let key_like = word.len() == 20
            && KEY_PREFIXES.iter().any(|prefix| word.starts_with(prefix))
            && word.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit());
        out.push_str(if key_like { "<access-key-id>" } else { word.as_str() });
        word.clear();
    };
    let mut cut = false;
    for c in message.chars() {
        if out.len() >= LIMIT {
            cut = true;
            break;
        }
        if c.is_ascii_alphanumeric() {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push(if c.is_control() { ' ' } else { c });
        }
    }
    flush(&mut word, &mut out);
    if cut || out.len() > LIMIT {
        let mut end = LIMIT.min(out.len());
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
        out.push_str("...");
    }
    out
}

/// Counts requests in flight and makes callers wait for a free slot.
struct Limiter {
    free: Mutex<usize>,
    available: Condvar,
}

struct Permit<'a>(&'a Limiter);

impl Limiter {
    fn new(slots: usize) -> Self {
        Self {
            free: Mutex::new(slots),
            available: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, usize> {
        self.free.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn acquire(&self) -> Permit<'_> {
        let mut free = self.lock();
        while *free == 0 {
            free = self
                .available
                .wait(free)
                .unwrap_or_else(PoisonError::into_inner);
        }
        *free -= 1;
        Permit(self)
    }
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        *self.0.lock() += 1;
        self.0.available.notify_one();
    }
}

/// A repository in an S3 bucket. See the module documentation.
pub struct S3Backend {
    api: Arc<dyn ObjectApi>,
    bucket: String,
    /// Empty, or the folder with a trailing `/`.
    prefix: String,
    /// `s3://bucket/folder`, for messages.
    root: PathBuf,
    capabilities: Mutex<Capabilities>,
    limiter: Limiter,
}

impl fmt::Debug for S3Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Backend")
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

impl S3Backend {
    /// A backend over `api` that has not yet checked what the service really guarantees: it
    /// declares the guarantees of S3 itself. Use [`S3Backend::open`] to get a verified one.
    pub fn with_api(api: Arc<dyn ObjectApi>, config: &S3Config) -> Result<Self> {
        config.validate()?;
        let prefix = if config.prefix.is_empty() {
            String::new()
        } else {
            format!("{}/", config.prefix)
        };
        Ok(Self {
            api,
            bucket: config.bucket.clone(),
            root: PathBuf::from(format!("s3://{}/{}", config.bucket, config.prefix)),
            prefix,
            capabilities: Mutex::new(Capabilities {
                read_after_write: true,
                create_if_absent: Guarantee::Global,
                replace_if_unchanged: Guarantee::Global,
                atomic_put: true,
                directories: false,
            }),
            limiter: Limiter::new(config.max_concurrent_requests),
        })
    }

    /// Connects through `api`, checks that the bucket can be reached, verifies that the service
    /// really honors conditional writes, and returns the backend with retries around it. This
    /// does I/O and blocks; every transient failure on the way is retried according to
    /// `config.retry`, waiting with `sleeper`.
    pub fn open(
        api: Arc<dyn ObjectApi>,
        config: &S3Config,
        sleeper: Arc<dyn Sleeper>,
    ) -> Result<Arc<dyn Backend>> {
        let s3 = Arc::new(Self::with_api(api, config)?);
        let retrying = RetryingBackend::with_sleeper(s3.clone(), config.retry.clone(), sleeper);
        retrying.probe()?;
        let (create, replace) = verify_conditional_writes(&retrying)?;
        if create != Guarantee::Global {
            tracing::warn!(
                bucket = %config.bucket,
                "the service ignores If-None-Match; it cannot hold a repository"
            );
        }
        let mut capabilities = s3.lock_capabilities();
        capabilities.create_if_absent = create;
        capabilities.replace_if_unchanged = replace;
        drop(capabilities);
        Ok(Arc::new(retrying))
    }

    /// Connects to the service described by `config` with the AWS SDK and returns the verified
    /// backend, wrapped in retries. Blocks, and starts threads of its own for the SDK: call it
    /// from a blocking context (not directly from async code).
    #[cfg(feature = "s3")]
    pub fn connect(config: S3Config) -> Result<Arc<dyn Backend>> {
        config.validate()?;
        let api = sdk::SdkApi::new(&config)?;
        Self::open(Arc::new(api), &config, Arc::new(super::ThreadSleeper))
    }

    /// Without the `s3` feature there is no client to connect with.
    #[cfg(not(feature = "s3"))]
    pub fn connect(config: S3Config) -> Result<Arc<dyn Backend>> {
        config.validate()?;
        Err(Error::UnsupportedBackend(
            "this build of VaultSnap was compiled without S3 support (the `s3` feature)".to_owned(),
        ))
    }

    fn lock_capabilities(&self) -> MutexGuard<'_, Capabilities> {
        self.capabilities
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn key(&self, name: &str) -> String {
        format!("{}{name}", self.prefix)
    }

    /// Runs one call to the service within the limit on concurrent requests.
    fn api<T>(&self, call: impl FnOnce(&dyn ObjectApi) -> ApiResult<T>) -> ApiResult<T> {
        let _permit = self.limiter.acquire();
        call(self.api.as_ref())
    }

    /// Turns a failed call into the error the repository sees.
    fn failure(&self, operation: &'static str, name: &str, error: &ApiError) -> Error {
        let text = redact(&error.to_string());
        match error.kind {
            ApiErrorKind::NotFound => {
                Error::backend(operation, name, BackendErrorKind::NotFound, "no such object")
            }
            ApiErrorKind::NoSuchBucket => Error::backend(
                operation,
                &self.bucket,
                BackendErrorKind::Other,
                "the bucket does not exist; create it first (VaultSnap does not create buckets)",
            ),
            ApiErrorKind::Transient => {
                Error::backend(operation, name, BackendErrorKind::Transient, text)
            }
            ApiErrorKind::Denied => Error::backend(
                operation,
                name,
                BackendErrorKind::Permission,
                format!(
                    "access was refused ({text}); check the AWS credentials, the profile or \
                     region, the system clock, and the bucket policy"
                ),
            ),
            ApiErrorKind::PreconditionFailed
            | ApiErrorKind::InvalidRange
            | ApiErrorKind::TooLarge
            | ApiErrorKind::Other => Error::backend(operation, name, BackendErrorKind::Other, text),
        }
    }
}

impl Backend for S3Backend {
    fn root(&self) -> &Path {
        &self.root
    }

    fn capabilities(&self) -> Capabilities {
        self.lock_capabilities().clone()
    }

    fn probe(&self) -> Result<LocationState> {
        self.api(|api| api.head_bucket())
            .map(|()| LocationState::Available)
            .map_err(|error| self.failure("probe", &self.bucket, &error))
    }

    fn create_root(&self) -> Result<()> {
        self.probe().map(|_| ())
    }

    fn create_dir(&self, dir: &str) -> Result<()> {
        validate_name(dir)
    }

    fn dir_exists(&self, dir: &str) -> Result<bool> {
        validate_name(dir)?;
        Ok(true)
    }

    fn exists(&self, name: &str) -> Result<bool> {
        validate_name(name)?;
        let key = self.key(name);
        match self.api(|api| api.head(&key)) {
            Ok(_) => Ok(true),
            Err(error) if error.kind == ApiErrorKind::NotFound => Ok(false),
            Err(error) => Err(self.failure("exists", name, &error)),
        }
    }

    fn stat(&self, name: &str) -> Result<ObjectInfo> {
        validate_name(name)?;
        let key = self.key(name);
        self.api(|api| api.head(&key))
            .map(|head| ObjectInfo {
                len: head.len,
                modified: head.modified,
                version: Version::new(head.etag),
            })
            .map_err(|error| self.failure("stat", name, &error))
    }

    fn get(&self, name: &str, max_len: u64) -> Result<Object> {
        validate_name(name)?;
        let key = self.key(name);
        match self.api(|api| api.get(&key, None, max_len)) {
            Ok(output) => {
                if output.data.len() as u64 != output.total_len {
                    return Err(Error::backend(
                        "get",
                        name,
                        BackendErrorKind::Transient,
                        "the response ended before the whole object arrived",
                    ));
                }
                Ok(Object {
                    info: ObjectInfo {
                        len: output.total_len,
                        modified: output.modified,
                        version: Version::new(output.etag),
                    },
                    data: output.data,
                })
            }
            Err(error) if error.kind == ApiErrorKind::TooLarge => Err(Error::TooLarge {
                path: self.root.join(name),
                max_len,
            }),
            Err(error) => Err(self.failure("get", name, &error)),
        }
    }

    fn get_prefix(&self, name: &str, max: usize) -> Result<(Vec<u8>, u64)> {
        validate_name(name)?;
        let key = self.key(name);
        if max == 0 {
            return self
                .api(|api| api.head(&key))
                .map(|head| (Vec::new(), head.len))
                .map_err(|error| self.failure("get_prefix", name, &error));
        }
        let last = u64::try_from(max - 1).unwrap_or(u64::MAX);
        match self.api(|api| api.get(&key, Some((0, last)), u64::MAX)) {
            Ok(mut output) => {
                output.data.truncate(max);
                Ok((output.data, output.total_len))
            }
            // An empty object answers every range with 416.
            Err(error) if error.kind == ApiErrorKind::InvalidRange => Ok((Vec::new(), 0)),
            Err(error) => Err(self.failure("get_prefix", name, &error)),
        }
    }

    fn list_dir(&self, dir: &str) -> Result<Vec<DirEntryInfo>> {
        validate_dir(dir)?;
        let prefix = if dir.is_empty() {
            self.prefix.clone()
        } else {
            format!("{}{dir}/", self.prefix)
        };
        let mut entries = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let page = self
                .api(|api| api.list(&prefix, true, token.as_deref()))
                .map_err(|error| self.failure("list_dir", dir, &error))?;
            for object in page.objects {
                let Some(name) = object.key.strip_prefix(prefix.as_str()) else {
                    continue;
                };
                // An empty name is a "folder" object some tools create; it is not an entry.
                if !name.is_empty() && !name.contains('/') {
                    entries.push(DirEntryInfo {
                        name: name.to_owned(),
                        kind: DirEntryKind::File,
                        len: object.len,
                    });
                }
            }
            for group in page.prefixes {
                let Some(rest) = group.strip_prefix(prefix.as_str()) else {
                    continue;
                };
                let name = rest.trim_end_matches('/');
                if !name.is_empty() && !name.contains('/') {
                    entries.push(DirEntryInfo {
                        name: name.to_owned(),
                        kind: DirEntryKind::Directory,
                        len: 0,
                    });
                }
            }
            match page.next_token {
                Some(next) if token.as_deref() == Some(next.as_str()) => {
                    return Err(Error::backend(
                        "list_dir",
                        dir,
                        BackendErrorKind::Other,
                        "the service returned the same continuation token twice",
                    ));
                }
                Some(next) => token = Some(next),
                None => break,
            }
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    fn put(&self, name: &str, data: &[u8], mode: PutMode) -> Result<Version> {
        validate_name(name)?;
        let key = self.key(name);
        let condition = match &mode {
            PutMode::Create => Condition::IfNoneMatchAny,
            PutMode::Overwrite => Condition::None,
            PutMode::Update(version) => Condition::IfMatch(version.as_str().to_owned()),
        };
        let refused = |mode: &PutMode| {
            let path = self.root.join(name);
            match mode {
                PutMode::Update(_) => Error::PreconditionFailed { path },
                PutMode::Create | PutMode::Overwrite => Error::AlreadyExists { path },
            }
        };
        match self.api(|api| api.put(&key, data, &condition)) {
            Ok(etag) => Ok(Version::new(etag)),
            Err(error) if error.kind == ApiErrorKind::PreconditionFailed => Err(refused(&mode)),
            // S3 answers If-Match on a key that does not exist with 404.
            Err(error)
                if error.kind == ApiErrorKind::NotFound && matches!(mode, PutMode::Update(_)) =>
            {
                Err(refused(&mode))
            }
            Err(error) => Err(self.failure("put", name, &error)),
        }
    }

    fn delete(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        let key = self.key(name);
        match self.api(|api| api.delete(&key)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind == ApiErrorKind::NotFound => Ok(()),
            Err(error) => Err(self.failure("delete", name, &error)),
        }
    }

    fn sync_dir(&self, dir: &str) -> Result<()> {
        // A request that has been answered has been stored durably; there is nothing to flush.
        validate_dir(dir)
    }
}

/// Finds out whether the service honors `If-None-Match` and `If-Match`, by trying them on an
/// object of its own that is removed again. Returns the guarantees for `Create` and `Update`.
///
/// Credentials that cannot write cannot break exclusivity either, so if the first write is
/// refused as forbidden the guarantees of S3 are assumed (a read-only user can still list,
/// check, and, where the locks allow it, restore).
fn verify_conditional_writes(backend: &dyn Backend) -> Result<(Guarantee, Guarantee)> {
    let name = format!("vaultsnap-probe-{}", random_hex(8)?);
    let outcome = probe_conditions(backend, &name);
    match backend.delete(&name) {
        Ok(()) => {}
        Err(err) if err.is_not_found() => {}
        Err(err) => {
            tracing::warn!(error = %err, "failed to remove the probe object {name}");
        }
    }
    outcome
}

fn probe_conditions(backend: &dyn Backend, name: &str) -> Result<(Guarantee, Guarantee)> {
    match backend.put(name, b"first", PutMode::Create) {
        Ok(_) => {}
        Err(Error::Backend {
            kind: BackendErrorKind::Permission,
            ..
        }) => return Ok((Guarantee::Global, Guarantee::Global)),
        Err(err) => return Err(err),
    }
    let create = match backend.put(name, b"second", PutMode::Create) {
        Err(Error::AlreadyExists { .. }) => Guarantee::Global,
        Ok(_) => Guarantee::None,
        Err(err) => return Err(err),
    };

    let stale = Version::new("\"00000000000000000000000000000000\"");
    let refused_stale = match backend.put(name, b"third", PutMode::Update(stale)) {
        Err(Error::PreconditionFailed { .. }) => true,
        Err(err) if err.is_transient() => return Err(err),
        Ok(_) | Err(_) => false,
    };
    let current = backend.stat(name)?.version;
    let accepted_current = backend.put(name, b"fourth", PutMode::Update(current)).is_ok();
    let replace = if refused_stale && accepted_current {
        Guarantee::Global
    } else {
        Guarantee::None
    };
    Ok((create, replace))
}

#[cfg(test)]
mod tests;

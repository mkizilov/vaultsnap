//! The narrow view of an S3-compatible service that [`S3Backend`](super::S3Backend) is written
//! against: six calls, plain data in and out, and errors reduced to what the backend needs to
//! decide (does it not exist, was a condition refused, may it be retried, is it forbidden).
//!
//! Keeping the service behind this trait has two purposes. The logic that turns VaultSnap's
//! storage model into S3 requests (key mapping, listings with a delimiter, ranged reads,
//! conditional writes, error classification, bounded concurrency, the probe for conditional-write
//! support) is plain Rust that can be tested without a network, against [`FakeS3`](super::FakeS3),
//! which implements this trait in memory with S3's behavior. And everything that depends on the
//! AWS SDK, whose API is large and moves, is confined to one small adapter (`sdk.rs`).

use std::fmt;
use std::time::SystemTime;

/// What kind of failure an S3 call ended in, as far as the backend cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiErrorKind {
    /// The key does not exist (`404 NoSuchKey`, or a plain `404` to a `HEAD`).
    NotFound,
    /// The bucket does not exist (`404 NoSuchBucket`).
    NoSuchBucket,
    /// A condition (`If-None-Match`, `If-Match`) was not met: `412 PreconditionFailed`.
    PreconditionFailed,
    /// A ranged read started beyond the end of the object: `416 InvalidRange`. This is what an
    /// empty object answers to any range.
    InvalidRange,
    /// The object is longer than the caller allowed; nothing of its body was read.
    TooLarge,
    /// The call may succeed if repeated: no connection, a timeout, `5xx`, `SlowDown`, a
    /// concurrent conditional write (`409 ConditionalRequestConflict`).
    Transient,
    /// The service refused the caller: bad or missing credentials, no permission, a request
    /// signed with the wrong clock. Repeating the call will not help.
    Denied,
    /// Anything else. Not worth repeating.
    Other,
}

/// A failed S3 call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    /// How to treat the failure.
    pub kind: ApiErrorKind,
    /// The HTTP status, if there was a response.
    pub status: Option<u16>,
    /// The S3 error code (`NoSuchKey`, `SlowDown`, ...), if the response carried one.
    pub code: Option<String>,
    /// A description. It is shown to people, so it must never contain credentials.
    pub message: String,
}

impl ApiError {
    /// An error of `kind` with only a message.
    pub fn new(kind: ApiErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            status: None,
            code: None,
            message: message.into(),
        }
    }

    /// An error of `kind` as an S3 response with `status` and `code` would report it.
    pub fn response(
        kind: ApiErrorKind,
        status: u16,
        code: &str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            status: Some(status),
            code: Some(code.to_owned()),
            message: message.into(),
        }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.code, self.status) {
            (Some(code), Some(status)) => write!(f, "{code} (HTTP {status}): {}", self.message),
            (Some(code), None) => write!(f, "{code}: {}", self.message),
            (None, Some(status)) => write!(f, "HTTP {status}: {}", self.message),
            (None, None) => f.write_str(&self.message),
        }
    }
}

/// The result of an S3 call.
pub type ApiResult<T> = std::result::Result<T, ApiError>;

/// S3 error codes that mean the caller is not allowed, whatever the status says.
///
/// Only the SDK adapter calls [`classify_response`] and uses these tables, but they are compiled
/// and tested without the `s3` feature too, so that the logic cannot rot unnoticed.
#[cfg_attr(not(feature = "s3"), allow(dead_code))]
const DENIED_CODES: [&str; 11] = [
    "AccessDenied",
    "AllAccessDisabled",
    "AccountProblem",
    "AuthorizationHeaderMalformed",
    "ExpiredToken",
    "InvalidAccessKeyId",
    "InvalidSecurity",
    "InvalidToken",
    "RequestTimeTooSkewed",
    "SignatureDoesNotMatch",
    "UnauthorizedOperation",
];

/// S3 error codes that mean "try again", whatever the status says.
#[cfg_attr(not(feature = "s3"), allow(dead_code))]
const TRANSIENT_CODES: [&str; 9] = [
    "ConditionalRequestConflict",
    "InternalError",
    "OperationAborted",
    "RequestLimitExceeded",
    "RequestTimeout",
    "ServiceUnavailable",
    "SlowDown",
    "Throttling",
    "ThrottlingException",
];

/// Decides what an S3 error response means from its HTTP status and error code.
///
/// A concurrent conditional write (`409 ConditionalRequestConflict`) is transient: repeating it
/// gets the real answer, a `412` or a success.
#[cfg_attr(not(feature = "s3"), allow(dead_code))]
pub fn classify_response(status: Option<u16>, code: Option<&str>) -> ApiErrorKind {
    if code == Some("NoSuchBucket") {
        return ApiErrorKind::NoSuchBucket;
    }
    if code.is_some_and(|code| DENIED_CODES.contains(&code)) {
        return ApiErrorKind::Denied;
    }
    if code.is_some_and(|code| TRANSIENT_CODES.contains(&code)) {
        return ApiErrorKind::Transient;
    }
    match status {
        Some(404) => ApiErrorKind::NotFound,
        Some(412) => ApiErrorKind::PreconditionFailed,
        Some(416) => ApiErrorKind::InvalidRange,
        Some(401 | 403) => ApiErrorKind::Denied,
        Some(408 | 409 | 429) => ApiErrorKind::Transient,
        Some(status) if status >= 500 => ApiErrorKind::Transient,
        _ => ApiErrorKind::Other,
    }
}

/// What `HEAD` tells about an object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadInfo {
    /// `Content-Length`.
    pub len: u64,
    /// The `ETag` exactly as the service returned it, quotes included.
    pub etag: String,
    /// `Last-Modified`.
    pub modified: SystemTime,
}

/// What a `GET` returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GetOutput {
    /// The bytes requested: the whole object, or the range.
    pub data: Vec<u8>,
    /// The `ETag` of the object these bytes belong to, exactly as returned.
    pub etag: String,
    /// `Last-Modified`.
    pub modified: SystemTime,
    /// The size of the whole object (from `Content-Range` for a ranged read).
    pub total_len: u64,
}

/// The condition of a `PUT`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Condition {
    /// Unconditional.
    None,
    /// `If-None-Match: *`: only if the key does not exist.
    IfNoneMatchAny,
    /// `If-Match: <etag>`: only if the key exists with exactly this `ETag`.
    IfMatch(String),
}

/// One object in a listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedObject {
    /// The whole key.
    pub key: String,
    /// Its size.
    pub len: u64,
}

/// One page of a listing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ListPage {
    /// Objects directly below the prefix (or all below it, without a delimiter).
    pub objects: Vec<ListedObject>,
    /// With a delimiter: the keys' next segments, as full prefixes ending in the delimiter.
    pub prefixes: Vec<String>,
    /// Where the next page starts; `None` on the last page.
    pub next_token: Option<String>,
}

/// An S3-compatible service, bound to one bucket. Implementations are shared between threads and
/// block the caller until the call is done.
pub trait ObjectApi: Send + Sync + fmt::Debug {
    /// `HEAD` on the bucket: it exists and the caller may list it.
    fn head_bucket(&self) -> ApiResult<()>;

    /// `HEAD` on a key. A missing key is [`ApiErrorKind::NotFound`].
    fn head(&self, key: &str) -> ApiResult<HeadInfo>;

    /// `GET`. With `range` (first and last byte, inclusive) a ranged read. An object longer than
    /// `max_len` is [`ApiErrorKind::TooLarge`], decided from the response headers before the body
    /// is read. A range beyond the end of the object is [`ApiErrorKind::InvalidRange`].
    fn get(&self, key: &str, range: Option<(u64, u64)>, max_len: u64) -> ApiResult<GetOutput>;

    /// `PUT` of the whole object under `condition`; returns the new `ETag`. A refused condition
    /// is [`ApiErrorKind::PreconditionFailed`]. `If-Match` on a key that does not exist may be
    /// answered with [`ApiErrorKind::NotFound`] or with a refused condition.
    fn put(&self, key: &str, data: &[u8], condition: &Condition) -> ApiResult<String>;

    /// `DELETE`. Deleting a key that does not exist succeeds.
    fn delete(&self, key: &str) -> ApiResult<()>;

    /// One page of `ListObjectsV2` for `prefix`. With `delimiter` (always `/`) keys below a
    /// further `/` are rolled up into [`ListPage::prefixes`]. `token` continues a listing.
    fn list(&self, prefix: &str, delimiter: bool, token: Option<&str>) -> ApiResult<ListPage>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_are_classified_by_code_first_and_status_second() {
        use ApiErrorKind::*;
        let cases: [(Option<u16>, Option<&str>, ApiErrorKind); 22] = [
            (Some(404), Some("NoSuchKey"), NotFound),
            (Some(404), Some("NotFound"), NotFound),
            (Some(404), None, NotFound),
            (Some(404), Some("NoSuchBucket"), NoSuchBucket),
            (Some(412), Some("PreconditionFailed"), PreconditionFailed),
            (Some(416), Some("InvalidRange"), InvalidRange),
            (Some(409), Some("ConditionalRequestConflict"), Transient),
            (Some(409), None, Transient),
            (Some(503), Some("SlowDown"), Transient),
            (Some(500), Some("InternalError"), Transient),
            (Some(502), None, Transient),
            (Some(504), None, Transient),
            (Some(429), None, Transient),
            (Some(408), Some("RequestTimeout"), Transient),
            (Some(403), Some("AccessDenied"), Denied),
            (Some(403), Some("InvalidAccessKeyId"), Denied),
            (Some(403), Some("SignatureDoesNotMatch"), Denied),
            (Some(403), Some("RequestTimeTooSkewed"), Denied),
            (Some(400), Some("ExpiredToken"), Denied),
            (Some(401), None, Denied),
            (Some(400), Some("InvalidArgument"), Other),
            (None, None, Other),
        ];
        for (status, code, expected) in cases {
            assert_eq!(
                classify_response(status, code),
                expected,
                "{status:?} {code:?}"
            );
        }
    }

    #[test]
    fn a_throttling_code_wins_over_a_status_that_would_not_be_retried() {
        assert_eq!(
            classify_response(Some(400), Some("SlowDown")),
            ApiErrorKind::Transient
        );
        assert_eq!(
            classify_response(Some(200), Some("InternalError")),
            ApiErrorKind::Transient
        );
    }

    #[test]
    fn errors_describe_themselves_without_inventing_fields() {
        let full = ApiError::response(ApiErrorKind::Transient, 503, "SlowDown", "reduce the rate");
        assert_eq!(full.to_string(), "SlowDown (HTTP 503): reduce the rate");
        let bare = ApiError::new(ApiErrorKind::Other, "no route to host");
        assert_eq!(bare.to_string(), "no route to host");
        let status_only = ApiError {
            status: Some(502),
            ..bare.clone()
        };
        assert_eq!(status_only.to_string(), "HTTP 502: no route to host");
        let code_only = ApiError {
            code: Some("X".to_owned()),
            ..bare
        };
        assert_eq!(code_only.to_string(), "X: no route to host");
    }
}

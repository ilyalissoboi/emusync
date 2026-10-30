//! Storage backends for emusync (DESIGN §6.1, §6.2, §30).
//!
//! A [`Remote`] is a flat key/value object store with create-only writes. Keys are relative to
//! the repository root (`repo.json`, `blobs/ab/abcd…`, `devices/<id>/log/<seq>.ndjson`); each
//! backend maps them onto its own namespace (an S3 prefix, for example).
//!
//! The repository layout guarantees that no key is ever written by more than one device, so
//! backends only need create-only puts, gets, and ordered listing.

pub mod conformance;
mod credentials;
pub mod dropbox;
mod memory;
mod s3;

pub use credentials::{
    CallbackCredentials, CredentialProvider, Credentials, EnvCredentials, ExpiringCredentials, S3Credentials,
    StaticCredentials,
};
pub use dropbox::{DropboxConfig, DropboxRemote};
pub use memory::{MemoryRemote, MemoryRemoteOptions};
pub use s3::{S3Config, S3Remote};

#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    /// The remote could not be reached (DNS, TCP, TLS, timeouts, 5xx). Retry later.
    #[error("remote unreachable: {0}")]
    Network(String),
    /// Credentials are missing, invalid, or not allowed to do this.
    #[error("authentication failed: {0}")]
    Auth(String),
    /// The backend doesn't implement something we asked for (e.g. conditional writes).
    #[error("not supported by remote: {0}")]
    Unsupported(String),
    #[error("invalid remote uri: {0}")]
    InvalidUri(String),
    #[error("remote returned HTTP {status}: {message}")]
    Http { status: u16, message: String },
    #[error("{0}")]
    Other(String),
}

impl RemoteError {
    /// True if the failure means "offline, try again later" rather than a hard error.
    pub fn is_offline(&self) -> bool {
        matches!(self, RemoteError::Network(_))
    }
}

pub type Result<T> = std::result::Result<T, RemoteError>;

/// Result of a put.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutOutcome {
    Created,
    /// The key already existed and was left untouched.
    AlreadyExists,
}

pub trait Remote: Send + Sync {
    /// Human-readable identity of the remote, such as its URI.
    fn describe(&self) -> String;

    /// Writes `key`. With `if_none_match`, the write is create-only on the server
    /// (`If-None-Match: *`) and reports [`PutOutcome::AlreadyExists`] instead of overwriting.
    /// Backends without conditional-write support may ignore the flag and overwrite; use
    /// [`probe_conditional_writes`] and [`create_only`] rather than calling this directly.
    fn put(&self, key: &str, data: &[u8], if_none_match: bool) -> Result<PutOutcome>;

    /// Reads `key`. `Ok(None)` if it doesn't exist (or isn't visible yet).
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>>;

    /// Whether `key` exists.
    fn head(&self, key: &str) -> Result<bool>;

    /// All keys under `prefix` that sort after `start_after`, in lexical order.
    fn list(&self, prefix: &str, start_after: Option<&str>) -> Result<Vec<String>>;

    /// Names of the immediate "subdirectories" of `prefix` (which should end in `/`), without
    /// the prefix or a trailing slash. For `devices/` this returns the device ids.
    fn list_dirs(&self, prefix: &str) -> Result<Vec<String>>;
}

/// Create-only write that works whether or not the backend supports conditional writes
/// (DESIGN §6.2). Without support it falls back to HEAD-before-PUT, which is only racy if two
/// installations share a device id.
pub fn create_only(remote: &dyn Remote, key: &str, data: &[u8], conditional_writes: bool) -> Result<PutOutcome> {
    if conditional_writes {
        remote.put(key, data, true)
    } else if remote.head(key)? {
        Ok(PutOutcome::AlreadyExists)
    } else {
        remote.put(key, data, false)
    }
}

/// Probes whether the backend enforces `If-None-Match: *` by writing the same new key twice.
/// Leaves one small object under `_probe/`.
pub fn probe_conditional_writes(remote: &dyn Remote) -> Result<bool> {
    let key = format!("_probe/{}", uuid::Uuid::now_v7());
    match remote.put(&key, b"probe", true) {
        Ok(_) => {}
        Err(RemoteError::Unsupported(_)) => return Ok(false),
        Err(e) => return Err(e),
    }
    match remote.put(&key, b"probe", true) {
        Ok(PutOutcome::AlreadyExists) => Ok(true),
        Ok(PutOutcome::Created) | Err(RemoteError::Unsupported(_)) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Builds a remote from a URI: `s3://…` or `dropbox:/…`.
pub fn open(uri: &str, creds: Box<dyn CredentialProvider>) -> Result<Box<dyn Remote>> {
    if uri.starts_with("s3://") {
        Ok(Box::new(S3Remote::new(S3Config::parse(uri)?, creds)?))
    } else if uri.starts_with("dropbox:") {
        Ok(Box::new(DropboxRemote::new(DropboxConfig::parse(uri)?, creds)?))
    } else {
        Err(RemoteError::InvalidUri(format!("unsupported scheme in {uri:?} (supported: s3://, dropbox:)")))
    }
}

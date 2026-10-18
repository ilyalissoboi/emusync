use emusync_formats::FormatError;
use emusync_remote::RemoteError;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Remote(#[from] RemoteError),
    #[error("local database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("container format: {0}")]
    Format(#[from] FormatError),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid argument: {0}")]
    InvalidArg(String),
    #[error("integrity check failed: {0}")]
    Integrity(String),
    #[error("no remote configured")]
    NoRemote,
}

impl Error {
    /// True when the remote is unreachable and the operation can be retried later.
    pub fn is_offline(&self) -> bool {
        matches!(self, Error::Remote(e) if e.is_offline())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

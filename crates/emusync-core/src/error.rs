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
    /// The repository is encrypted and this device doesn't have its key yet: it must be
    /// unlocked with the passphrase ([`crate::Engine::unlock`]).
    #[error("the repository is encrypted and locked on this device: unlock it with the passphrase")]
    Locked,
    #[error("wrong passphrase")]
    WrongPassphrase,
    /// The repository needs something this version doesn't have (a newer format, an unknown
    /// encryption scheme), or doesn't match what was asked for (encryption on an existing
    /// unencrypted repository).
    #[error("incompatible repository: {0}")]
    Incompatible(String),
}

impl Error {
    /// True when the remote is unreachable and the operation can be retried later.
    pub fn is_offline(&self) -> bool {
        matches!(self, Error::Remote(e) if e.is_offline())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

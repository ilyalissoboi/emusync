//! Credentials for every backend (DESIGN §6.2, §30.5): S3 keys, or an OAuth token.
//!
//! A [`CredentialProvider`] is asked before each request, so it can refresh temporary
//! credentials; each backend refuses the kinds it can't use.

use crate::{RemoteError, Result};
use std::time::SystemTime;

#[derive(Clone)]
pub struct S3Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

impl std::fmt::Debug for S3Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Credentials").field("access_key_id", &self.access_key_id).finish_non_exhaustive()
    }
}

/// What a provider supplies.
#[derive(Clone)]
pub enum Credentials {
    /// S3 access keys, optionally STS temporary credentials.
    S3(S3Credentials),
    /// An OAuth access token the host refreshes itself.
    Bearer(String),
    /// An OAuth refresh token: the backend exchanges it for access tokens, kept in memory only.
    OAuthRefresh(String),
}

impl Credentials {
    /// The kind, for error messages (never the secret).
    pub fn kind(&self) -> &'static str {
        match self {
            Credentials::S3(_) => "S3 keys",
            Credentials::Bearer(_) => "an OAuth access token",
            Credentials::OAuthRefresh(_) => "an OAuth refresh token",
        }
    }
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Credentials::S3(c) => c.fmt(f),
            other => f.write_str(other.kind()),
        }
    }
}

impl From<S3Credentials> for Credentials {
    fn from(c: S3Credentials) -> Self {
        Credentials::S3(c)
    }
}

/// Supplies credentials on demand. Called before each request, so a provider can refresh
/// temporary credentials. Implementations must not persist secrets in the data directory.
pub trait CredentialProvider: Send + Sync {
    fn credentials(&self) -> Result<Credentials>;

    /// The remote rejected the last credentials (HTTP 401/403). Returns true if the provider
    /// dropped them and may supply different ones, in which case the request is retried once.
    fn invalidate(&self) -> bool {
        false
    }
}

/// Credentials as a host supplies them: static ones (`expires_unix_ms: None`), or temporary
/// ones (STS credentials, an access token) with their expiry.
#[derive(Clone)]
pub struct ExpiringCredentials {
    pub creds: Credentials,
    pub expires_unix_ms: Option<i64>,
}

type FetchFn = dyn Fn() -> Result<ExpiringCredentials> + Send + Sync;

/// Credentials from a callback (the C ABI's `es_credential_provider`). The callback runs on
/// first use, again shortly before temporary credentials expire, and after the remote rejects
/// them; in between, the last answer is reused. Nothing is persisted.
pub struct CallbackCredentials {
    fetch: Box<FetchFn>,
    cache: std::sync::Mutex<Option<ExpiringCredentials>>,
}

impl CallbackCredentials {
    /// Temporary credentials are refreshed this long before they expire.
    pub const REFRESH_MARGIN_MS: i64 = 5 * 60 * 1000;

    pub fn new(fetch: impl Fn() -> Result<ExpiringCredentials> + Send + Sync + 'static) -> Self {
        Self { fetch: Box::new(fetch), cache: std::sync::Mutex::new(None) }
    }
}

pub(crate) fn now_unix_ms() -> i64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

impl CredentialProvider for CallbackCredentials {
    fn credentials(&self) -> Result<Credentials> {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let fresh = |c: &ExpiringCredentials| {
            c.expires_unix_ms.map_or(true, |exp| now_unix_ms() < exp - Self::REFRESH_MARGIN_MS)
        };
        if let Some(c) = cache.as_ref().filter(|c| fresh(c)) {
            return Ok(c.creds.clone());
        }
        let c = (self.fetch)()?;
        let creds = c.creds.clone();
        *cache = Some(c);
        Ok(creds)
    }

    fn invalidate(&self) -> bool {
        self.cache.lock().unwrap_or_else(|e| e.into_inner()).take();
        true
    }
}

pub struct StaticCredentials(pub Credentials);

impl CredentialProvider for StaticCredentials {
    fn credentials(&self) -> Result<Credentials> {
        Ok(self.0.clone())
    }
}

/// Reads `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and optional `AWS_SESSION_TOKEN`.
pub struct EnvCredentials;

impl CredentialProvider for EnvCredentials {
    fn credentials(&self) -> Result<Credentials> {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        match (get("AWS_ACCESS_KEY_ID"), get("AWS_SECRET_ACCESS_KEY")) {
            (Some(access_key_id), Some(secret_access_key)) => Ok(Credentials::S3(S3Credentials {
                access_key_id,
                secret_access_key,
                session_token: get("AWS_SESSION_TOKEN"),
            })),
            _ => Err(RemoteError::Auth("AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY must be set".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn access_key(c: Credentials) -> String {
        match c {
            Credentials::S3(c) => c.access_key_id,
            other => panic!("expected S3 keys, got {other:?}"),
        }
    }

    #[test]
    fn callback_credentials_cache_and_refresh() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc;
        let calls = Arc::new(AtomicU32::new(0));
        let expires = Arc::new(std::sync::Mutex::new(None));
        let provider = {
            let (calls, expires) = (calls.clone(), expires.clone());
            CallbackCredentials::new(move || {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                Ok(ExpiringCredentials {
                    creds: Credentials::S3(S3Credentials {
                        access_key_id: format!("AK{n}"),
                        secret_access_key: "secret".into(),
                        session_token: None,
                    }),
                    expires_unix_ms: *expires.lock().unwrap(),
                })
            })
        };
        let key = || access_key(provider.credentials().unwrap());
        // Static keys are fetched once.
        assert_eq!(key(), "AK0");
        assert_eq!(key(), "AK0");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // A 403 drops them.
        assert!(provider.invalidate());
        assert_eq!(key(), "AK1");
        // Temporary credentials inside the refresh margin are fetched again on every use.
        *expires.lock().unwrap() = Some(now_unix_ms() + 60_000);
        provider.invalidate();
        assert_eq!(key(), "AK2");
        assert_eq!(key(), "AK3");
        // Ones that expire later are reused.
        *expires.lock().unwrap() = Some(now_unix_ms() + 3_600_000);
        provider.invalidate();
        assert_eq!(key(), "AK4");
        assert_eq!(key(), "AK4");
        // A failing callback is passed through.
        let failing = CallbackCredentials::new(|| Err(RemoteError::Auth("sign in".into())));
        assert!(matches!(failing.credentials(), Err(RemoteError::Auth(_))));
    }

    #[test]
    fn debug_hides_secrets() {
        let s3 = Credentials::S3(S3Credentials {
            access_key_id: "AKID".into(),
            secret_access_key: "hunter2".into(),
            session_token: Some("sts-secret".into()),
        });
        let bearer = Credentials::Bearer("bearer-secret".into());
        let refresh = Credentials::OAuthRefresh("refresh-secret".into());
        let shown = format!("{s3:?} {bearer:?} {refresh:?}");
        assert!(shown.contains("AKID"));
        assert!(!shown.contains("hunter2") && !shown.contains("secret"), "{shown}");
    }
}

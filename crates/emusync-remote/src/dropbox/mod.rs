//! Dropbox backend (DESIGN §30).
//!
//! The repository lives in a folder of the emusync app's App folder (`/Apps/emusync/` in the
//! user's Dropbox): `dropbox:/saves` is `/saves` inside it. Keys are paths under that folder.
//! Hand-written over `ureq` like the S3 backend: only `files/upload` (create-only with
//! `strict_conflict`), `files/download`, `files/get_metadata` and `files/list_folder` are used.
//! Access tokens come from [`oauth::TokenCache`], which exchanges the host's refresh token.

pub mod oauth;

#[cfg(any(test, feature = "fake-dropbox"))]
pub mod fake;

use crate::{CredentialProvider, PutOutcome, Remote, RemoteError, Result};
use oauth::TokenCache;
use std::io::Read;
use std::time::Duration;

/// The emusync Dropbox app's key, set at build time (`EMUSYNC_DROPBOX_APP_KEY`) once the app is
/// registered (DESIGN §30.1). Public: sign-in uses PKCE, so there is no client secret. A URI's
/// `app_key` parameter overrides it.
pub const BUILT_IN_APP_KEY: Option<&str> = option_env!("EMUSYNC_DROPBOX_APP_KEY");

/// Where requests go. Tests point these at a fake server; nothing else changes them, so tokens
/// only ever reach Dropbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropboxEndpoints {
    /// RPC calls and the token endpoint: `https://api.dropboxapi.com`.
    pub api: String,
    /// Uploads and downloads: `https://content.dropboxapi.com`.
    pub content: String,
    /// The sign-in page: `https://www.dropbox.com/oauth2/authorize`.
    pub authorize: String,
}

impl Default for DropboxEndpoints {
    fn default() -> Self {
        Self {
            api: "https://api.dropboxapi.com".into(),
            content: "https://content.dropboxapi.com".into(),
            authorize: "https://www.dropbox.com/oauth2/authorize".into(),
        }
    }
}

/// Parsed `dropbox:/<folder>[?app_key=…]` URI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropboxConfig {
    /// The repository folder inside the app folder, without leading or trailing slashes; empty
    /// for the app folder itself.
    pub folder: String,
    /// The `app_key` parameter, if the URI has one.
    pub app_key_param: Option<String>,
}

impl DropboxConfig {
    pub fn parse(uri: &str) -> Result<Self> {
        let rest = uri
            .strip_prefix("dropbox:")
            .ok_or_else(|| RemoteError::InvalidUri(format!("{uri:?} does not start with dropbox:")))?;
        let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
        let folder = path.trim_matches('/').to_string();
        if folder.split('/').any(|s| s.is_empty() || s == "." || s == "..") && !folder.is_empty() {
            return Err(RemoteError::InvalidUri(format!("{uri:?}: the folder has an empty, . or .. part")));
        }
        if folder.chars().any(|c| c.is_control() || c == '\\') {
            return Err(RemoteError::InvalidUri(format!("{uri:?}: the folder has a control character or \\")));
        }
        let mut app_key_param = None;
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            match k {
                "app_key" if !v.is_empty() && v.bytes().all(|b| b.is_ascii_alphanumeric()) => {
                    app_key_param = Some(v.to_string())
                }
                "app_key" => {
                    return Err(RemoteError::InvalidUri(format!("{uri:?}: app_key must be letters and digits")))
                }
                _ => return Err(RemoteError::InvalidUri(format!("unknown parameter {k:?} in {uri:?}"))),
            }
        }
        Ok(Self { folder, app_key_param })
    }

    /// The Dropbox app this repository belongs to: the URI's, else the built-in one.
    pub fn app_key(&self) -> Result<String> {
        self.app_key_param
            .clone()
            .or_else(|| BUILT_IN_APP_KEY.filter(|k| !k.is_empty()).map(str::to_string))
            .ok_or_else(|| {
                RemoteError::InvalidUri(
                    "this build has no Dropbox app key: add ?app_key=<your app's key> to the URI (DESIGN §30.1)".into(),
                )
            })
    }

    /// The canonical URI: what `describe()` returns and data directories are bound to.
    pub fn uri(&self) -> String {
        let mut s = format!("dropbox:/{}", self.folder);
        if let Some(k) = &self.app_key_param {
            s.push_str(&format!("?app_key={k}"));
        }
        s
    }

    /// The Dropbox path of the repository folder: `/saves`, or `""` for the app folder.
    fn root(&self) -> String {
        if self.folder.is_empty() {
            String::new()
        } else {
            format!("/{}", self.folder)
        }
    }

    /// The Dropbox path of `key`.
    fn path(&self, key: &str) -> Result<String> {
        // Dropbox paths ignore case. Every key the core writes is lower-case, so none fold
        // together; refuse any other, so a future key scheme can't collide silently.
        if key.is_empty()
            || key.starts_with('/')
            || key.ends_with('/')
            || key.contains("//")
            || key.bytes().any(|b| b.is_ascii_uppercase() || b.is_ascii_control() || b == b'\\')
        {
            return Err(RemoteError::Other(format!("{key:?} is not a valid Dropbox key (lower-case paths only)")));
        }
        Ok(format!("{}/{key}", self.root()))
    }

    /// The Dropbox path of the folder `dir` (a key prefix without its trailing slash).
    fn folder_path(&self, dir: &str) -> Result<String> {
        if dir.is_empty() {
            Ok(self.root())
        } else {
            self.path(dir)
        }
    }

    /// The key of a listed entry, from its `path_lower`.
    fn key_of(&self, path_lower: &str) -> Option<String> {
        let root = self.root().to_lowercase();
        path_lower.strip_prefix(&root)?.strip_prefix('/').map(str::to_string)
    }
}

/// An HTTP response, with what the retry logic needs.
struct Response {
    status: u16,
    body: Vec<u8>,
    retry_after: Option<u64>,
}

impl Response {
    /// Dropbox's `error_summary` (`path/not_found/..`), or the body as text.
    fn summary(&self) -> String {
        serde_json::from_slice::<serde_json::Value>(&self.body)
            .ok()
            .and_then(|v| v.get("error_summary").and_then(|s| s.as_str()).map(str::to_string))
            .unwrap_or_else(|| String::from_utf8_lossy(&self.body).chars().take(300).collect())
    }

    fn json(&self) -> Result<serde_json::Value> {
        serde_json::from_slice(&self.body).map_err(|e| RemoteError::Other(format!("Dropbox sent invalid JSON: {e}")))
    }
}

enum Body<'a> {
    /// An RPC call's JSON arguments.
    Json(String),
    /// A content call: arguments in `Dropbox-API-Arg`, then the bytes to upload (or none).
    Content { arg: String, data: &'a [u8] },
}

pub struct DropboxRemote {
    cfg: DropboxConfig,
    endpoints: DropboxEndpoints,
    tokens: TokenCache,
    agent: ureq::Agent,
    /// `limit` for `files/list_folder` (default 2000). Tests lower it to exercise pagination.
    pub list_limit: u32,
    /// Longest wait for a `Retry-After` (default 60 s). Tests lower it.
    pub max_retry_after: Duration,
}

impl DropboxRemote {
    pub fn new(cfg: DropboxConfig, creds: Box<dyn CredentialProvider>) -> Result<Self> {
        Self::with_endpoints(cfg, creds, DropboxEndpoints::default())
    }

    /// A remote that talks to `endpoints` instead of Dropbox (tests).
    pub fn with_endpoints(
        cfg: DropboxConfig,
        creds: Box<dyn CredentialProvider>,
        endpoints: DropboxEndpoints,
    ) -> Result<Self> {
        let agent = agent();
        let tokens = TokenCache::new(cfg.app_key()?, &endpoints, creds, agent.clone());
        Ok(Self { cfg, endpoints, tokens, agent, list_limit: 2000, max_retry_after: Duration::from_secs(60) })
    }

    pub fn config(&self) -> &DropboxConfig {
        &self.cfg
    }

    /// Sends one call, retrying expired tokens once, rate limits after `Retry-After` and
    /// transient failures. Returns the response whatever its status.
    fn call(&self, content: bool, route: &str, body: &Body) -> Result<Response> {
        let base = if content { &self.endpoints.content } else { &self.endpoints.api };
        let url = format!("{base}/2/{route}");
        let (mut attempt, mut limited, mut refreshed) = (0u32, 0u32, false);
        loop {
            let token = self.tokens.access_token()?;
            let wait = match self.send_once(&url, &token, body) {
                // Expired, or revoked: refresh once (a revoked refresh token fails there).
                Ok(r) if r.status == 401 && !refreshed => {
                    log::debug!("{route}: token rejected ({}), refreshing", r.summary());
                    self.tokens.invalidate();
                    refreshed = true;
                    continue;
                }
                Ok(r) if r.status == 401 => {
                    return Err(RemoteError::Auth(format!("Dropbox rejected the sign-in: {}", r.summary())))
                }
                // Rate limits, including too_many_write_operations (lock contention between the
                // user's devices): wait as long as Dropbox says.
                Ok(r) if matches!(r.status, 429 | 503) && limited < 3 => {
                    limited += 1;
                    let secs = r.retry_after.unwrap_or(1 << limited);
                    log::debug!("{route}: HTTP {} ({}), retrying in {secs} s", r.status, r.summary());
                    Duration::from_secs(secs).min(self.max_retry_after)
                }
                Ok(r) if r.status >= 500 && attempt < 2 => {
                    log::debug!("{route}: HTTP {}, retrying", r.status);
                    attempt += 1;
                    Duration::from_millis(200 << attempt)
                }
                Err(RemoteError::Network(e)) if attempt < 2 => {
                    log::debug!("{route}: {e}, retrying");
                    attempt += 1;
                    Duration::from_millis(200 << attempt)
                }
                Ok(r) if matches!(r.status, 429 | 503) || r.status >= 500 => {
                    return Err(RemoteError::Network(format!("{route}: HTTP {} {}", r.status, r.summary())))
                }
                other => return other,
            };
            std::thread::sleep(wait);
        }
    }

    fn send_once(&self, url: &str, token: &str, body: &Body) -> Result<Response> {
        let req = self.agent.post(url).set("Authorization", &format!("Bearer {token}"));
        let result = match body {
            Body::Json(json) => req.set("Content-Type", "application/json").send_bytes(json.as_bytes()),
            Body::Content { arg, data } => {
                req.set("Dropbox-API-Arg", arg).set("Content-Type", "application/octet-stream").send_bytes(data)
            }
        };
        read_response(result)
    }

    /// An error for an unexpected response to `op`.
    fn error(&self, op: &str, r: &Response) -> RemoteError {
        let summary = r.summary();
        match r.status {
            409 if summary.contains("insufficient_space") => {
                RemoteError::Other(format!("{op}: the Dropbox account is full"))
            }
            401 | 403 => RemoteError::Auth(format!("{op}: {summary}")),
            status => RemoteError::Http { status, message: format!("{op}: {summary}") },
        }
    }

    /// Every entry under `folder` (all levels with `recursive`), following the cursor. A missing
    /// folder has none.
    fn list_folder(&self, folder: &str, recursive: bool) -> Result<Vec<serde_json::Value>> {
        let arg = serde_json::json!({ "path": folder, "recursive": recursive, "limit": self.list_limit });
        let mut r = self.call(false, "files/list_folder", &Body::Json(arg.to_string()))?;
        let mut entries = Vec::new();
        loop {
            match r.status {
                200 => {}
                409 if r.summary().contains("not_found") => return Ok(entries),
                _ => return Err(self.error(&format!("list_folder {folder}"), &r)),
            }
            let page = r.json()?;
            entries.extend(page["entries"].as_array().cloned().unwrap_or_default());
            if page["has_more"].as_bool() != Some(true) {
                return Ok(entries);
            }
            let cursor = page["cursor"]
                .as_str()
                .ok_or_else(|| RemoteError::Other("list_folder: has_more without a cursor".into()))?;
            let arg = serde_json::json!({ "cursor": cursor });
            r = self.call(false, "files/list_folder/continue", &Body::Json(arg.to_string()))?;
        }
    }
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .redirects(0)
        .build()
}

fn read_response(result: std::result::Result<ureq::Response, ureq::Error>) -> Result<Response> {
    let resp = match result {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(ureq::Error::Transport(t)) => return Err(RemoteError::Network(t.to_string())),
    };
    let status = resp.status();
    let retry_after = resp.header("Retry-After").and_then(|v| v.trim().parse().ok());
    let mut body = Vec::new();
    resp.into_reader()
        .take(64 * 1024 * 1024)
        .read_to_end(&mut body)
        .map_err(|e| RemoteError::Network(e.to_string()))?;
    Ok(Response { status, body, retry_after })
}

/// JSON for an HTTP header (`Dropbox-API-Arg`): non-ASCII characters escaped as `\uXXXX`.
fn header_json(v: &serde_json::Value) -> String {
    let mut out = String::new();
    for c in v.to_string().chars() {
        if c.is_ascii() && c != '\x7f' {
            out.push(c);
        } else {
            let mut units = [0u16; 2];
            for u in c.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{u:04x}"));
            }
        }
    }
    out
}

impl Remote for DropboxRemote {
    fn describe(&self) -> String {
        self.cfg.uri()
    }

    fn put(&self, key: &str, data: &[u8], if_none_match: bool) -> Result<PutOutcome> {
        let path = self.cfg.path(key)?;
        // `add` alone treats identical contents as no conflict and "succeeds"; strict_conflict
        // makes every existing file a conflict, so create-only really is (DESIGN §30.3).
        let arg = if if_none_match {
            serde_json::json!({ "path": path, "mode": "add", "autorename": false, "mute": true, "strict_conflict": true })
        } else {
            serde_json::json!({ "path": path, "mode": "overwrite", "autorename": false, "mute": true })
        };
        let r = self.call(true, "files/upload", &Body::Content { arg: header_json(&arg), data })?;
        match r.status {
            200 => Ok(PutOutcome::Created),
            409 if if_none_match && r.summary().contains("conflict") => Ok(PutOutcome::AlreadyExists),
            _ => Err(self.error(&format!("upload {key}"), &r)),
        }
    }

    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let arg = serde_json::json!({ "path": self.cfg.path(key)? });
        let r = self.call(true, "files/download", &Body::Content { arg: header_json(&arg), data: &[] })?;
        match r.status {
            200 => Ok(Some(r.body)),
            409 if r.summary().contains("not_found") => Ok(None),
            _ => Err(self.error(&format!("download {key}"), &r)),
        }
    }

    fn head(&self, key: &str) -> Result<bool> {
        let arg = serde_json::json!({ "path": self.cfg.path(key)? });
        let r = self.call(false, "files/get_metadata", &Body::Json(arg.to_string()))?;
        match r.status {
            200 => Ok(r.json()?[".tag"] == "file"),
            409 if r.summary().contains("not_found") => Ok(false),
            _ => Err(self.error(&format!("get_metadata {key}"), &r)),
        }
    }

    fn list(&self, prefix: &str, start_after: Option<&str>) -> Result<Vec<String>> {
        // Folders, not string prefixes: list the folder the prefix is in, then filter.
        let dir = prefix.rsplit_once('/').map_or("", |(d, _)| d);
        let mut keys: Vec<String> = self
            .list_folder(&self.cfg.folder_path(dir)?, true)?
            .iter()
            .filter(|e| e[".tag"] == "file")
            .filter_map(|e| e["path_lower"].as_str().and_then(|p| self.cfg.key_of(p)))
            .filter(|k| k.starts_with(prefix) && start_after.map_or(true, |s| k.as_str() > s))
            .collect();
        keys.sort();
        Ok(keys)
    }

    fn list_dirs(&self, prefix: &str) -> Result<Vec<String>> {
        let dir = prefix.trim_end_matches('/');
        let mut dirs: Vec<String> = self
            .list_folder(&self.cfg.folder_path(dir)?, false)?
            .iter()
            .filter(|e| e[".tag"] == "folder")
            .filter_map(|e| e["name"].as_str().map(str::to_string))
            .collect();
        dirs.sort();
        dirs.dedup();
        Ok(dirs)
    }
}

#[cfg(test)]
mod tests;

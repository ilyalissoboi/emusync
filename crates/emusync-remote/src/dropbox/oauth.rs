//! Dropbox sign-in and tokens (DESIGN §30.4, §30.5).
//!
//! Sign-in is the OAuth 2 code flow with PKCE and `token_access_type=offline`: [`start`] makes
//! the URL and a `pending` string the host keeps, [`wait_redirect`] receives a loopback redirect
//! on desktop, and [`finish`] exchanges the code for a refresh token, which the host stores.
//! [`TokenCache`] turns that refresh token into access tokens, in memory only. No client secret
//! is involved anywhere.

use super::{read_response, DropboxConfig, DropboxEndpoints};
use crate::credentials::now_unix_ms;
use crate::{CredentialProvider, Credentials, RemoteError, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Access tokens are exchanged again this long before they expire.
const REFRESH_MARGIN_MS: i64 = 5 * 60 * 1000;

/// Access tokens for one Dropbox app, from the host's [`CredentialProvider`]: a refresh token is
/// exchanged (and again before expiry or after a 401), an access token is used as it is.
pub(crate) struct TokenCache {
    app_key: String,
    token_url: String,
    provider: Box<dyn CredentialProvider>,
    agent: ureq::Agent,
    /// The current access token and when it expires.
    cache: Mutex<Option<(String, i64)>>,
}

impl TokenCache {
    pub(crate) fn new(
        app_key: String,
        endpoints: &DropboxEndpoints,
        provider: Box<dyn CredentialProvider>,
        agent: ureq::Agent,
    ) -> Self {
        let token_url = format!("{}/oauth2/token", endpoints.api);
        Self { app_key, token_url, provider, agent, cache: Mutex::new(None) }
    }

    /// A current access token. Threads that ask at once wait for one exchange.
    pub(crate) fn access_token(&self) -> Result<String> {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((token, _)) = cache.as_ref().filter(|(_, exp)| now_unix_ms() < exp - REFRESH_MARGIN_MS) {
            return Ok(token.clone());
        }
        match self.provider.credentials()? {
            // The provider refreshes it itself (CallbackCredentials, by its expiry).
            Credentials::Bearer(token) => Ok(token),
            Credentials::OAuthRefresh(refresh) => {
                let (token, expires_in) = refresh_access_token(&self.agent, &self.token_url, &self.app_key, &refresh)?;
                *cache = Some((token.clone(), now_unix_ms() + expires_in * 1000));
                Ok(token)
            }
            other => Err(RemoteError::Auth(format!("Dropbox remotes need an OAuth token, not {}", other.kind()))),
        }
    }

    /// Dropbox rejected the last access token: exchange (or ask the provider) again.
    pub(crate) fn invalidate(&self) {
        self.cache.lock().unwrap_or_else(|e| e.into_inner()).take();
        self.provider.invalidate();
    }
}

/// POSTs a form to the token endpoint. Errors: `Auth` for a refused grant, `Network` for
/// transport failures and 5xx.
fn token_request(agent: &ureq::Agent, url: &str, form: &[(&str, &str)]) -> Result<serde_json::Value> {
    let r = read_response(agent.post(url).send_form(form))?;
    let json: Option<serde_json::Value> = serde_json::from_slice(&r.body).ok();
    match r.status {
        200 => json.ok_or_else(|| RemoteError::Other("the Dropbox token endpoint sent invalid JSON".into())),
        400 | 401 => {
            let field = |k: &str| json.as_ref().and_then(|j| j[k].as_str()).unwrap_or("").to_string();
            let (error, description) = (field("error"), field("error_description"));
            Err(RemoteError::Auth(if error == "invalid_grant" {
                format!("the Dropbox sign-in has expired or was revoked: sign in again ({description})")
            } else {
                format!("Dropbox refused the sign-in: {error} {description}").trim().to_string()
            }))
        }
        s if s >= 500 || s == 429 => Err(RemoteError::Network(format!("Dropbox token endpoint: HTTP {s}"))),
        s => Err(RemoteError::Http { status: s, message: "Dropbox token endpoint".into() }),
    }
}

/// Exchanges a refresh token for an access token and its lifetime in seconds.
fn refresh_access_token(agent: &ureq::Agent, url: &str, app_key: &str, refresh: &str) -> Result<(String, i64)> {
    let j = token_request(
        agent,
        url,
        &[("grant_type", "refresh_token"), ("refresh_token", refresh), ("client_id", app_key)],
    )?;
    let token = j["access_token"]
        .as_str()
        .ok_or_else(|| RemoteError::Other("the Dropbox token endpoint sent no access_token".into()))?;
    Ok((token.to_string(), j["expires_in"].as_i64().unwrap_or(4 * 3600)))
}

/// Why a sign-in step failed.
#[derive(Debug, thiserror::Error)]
pub enum SignInError {
    #[error("{0}")]
    InvalidArg(String),
    /// The loopback port is in use: start again without a redirect URI (paste the code).
    #[error("port {0} is in use")]
    PortInUse(u16),
    #[error("no answer from the browser in time")]
    Timeout,
    /// The user declined, the state doesn't match, or Dropbox refused the code.
    #[error("{0}")]
    Declined(String),
    #[error("{0}")]
    Remote(RemoteError),
}

impl From<RemoteError> for SignInError {
    fn from(e: RemoteError) -> Self {
        match e {
            RemoteError::Auth(m) => SignInError::Declined(m),
            RemoteError::InvalidUri(m) => SignInError::InvalidArg(m),
            other => SignInError::Remote(other),
        }
    }
}

/// What [`start`] returns.
pub struct SignInStart {
    /// The page to open in a browser.
    pub url: String,
    /// What [`wait_redirect`] and [`finish`] need: app key, PKCE verifier, state, redirect URI.
    /// Opaque to the host, which keeps it until [`finish`] (across a restart if it must).
    pub pending: String,
}

/// What a finished sign-in returns.
pub struct SignIn {
    /// For the host to store and hand back as [`Credentials::OAuthRefresh`].
    pub refresh_token: String,
    /// `dbid:…`: the same for every sign-in to this account.
    pub account_id: String,
    /// For a settings screen; empty if Dropbox didn't say.
    pub email: String,
}

impl std::fmt::Debug for SignIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignIn")
            .field("account_id", &self.account_id)
            .field("email", &self.email)
            .finish_non_exhaustive()
    }
}

fn random_b64(n: usize) -> String {
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).expect("the operating system's random number generator is unavailable");
    URL_SAFE_NO_PAD.encode(b)
}

/// The PKCE `code_challenge` for `verifier` (S256).
pub(crate) fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn form_encode(pairs: &[(&str, &str)]) -> String {
    let enc = |s: &str| {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
                _ => format!("%{b:02X}"),
            })
            .collect::<String>()
    };
    pairs.iter().map(|(k, v)| format!("{}={}", enc(k), enc(v))).collect::<Vec<_>>().join("&")
}

fn form_decode(s: &str) -> String {
    let s = s.replace('+', " ");
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 3 <= bytes.len() {
            if let Some(b) = std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The query parameters of a URL (or of a bare query string).
fn query_params(url: &str) -> Vec<(String, String)> {
    let query = url.split_once('?').map_or(url, |(_, q)| q);
    let query = query.split('#').next().unwrap_or("");
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (form_decode(k), form_decode(v))
        })
        .collect()
}

/// The loopback port of a redirect URI (`http://127.0.0.1:<port>/…` or `http://localhost:…`).
fn loopback_port(redirect_uri: &str) -> Option<u16> {
    let rest = redirect_uri.strip_prefix("http://")?;
    let host_port = rest.split('/').next()?;
    let (host, port) = host_port.rsplit_once(':')?;
    matches!(host, "127.0.0.1" | "localhost").then_some(())?;
    port.parse().ok()
}

/// Starts a sign-in for the repository at `remote_uri` (DESIGN §30.5, `es_oauth_start`). The
/// URI's `app_key`, else the built-in one, picks the Dropbox app. `redirect_uri` is a loopback
/// or app-scheme URI registered for that app, or `""` for paste-the-code. No network.
pub fn start(remote_uri: &str, redirect_uri: &str) -> std::result::Result<SignInStart, SignInError> {
    start_with(&DropboxEndpoints::default(), remote_uri, redirect_uri)
}

#[doc(hidden)]
pub fn start_with(
    endpoints: &DropboxEndpoints,
    remote_uri: &str,
    redirect_uri: &str,
) -> std::result::Result<SignInStart, SignInError> {
    if !remote_uri.starts_with("dropbox:") {
        return Err(SignInError::InvalidArg(format!("{remote_uri:?}: only dropbox: remotes sign in")));
    }
    let app_key = DropboxConfig::parse(remote_uri)?.app_key()?;
    if redirect_uri.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(SignInError::InvalidArg(format!("{redirect_uri:?} is not a valid redirect URI")));
    }
    let verifier = random_b64(32);
    let state = random_b64(16);
    let challenge = challenge(&verifier);
    let mut params = vec![
        ("client_id", app_key.as_str()),
        ("response_type", "code"),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("token_access_type", "offline"),
        ("state", state.as_str()),
    ];
    if !redirect_uri.is_empty() {
        params.push(("redirect_uri", redirect_uri));
    }
    let url = format!("{}?{}", endpoints.authorize, form_encode(&params));
    let pending = serde_json::json!({
        "v": 1, "provider": "dropbox", "app_key": app_key, "verifier": verifier, "state": state,
        "redirect_uri": redirect_uri,
    })
    .to_string();
    Ok(SignInStart { url, pending })
}

struct Pending {
    app_key: String,
    verifier: String,
    state: String,
    redirect_uri: String,
}

fn parse_pending(pending: &str) -> std::result::Result<Pending, SignInError> {
    let bad = || SignInError::InvalidArg("pending is not from es_oauth_start".into());
    let j: serde_json::Value = serde_json::from_str(pending).map_err(|_| bad())?;
    if j["v"] != 1 || j["provider"] != "dropbox" {
        return Err(bad());
    }
    let field = |k: &str| j[k].as_str().map(str::to_string).ok_or_else(bad);
    Ok(Pending {
        app_key: field("app_key")?,
        verifier: field("verifier")?,
        state: field("state")?,
        redirect_uri: field("redirect_uri")?,
    })
}

const SIGNED_IN_PAGE: &str = "<!doctype html><meta charset=utf-8><title>emusync</title>\
    <body style=\"font-family:sans-serif;margin:3em\"><h1>Signed in</h1>\
    <p>You can close this tab and go back to emusync.</p></body>";

/// Desktop: listens on the loopback port of the pending sign-in's redirect URI until the
/// browser arrives with the redirect or `timeout` passes, and returns the redirect URL
/// (`es_oauth_wait_redirect`). Requests for other paths (a favicon) get a 404.
pub fn wait_redirect(pending: &str, timeout: Duration) -> std::result::Result<String, SignInError> {
    let p = parse_pending(pending)?;
    let port = loopback_port(&p.redirect_uri)
        .ok_or_else(|| SignInError::InvalidArg(format!("{:?} is not a loopback redirect URI", p.redirect_uri)))?;
    let path = p.redirect_uri.splitn(4, '/').nth(3).map_or("/".to_string(), |s| format!("/{s}"));
    let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|e| match e.kind() {
        std::io::ErrorKind::AddrInUse | std::io::ErrorKind::PermissionDenied => SignInError::PortInUse(port),
        _ => SignInError::Remote(RemoteError::Other(format!("listening on port {port}: {e}"))),
    })?;
    listener.set_nonblocking(true).map_err(|e| SignInError::Remote(RemoteError::Other(e.to_string())))?;
    let deadline = Instant::now() + timeout;
    loop {
        let (mut stream, _) = match listener.accept() {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(SignInError::Timeout);
                }
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            Err(e) => return Err(SignInError::Remote(RemoteError::Other(format!("accepting the redirect: {e}")))),
        };
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut buf = [0u8; 8192];
        let mut req = Vec::new();
        while !req.windows(4).any(|w| w == b"\r\n\r\n") && req.len() < buf.len() {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => req.extend_from_slice(&buf[..n]),
            }
        }
        let req = String::from_utf8_lossy(&req);
        let target = req.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("");
        if target.split('?').next() != Some(path.as_str()) {
            let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            continue;
        }
        let _ = stream.write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{SIGNED_IN_PAGE}",
                SIGNED_IN_PAGE.len()
            )
            .as_bytes(),
        );
        let origin = p.redirect_uri.splitn(4, '/').take(3).collect::<Vec<_>>().join("/");
        return Ok(format!("{origin}{target}"));
    }
}

/// Exchanges the redirect the browser arrived with, or the code the user pasted, for a refresh
/// token (`es_oauth_finish`). Blocks for the token exchange and an account lookup.
pub fn finish(pending: &str, redirect_or_code: &str) -> std::result::Result<SignIn, SignInError> {
    finish_with(&DropboxEndpoints::default(), pending, redirect_or_code)
}

#[doc(hidden)]
pub fn finish_with(
    endpoints: &DropboxEndpoints,
    pending: &str,
    redirect_or_code: &str,
) -> std::result::Result<SignIn, SignInError> {
    let p = parse_pending(pending)?;
    let input = redirect_or_code.trim();
    let code = if input.contains("://") || input.contains('?') || input.contains("code=") {
        let params = query_params(input);
        let get = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        if let Some(error) = get("error") {
            let description = get("error_description").unwrap_or_default();
            return Err(SignInError::Declined(
                format!("the sign-in was not completed: {error} {description}").trim().into(),
            ));
        }
        if get("state").as_deref() != Some(p.state.as_str()) {
            return Err(SignInError::Declined("the redirect doesn't belong to this sign-in (state mismatch)".into()));
        }
        get("code").filter(|c| !c.is_empty()).ok_or_else(|| SignInError::Declined("the redirect has no code".into()))?
    } else if input.is_empty() {
        return Err(SignInError::InvalidArg("no redirect or code".into()));
    } else {
        input.to_string()
    };
    let agent = super::agent();
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("code_verifier", p.verifier.as_str()),
        ("client_id", p.app_key.as_str()),
    ];
    if !p.redirect_uri.is_empty() {
        form.push(("redirect_uri", p.redirect_uri.as_str()));
    }
    let j = token_request(&agent, &format!("{}/oauth2/token", endpoints.api), &form)?;
    let refresh_token = j["refresh_token"]
        .as_str()
        .ok_or_else(|| SignInError::Remote(RemoteError::Other("Dropbox sent no refresh token".into())))?
        .to_string();
    let access = j["access_token"].as_str().unwrap_or_default();
    let mut account_id = j["account_id"].as_str().unwrap_or_default().to_string();
    // The email address is only for display: a failed lookup doesn't fail the sign-in.
    let mut email = String::new();
    let r = read_response(
        agent
            .post(&format!("{}/2/users/get_current_account", endpoints.api))
            .set("Authorization", &format!("Bearer {access}"))
            .set("Content-Type", "application/json")
            .send_bytes(b"null"),
    );
    match r {
        Ok(r) if r.status == 200 => {
            let a: serde_json::Value = serde_json::from_slice(&r.body).unwrap_or_default();
            email = a["email"].as_str().unwrap_or_default().to_string();
            if account_id.is_empty() {
                account_id = a["account_id"].as_str().unwrap_or_default().to_string();
            }
        }
        Ok(r) => log::warn!("Dropbox account lookup: HTTP {}", r.status),
        Err(e) => log::warn!("Dropbox account lookup: {e}"),
    }
    Ok(SignIn { refresh_token, account_id, email })
}

/// Revokes a refresh token (sign-out), for the app `remote_uri` names. A token Dropbox already
/// considers revoked counts as success.
pub fn revoke(remote_uri: &str, refresh_token: &str) -> Result<()> {
    revoke_with(&DropboxEndpoints::default(), remote_uri, refresh_token)
}

#[doc(hidden)]
pub fn revoke_with(endpoints: &DropboxEndpoints, remote_uri: &str, refresh_token: &str) -> Result<()> {
    let app_key = DropboxConfig::parse(remote_uri)?.app_key()?;
    let agent = super::agent();
    let access = match refresh_access_token(&agent, &format!("{}/oauth2/token", endpoints.api), &app_key, refresh_token)
    {
        Ok((t, _)) => t,
        Err(RemoteError::Auth(_)) => return Ok(()),
        Err(e) => return Err(e),
    };
    let r = read_response(
        agent
            .post(&format!("{}/2/auth/token/revoke", endpoints.api))
            .set("Authorization", &format!("Bearer {access}"))
            .set("Content-Type", "application/json")
            .send_bytes(b"null"),
    )?;
    match r.status {
        200 | 401 => Ok(()),
        s => Err(RemoteError::Http { status: s, message: format!("token/revoke: {}", r.summary()) }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_matches_rfc7636() {
        // RFC 7636, appendix B.
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn start_builds_the_url_and_pending() {
        let ep = DropboxEndpoints::default();
        let s = start_with(&ep, "dropbox:/saves?app_key=abc123", "http://127.0.0.1:53682/dropbox").unwrap();
        assert!(s.url.starts_with("https://www.dropbox.com/oauth2/authorize?client_id=abc123&response_type=code&"));
        assert!(s.url.contains("&code_challenge_method=S256&token_access_type=offline&"));
        assert!(s.url.ends_with("&redirect_uri=http%3A%2F%2F127.0.0.1%3A53682%2Fdropbox"));
        let p = parse_pending(&s.pending).unwrap();
        assert_eq!(p.app_key, "abc123");
        assert_eq!(p.verifier.len(), 43);
        let challenge_in_url = query_params(&s.url).into_iter().find(|(k, _)| k == "code_challenge").unwrap().1;
        assert_eq!(challenge_in_url, challenge(&p.verifier));
        // Paste-the-code: no redirect URI at all.
        let s = start_with(&ep, "dropbox:/saves?app_key=abc123", "").unwrap();
        assert!(!s.url.contains("redirect_uri"));
        // Not Dropbox, no app key (in a build without one), a bad redirect.
        assert!(matches!(start_with(&ep, "s3://b/p", ""), Err(SignInError::InvalidArg(_))));
        if crate::dropbox::BUILT_IN_APP_KEY.is_none() {
            assert!(matches!(start_with(&ep, "dropbox:/saves", ""), Err(SignInError::InvalidArg(_))));
        }
        assert!(matches!(start_with(&ep, "dropbox:/s?app_key=k", "http://x y"), Err(SignInError::InvalidArg(_))));
        assert!(parse_pending("{}").is_err() && parse_pending("nonsense").is_err());
    }

    #[test]
    fn redirect_parsing() {
        assert_eq!(loopback_port("http://127.0.0.1:53682/dropbox"), Some(53682));
        assert_eq!(loopback_port("http://localhost:8080/"), Some(8080));
        assert_eq!(loopback_port("https://127.0.0.1:1/x"), None);
        assert_eq!(loopback_port("io.example.emusync://dropbox"), None);
        let q = query_params("http://127.0.0.1:1/dropbox?code=a%2Bb&state=s+t#frag");
        assert_eq!(q, [("code".to_string(), "a+b".to_string()), ("state".to_string(), "s t".to_string())]);
    }

    #[test]
    fn wait_redirect_receives_the_browser() {
        // A free port, released for wait_redirect to take.
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let s = start_with(
            &DropboxEndpoints::default(),
            "dropbox:/?app_key=k",
            &format!("http://127.0.0.1:{port}/dropbox"),
        )
        .unwrap();
        let pending = s.pending.clone();
        let waiter = std::thread::spawn(move || wait_redirect(&pending, Duration::from_secs(10)));
        let get = |path: &str| loop {
            match std::net::TcpStream::connect(("127.0.0.1", port)) {
                Ok(mut c) => {
                    c.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes()).unwrap();
                    let mut out = String::new();
                    let _ = c.read_to_string(&mut out);
                    break out;
                }
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        };
        assert!(get("/favicon.ico").starts_with("HTTP/1.1 404"));
        assert!(get("/dropbox?code=C&state=S").contains("Signed in"));
        assert_eq!(waiter.join().unwrap().unwrap(), format!("http://127.0.0.1:{port}/dropbox?code=C&state=S"));
        // Nobody comes: a timeout.
        assert!(matches!(wait_redirect(&s.pending, Duration::from_millis(100)), Err(SignInError::Timeout)));
        // The port is taken.
        let _taken = TcpListener::bind(("127.0.0.1", port)).unwrap();
        assert!(
            matches!(wait_redirect(&s.pending, Duration::from_millis(100)), Err(SignInError::PortInUse(p)) if p == port)
        );
    }
}

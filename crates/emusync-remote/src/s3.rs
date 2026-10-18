//! S3 and S3-compatible backend (DESIGN §6.2).
//!
//! SigV4 signing via `aws-sigv4` on top of a blocking rustls HTTP client (`ureq`), rather than
//! the full AWS SDK. Only PutObject (create-only), GetObject, HeadObject and ListObjectsV2 are
//! used.

use crate::{PutOutcome, Remote, RemoteError, Result};
use aws_sigv4::http_request::{
    sign, PayloadChecksumKind, PercentEncodingMode, SignableBody, SignableRequest, SigningSettings,
    UriPathNormalizationMode,
};
use aws_sigv4::sign::v4;
use aws_smithy_runtime_api::client::identity::Identity;
use std::time::{Duration, SystemTime};

/// Parsed `s3://bucket/prefix?region=…&endpoint=…&path_style=1` URI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Config {
    pub bucket: String,
    /// Repository prefix inside the bucket, without leading or trailing slashes.
    pub prefix: String,
    pub region: String,
    /// Custom endpoint (`https://host[:port]`) for S3-compatible services.
    pub endpoint: Option<String>,
    pub path_style: bool,
}

impl S3Config {
    pub fn parse(uri: &str) -> Result<Self> {
        let rest = uri
            .strip_prefix("s3://")
            .ok_or_else(|| RemoteError::InvalidUri(format!("{uri:?} does not start with s3://")))?;
        let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
        let (bucket, prefix) = path.split_once('/').unwrap_or((path, ""));
        if bucket.is_empty() {
            return Err(RemoteError::InvalidUri(format!("{uri:?} has no bucket")));
        }
        let mut cfg = S3Config {
            bucket: bucket.to_string(),
            prefix: prefix.trim_matches('/').to_string(),
            region: "us-east-1".to_string(),
            endpoint: None,
            path_style: false,
        };
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            let v = percent_decode(v);
            match k {
                "region" => cfg.region = v,
                "endpoint" => cfg.endpoint = Some(v.trim_end_matches('/').to_string()),
                "path_style" => cfg.path_style = matches!(v.as_str(), "1" | "true" | "yes"),
                _ => return Err(RemoteError::InvalidUri(format!("unknown parameter {k:?} in {uri:?}"))),
            }
        }
        if let Some(ep) = &cfg.endpoint {
            if !(ep.starts_with("http://") || ep.starts_with("https://")) {
                return Err(RemoteError::InvalidUri(format!("endpoint {ep:?} must start with http:// or https://")));
            }
        }
        Ok(cfg)
    }

    /// `(scheme, host[:port])` of the service endpoint.
    fn endpoint_parts(&self) -> (String, String) {
        match &self.endpoint {
            Some(ep) => {
                let (scheme, host) = ep.split_once("://").unwrap();
                (scheme.to_string(), host.to_string())
            }
            None => ("https".to_string(), format!("s3.{}.amazonaws.com", self.region)),
        }
    }

    /// Base URL for bucket-level requests (no trailing slash for path style).
    fn bucket_url(&self) -> String {
        let (scheme, host) = self.endpoint_parts();
        if self.path_style {
            format!("{scheme}://{host}/{}", self.bucket)
        } else {
            format!("{scheme}://{}.{host}", self.bucket)
        }
    }

    fn full_key(&self, key: &str) -> String {
        if self.prefix.is_empty() {
            key.to_string()
        } else {
            format!("{}/{key}", self.prefix)
        }
    }

    fn object_url(&self, key: &str) -> String {
        format!("{}/{}", self.bucket_url(), encode_path(&self.full_key(key)))
    }
}

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

/// Supplies credentials on demand. Called before each request, so a provider can refresh
/// temporary credentials. Implementations must not persist secrets in the data directory.
pub trait CredentialProvider: Send + Sync {
    fn credentials(&self) -> Result<S3Credentials>;
}

pub struct StaticCredentials(pub S3Credentials);

impl CredentialProvider for StaticCredentials {
    fn credentials(&self) -> Result<S3Credentials> {
        Ok(self.0.clone())
    }
}

/// Reads `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and optional `AWS_SESSION_TOKEN`.
pub struct EnvCredentials;

impl CredentialProvider for EnvCredentials {
    fn credentials(&self) -> Result<S3Credentials> {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        match (get("AWS_ACCESS_KEY_ID"), get("AWS_SECRET_ACCESS_KEY")) {
            (Some(access_key_id), Some(secret_access_key)) => {
                Ok(S3Credentials { access_key_id, secret_access_key, session_token: get("AWS_SESSION_TOKEN") })
            }
            _ => Err(RemoteError::Auth("AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY must be set".into())),
        }
    }
}

pub struct S3Remote {
    cfg: S3Config,
    creds: Box<dyn CredentialProvider>,
    agent: ureq::Agent,
    /// Page size for ListObjectsV2 (default: server default, 1000). Tests lower it to exercise
    /// pagination.
    pub max_keys: Option<u32>,
}

impl S3Remote {
    pub fn new(cfg: S3Config, creds: Box<dyn CredentialProvider>) -> Result<Self> {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .redirects(0)
            .build();
        Ok(Self { cfg, creds, agent, max_keys: None })
    }

    pub fn config(&self) -> &S3Config {
        &self.cfg
    }

    /// Signs and sends one request, retrying transient failures. Returns status and body.
    fn send(&self, method: &str, url: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<(u16, Vec<u8>)> {
        let mut attempt = 0;
        loop {
            match self.send_once(method, url, headers, body) {
                Ok((status, body)) if status >= 500 && attempt < 2 => {
                    log::debug!("{method} {url}: HTTP {status}, retrying");
                    let _ = body;
                }
                Err(RemoteError::Network(e)) if attempt < 2 => log::debug!("{method} {url}: {e}, retrying"),
                other => return other,
            }
            attempt += 1;
            std::thread::sleep(Duration::from_millis(200 << attempt));
        }
    }

    fn send_once(&self, method: &str, url: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<(u16, Vec<u8>)> {
        let creds = self.creds.credentials()?;
        let identity: Identity = aws_credential_types::Credentials::new(
            creds.access_key_id,
            creds.secret_access_key,
            creds.session_token,
            None,
            "emusync",
        )
        .into();
        let mut settings = SigningSettings::default();
        settings.payload_checksum_kind = PayloadChecksumKind::XAmzSha256;
        settings.percent_encoding_mode = PercentEncodingMode::Single;
        settings.uri_path_normalization_mode = UriPathNormalizationMode::Disabled;
        let params = v4::SigningParams::builder()
            .identity(&identity)
            .region(&self.cfg.region)
            .name("s3")
            .time(SystemTime::now())
            .settings(settings)
            .build()
            .map_err(|e| RemoteError::Other(format!("signing: {e}")))?
            .into();
        let signable = SignableRequest::new(method, url, headers.iter().copied(), SignableBody::Bytes(body))
            .map_err(|e| RemoteError::Other(format!("signing: {e}")))?;
        let (instructions, _) =
            sign(signable, &params).map_err(|e| RemoteError::Other(format!("signing: {e}")))?.into_parts();

        let mut req = self.agent.request(method, url);
        for (k, v) in headers {
            req = req.set(k, v);
        }
        for (k, v) in instructions.headers() {
            req = req.set(k, v);
        }
        let result = if method == "PUT" { req.send_bytes(body) } else { req.call() };
        let resp = match result {
            Ok(r) => r,
            Err(ureq::Error::Status(_, r)) => r,
            Err(ureq::Error::Transport(t)) => return Err(RemoteError::Network(t.to_string())),
        };
        let status = resp.status();
        let mut out = Vec::new();
        if method != "HEAD" {
            use std::io::Read;
            resp.into_reader()
                .take(64 * 1024 * 1024)
                .read_to_end(&mut out)
                .map_err(|e| RemoteError::Network(e.to_string()))?;
        }
        Ok((status, out))
    }

    fn error(&self, op: &str, status: u16, body: &[u8]) -> RemoteError {
        let text = String::from_utf8_lossy(body);
        let code = first_tag(&text, "Code").unwrap_or_default();
        let msg = first_tag(&text, "Message").unwrap_or_default();
        let message = format!("{op}: {code} {msg}").trim().to_string();
        match status {
            401 | 403 => RemoteError::Auth(message),
            301 | 307 => RemoteError::Http {
                status,
                message: format!("{message} (wrong region or endpoint for bucket {:?}?)", self.cfg.bucket),
            },
            501 => RemoteError::Unsupported(message),
            s if s >= 500 => RemoteError::Network(format!("HTTP {s} {message}")),
            _ => RemoteError::Http { status, message },
        }
    }

    /// One ListObjectsV2 call sequence (following continuation tokens).
    fn list_v2(&self, prefix: &str, start_after: Option<&str>, delimiter: bool) -> Result<ListResult> {
        let full_prefix = self.cfg.full_key(prefix);
        let mut result = ListResult::default();
        let mut token: Option<String> = None;
        loop {
            let mut q = vec![("list-type", "2".to_string()), ("prefix", full_prefix.clone())];
            if delimiter {
                q.push(("delimiter", "/".to_string()));
            }
            if let Some(s) = start_after {
                q.push(("start-after", self.cfg.full_key(s)));
            }
            if let Some(t) = &token {
                q.push(("continuation-token", t.clone()));
            }
            if let Some(m) = self.max_keys {
                q.push(("max-keys", m.to_string()));
            }
            q.sort();
            let query: Vec<String> = q.iter().map(|(k, v)| format!("{k}={}", encode_component(v))).collect();
            let url = format!("{}/?{}", self.cfg.bucket_url(), query.join("&"));
            let url = if self.cfg.path_style { url.replacen("/?", "?", 1) } else { url };
            let (status, body) = self.send("GET", &url, &[], &[])?;
            if status != 200 {
                return Err(self.error("ListObjectsV2", status, &body));
            }
            let text = String::from_utf8_lossy(&body);
            for block in all_tags(&text, "Contents") {
                if let Some(k) = first_tag(block, "Key") {
                    result.keys.push(k);
                }
            }
            for block in all_tags(&text, "CommonPrefixes") {
                if let Some(p) = first_tag(block, "Prefix") {
                    result.prefixes.push(p);
                }
            }
            let truncated = first_tag(&text, "IsTruncated").as_deref() == Some("true");
            token = first_tag(&text, "NextContinuationToken");
            if !truncated || token.is_none() {
                break;
            }
        }
        Ok(result)
    }

    fn strip(&self, full: &str) -> Option<String> {
        if self.cfg.prefix.is_empty() {
            Some(full.to_string())
        } else {
            full.strip_prefix(&self.cfg.prefix).and_then(|r| r.strip_prefix('/')).map(str::to_string)
        }
    }
}

#[derive(Default)]
struct ListResult {
    keys: Vec<String>,
    prefixes: Vec<String>,
}

impl Remote for S3Remote {
    fn describe(&self) -> String {
        let mut s = format!("s3://{}/{}?region={}", self.cfg.bucket, self.cfg.prefix, self.cfg.region);
        if let Some(ep) = &self.cfg.endpoint {
            s.push_str(&format!("&endpoint={ep}"));
        }
        if self.cfg.path_style {
            s.push_str("&path_style=1");
        }
        s
    }

    fn put(&self, key: &str, data: &[u8], if_none_match: bool) -> Result<PutOutcome> {
        let url = self.cfg.object_url(key);
        let headers: &[(&str, &str)] = if if_none_match { &[("if-none-match", "*")] } else { &[] };
        let (status, body) = self.send("PUT", &url, headers, data)?;
        match status {
            200 | 201 | 204 => Ok(PutOutcome::Created),
            412 if if_none_match => Ok(PutOutcome::AlreadyExists),
            // A concurrent conditional write to the same key is in flight; for create-only keys
            // the other writer wins, which is equivalent to "already exists" once it completes.
            409 if if_none_match => Err(RemoteError::Network(format!("PutObject {key}: concurrent write, retry"))),
            _ => Err(self.error(&format!("PutObject {key}"), status, &body)),
        }
    }

    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let (status, body) = self.send("GET", &self.cfg.object_url(key), &[], &[])?;
        match status {
            200 => Ok(Some(body)),
            404 => Ok(None),
            _ => Err(self.error(&format!("GetObject {key}"), status, &body)),
        }
    }

    fn head(&self, key: &str) -> Result<bool> {
        let (status, body) = self.send("HEAD", &self.cfg.object_url(key), &[], &[])?;
        match status {
            200 => Ok(true),
            404 => Ok(false),
            _ => Err(self.error(&format!("HeadObject {key}"), status, &body)),
        }
    }

    fn list(&self, prefix: &str, start_after: Option<&str>) -> Result<Vec<String>> {
        let r = self.list_v2(prefix, start_after, false)?;
        let mut keys: Vec<String> = r.keys.iter().filter_map(|k| self.strip(k)).collect();
        keys.sort();
        Ok(keys)
    }

    fn list_dirs(&self, prefix: &str) -> Result<Vec<String>> {
        let r = self.list_v2(prefix, None, true)?;
        let mut dirs: Vec<String> = r
            .prefixes
            .iter()
            .filter_map(|p| self.strip(p))
            .filter_map(|p| p.strip_prefix(prefix).map(|d| d.trim_end_matches('/').to_string()))
            .filter(|d| !d.is_empty())
            .collect();
        dirs.sort();
        dirs.dedup();
        Ok(dirs)
    }
}

fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~')
}

/// RFC 3986 encoding of a query component (everything but unreserved characters).
fn encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if is_unreserved(b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Encodes an object key for the URL path, keeping `/` separators.
fn encode_path(key: &str) -> String {
    key.split('/').map(encode_component).collect::<Vec<_>>().join("/")
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 3 <= bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
            if let Ok(b) = u8::from_str_radix(hex, 16) {
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

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// Inner text of every `<tag>…</tag>` (not nested) in `xml`.
fn all_tags<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        match after.find(&close) {
            Some(end) => {
                out.push(&after[..end]);
                rest = &after[end + close.len()..];
            }
            None => break,
        }
    }
    out
}

fn first_tag(xml: &str, tag: &str) -> Option<String> {
    all_tags(xml, tag).first().map(|s| xml_unescape(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_uris() {
        let c = S3Config::parse("s3://my-saves/emusync/v1?region=eu-west-1").unwrap();
        assert_eq!(c.bucket, "my-saves");
        assert_eq!(c.prefix, "emusync/v1");
        assert_eq!(c.region, "eu-west-1");
        assert_eq!(c.object_url("blobs/ab/x"), "https://my-saves.s3.eu-west-1.amazonaws.com/emusync/v1/blobs/ab/x");

        let c = S3Config::parse("s3://saves/emusync/v1?endpoint=http://nas.local:9000&path_style=1").unwrap();
        assert!(c.path_style);
        assert_eq!(c.object_url("repo.json"), "http://nas.local:9000/saves/emusync/v1/repo.json");
        assert_eq!(c.bucket_url(), "http://nas.local:9000/saves");

        let c = S3Config::parse("s3://b?endpoint=https%3A%2F%2Fx.r2.cloudflarestorage.com&region=auto").unwrap();
        assert_eq!(c.prefix, "");
        assert_eq!(c.object_url("a b"), "https://b.x.r2.cloudflarestorage.com/a%20b");

        assert!(S3Config::parse("s3://").is_err());
        assert!(S3Config::parse("s3://b?bogus=1").is_err());
        assert!(S3Config::parse("s3://b?endpoint=nas:9000").is_err());
    }

    #[test]
    fn list_xml() {
        let xml = "<ListBucketResult><Prefix>p/</Prefix><Contents><Key>p/a&amp;b</Key></Contents>\
                   <Contents><Key>p/c</Key></Contents><CommonPrefixes><Prefix>p/d/</Prefix></CommonPrefixes>\
                   <IsTruncated>false</IsTruncated></ListBucketResult>";
        let keys: Vec<String> = all_tags(xml, "Contents").iter().filter_map(|b| first_tag(b, "Key")).collect();
        assert_eq!(keys, ["p/a&b", "p/c"]);
        let dirs: Vec<String> = all_tags(xml, "CommonPrefixes").iter().filter_map(|b| first_tag(b, "Prefix")).collect();
        assert_eq!(dirs, ["p/d/"]);
    }

    #[test]
    fn encoding() {
        assert_eq!(encode_component("devices/x/log/"), "devices%2Fx%2Flog%2F");
        assert_eq!(encode_path("a b/c+d"), "a%20b/c%2Bd");
        assert_eq!(percent_decode("http%3A%2F%2Fh%3A9000"), "http://h:9000");
        assert_eq!(percent_decode("50%"), "50%");
    }
}

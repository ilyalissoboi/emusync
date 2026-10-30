//! A fake Dropbox for tests (DESIGN §30.8): an HTTP server on the loopback address that
//! implements the calls the backend and sign-in make, with Dropbox's quirks (case-insensitive
//! paths, identical uploads that only conflict with `strict_conflict`, unordered listings with
//! cursors) and injectable failures.

use super::oauth::challenge;
use super::DropboxEndpoints;
use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

/// A failure the next matching request gets instead of its answer.
#[derive(Debug, Clone)]
pub struct Injected {
    /// Only requests whose path contains this (`""` for any).
    pub route: String,
    pub status: u16,
    pub body: String,
    pub retry_after: Option<u64>,
}

#[derive(Default)]
struct State {
    /// Files by lower-case path: (path as created, contents).
    files: BTreeMap<String, (String, Vec<u8>)>,
    /// Access tokens that are valid now.
    access_tokens: Vec<String>,
    /// Refresh tokens that are valid now.
    refresh_tokens: Vec<String>,
    /// Codes waiting to be exchanged: code → (PKCE challenge, redirect URI).
    codes: BTreeMap<String, (String, String)>,
    injected: VecDeque<Injected>,
    issued: u32,
}

pub struct FakeDropbox {
    pub app_key: String,
    base: String,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    /// Entries per `list_folder` page (default 3, to exercise cursors).
    page_size: Arc<AtomicU32>,
    /// Refresh-token exchanges so far.
    pub refreshes: Arc<AtomicU32>,
    /// `files/upload` calls so far.
    pub uploads: Arc<AtomicU32>,
}

impl FakeDropbox {
    pub fn start(app_key: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("binding the fake Dropbox");
        let base = format!("http://{}", listener.local_addr().unwrap());
        let fake = Self {
            app_key: app_key.to_string(),
            base,
            state: Arc::default(),
            stop: Arc::default(),
            page_size: Arc::new(AtomicU32::new(3)),
            refreshes: Arc::default(),
            uploads: Arc::default(),
        };
        let ctx = Ctx {
            app_key: fake.app_key.clone(),
            state: fake.state.clone(),
            page_size: fake.page_size.clone(),
            refreshes: fake.refreshes.clone(),
            uploads: fake.uploads.clone(),
        };
        let stop = fake.stop.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let ctx = ctx.clone();
                std::thread::spawn(move || ctx.serve(stream));
            }
        });
        fake
    }

    pub fn endpoints(&self) -> DropboxEndpoints {
        DropboxEndpoints {
            api: self.base.clone(),
            content: self.base.clone(),
            authorize: format!("{}/oauth2/authorize", self.base),
        }
    }

    /// A refresh token the fake accepts, as a sign-in would have issued.
    pub fn issue_refresh_token(&self) -> String {
        let mut s = self.state.lock().unwrap();
        s.issued += 1;
        let t = format!("refresh-{}", s.issued);
        s.refresh_tokens.push(t.clone());
        t
    }

    /// The user approved the sign-in whose authorization URL is `url`: returns the code Dropbox
    /// would put in the redirect.
    pub fn approve(&self, url: &str) -> String {
        let param = |k: &str| {
            url.split_once('?')
                .map_or("", |(_, q)| q)
                .split('&')
                .find_map(|p| p.strip_prefix(&format!("{k}=")))
                .map(|v| v.replace("%3A", ":").replace("%2F", "/"))
                .unwrap_or_default()
        };
        assert_eq!(param("client_id"), self.app_key, "sign-in for another app");
        let mut s = self.state.lock().unwrap();
        s.issued += 1;
        let code = format!("code-{}", s.issued);
        s.codes.insert(code.clone(), (param("code_challenge"), param("redirect_uri")));
        code
    }

    /// Every access token stops working, as if they had all expired.
    pub fn expire_access_tokens(&self) {
        self.state.lock().unwrap().access_tokens.clear();
    }

    /// The user disconnected the app: no token works any more.
    pub fn revoke_all(&self) {
        let mut s = self.state.lock().unwrap();
        s.access_tokens.clear();
        s.refresh_tokens.clear();
    }

    pub fn inject(&self, failure: Injected) {
        self.state.lock().unwrap().injected.push_back(failure);
    }

    pub fn set_page_size(&self, n: u32) {
        self.page_size.store(n.max(1), Ordering::SeqCst);
    }

    /// Paths of the stored files, as created, sorted.
    pub fn paths(&self) -> Vec<String> {
        self.state.lock().unwrap().files.values().map(|(p, _)| p.clone()).collect()
    }
}

impl Drop for FakeDropbox {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop.
        let _ = TcpStream::connect(self.base.trim_start_matches("http://"));
    }
}

#[derive(Clone)]
struct Ctx {
    app_key: String,
    state: Arc<Mutex<State>>,
    page_size: Arc<AtomicU32>,
    refreshes: Arc<AtomicU32>,
    uploads: Arc<AtomicU32>,
}

struct Req {
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Req {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn json(status: u16, v: serde_json::Value) -> Resp {
    Resp { status, headers: vec![("Content-Type".into(), "application/json".into())], body: v.to_string().into_bytes() }
}

fn endpoint_error(summary: &str) -> Resp {
    json(409, serde_json::json!({ "error_summary": summary, "error": { ".tag": summary.split('/').next() } }))
}

fn metadata(tag: &str, display: &str) -> serde_json::Value {
    let name = display.rsplit('/').next().unwrap_or("");
    serde_json::json!({ ".tag": tag, "name": name, "path_lower": display.to_lowercase(), "path_display": display })
}

fn read_request(stream: &mut TcpStream) -> Option<Req> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let path = line.split_whitespace().nth(1)?.to_string();
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        reader.read_line(&mut h).ok()?;
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        let (k, v) = h.split_once(':')?;
        headers.push((k.trim().to_string(), v.trim().to_string()));
    }
    let len: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0; len];
    reader.read_exact(&mut body).ok()?;
    Some(Req { path, headers, body })
}

fn form(body: &[u8]) -> BTreeMap<String, String> {
    let dec = |s: &str| {
        let s = s.replace('+', " ");
        let b = s.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%' && i + 2 < b.len() {
                if let Ok(v) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16) {
                    out.push(v);
                    i += 3;
                    continue;
                }
            }
            out.push(b[i]);
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    };
    String::from_utf8_lossy(body).split('&').filter_map(|p| p.split_once('=')).map(|(k, v)| (dec(k), dec(v))).collect()
}

impl Ctx {
    fn serve(&self, mut stream: TcpStream) {
        let Some(req) = read_request(&mut stream) else { return };
        let resp = self.handle(&req);
        let mut head =
            format!("HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n", resp.status, resp.body.len());
        for (k, v) in &resp.headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(&resp.body);
    }

    fn handle(&self, req: &Req) -> Resp {
        let mut st = self.state.lock().unwrap();
        if let Some(i) = st.injected.iter().position(|f| req.path.contains(&f.route)) {
            let f = st.injected.remove(i).unwrap();
            let mut r = Resp { status: f.status, headers: Vec::new(), body: f.body.into_bytes() };
            if let Some(s) = f.retry_after {
                r.headers.push(("Retry-After".into(), s.to_string()));
            }
            return r;
        }
        if req.path == "/oauth2/token" {
            return self.token(&mut st, &form(&req.body));
        }
        let authorized = req
            .header("Authorization")
            .and_then(|h| h.strip_prefix("Bearer "))
            .is_some_and(|t| st.access_tokens.iter().any(|a| a == t));
        if !authorized {
            return json(
                401,
                serde_json::json!({ "error_summary": "expired_access_token/", "error": { ".tag": "expired_access_token" } }),
            );
        }
        let arg: serde_json::Value = match req.header("Dropbox-API-Arg") {
            Some(a) => {
                assert!(a.is_ascii(), "Dropbox-API-Arg must be ASCII: {a}");
                serde_json::from_str(a).expect("Dropbox-API-Arg is JSON")
            }
            None => serde_json::from_slice(&req.body).unwrap_or_default(),
        };
        let path = arg["path"].as_str().unwrap_or_default().to_string();
        let lower = path.to_lowercase();
        match req.path.as_str() {
            "/2/files/upload" => {
                self.uploads.fetch_add(1, Ordering::SeqCst);
                let strict = arg["strict_conflict"].as_bool().unwrap_or(false);
                if st.files.keys().any(|k| k.starts_with(&format!("{lower}/"))) {
                    return endpoint_error("path/conflict/folder/");
                }
                match (arg["mode"].as_str(), st.files.get(&lower)) {
                    (Some("add"), Some((_, data))) if strict || *data != req.body => {
                        return endpoint_error("path/conflict/file/")
                    }
                    // Identical contents: nothing is written, no conflict (without strict_conflict).
                    (Some("add"), Some(_)) => {}
                    (Some("add" | "overwrite"), _) => {
                        let display = st.files.get(&lower).map_or(path.clone(), |(d, _)| d.clone());
                        st.files.insert(lower.clone(), (display, req.body.clone()));
                    }
                    (mode, _) => panic!("unexpected upload mode {mode:?}"),
                }
                let display = st.files[&lower].0.clone();
                json(200, metadata("file", &display))
            }
            "/2/files/download" => match st.files.get(&lower) {
                Some((display, data)) => Resp {
                    status: 200,
                    headers: vec![("Dropbox-API-Result".into(), metadata("file", display).to_string())],
                    body: data.clone(),
                },
                None => endpoint_error("path/not_found/"),
            },
            "/2/files/get_metadata" => match st.files.get(&lower) {
                Some((display, _)) => json(200, metadata("file", display)),
                None if st.files.keys().any(|k| k.starts_with(&format!("{lower}/"))) => {
                    json(200, metadata("folder", &path))
                }
                None => endpoint_error("path/not_found/"),
            },
            "/2/files/list_folder" => {
                let recursive = arg["recursive"].as_bool().unwrap_or(false);
                let limit = arg["limit"].as_u64().unwrap_or(2000) as usize;
                if !lower.is_empty() && !st.files.keys().any(|k| k.starts_with(&format!("{lower}/"))) {
                    return if st.files.contains_key(&lower) {
                        endpoint_error("path/not_folder/")
                    } else {
                        endpoint_error("path/not_found/")
                    };
                }
                let entries = Self::entries(&st, &lower, recursive);
                self.page(entries, 0, limit).with_cursor_prefix(&lower, recursive)
            }
            "/2/files/list_folder/continue" => {
                // The cursor is "<path>|<recursive>|<offset>|<limit>".
                let cursor = arg["cursor"].as_str().unwrap_or_default().to_string();
                let parts: Vec<&str> = cursor.rsplitn(4, '|').collect();
                let [limit, offset, recursive, folder] = parts[..] else {
                    return Resp { status: 400, headers: Vec::new(), body: b"bad cursor".to_vec() };
                };
                let entries = Self::entries(&st, folder, recursive == "true");
                self.page(entries, offset.parse().unwrap_or(0), limit.parse().unwrap_or(2000))
                    .with_cursor_prefix(folder, recursive == "true")
            }
            "/2/users/get_current_account" => {
                json(200, serde_json::json!({ "account_id": "dbid:fake-account", "email": "tester@example.com" }))
            }
            // Ends the app's authorization: its refresh token and every access token stop working.
            "/2/auth/token/revoke" => {
                st.access_tokens.clear();
                st.refresh_tokens.clear();
                json(200, serde_json::Value::Null)
            }
            other => Resp { status: 400, headers: Vec::new(), body: format!("unknown route {other}").into_bytes() },
        }
    }

    /// Entries under `folder` (lower case): files, and the folders their paths imply. In a
    /// shuffled order, as Dropbox gives no order.
    fn entries(st: &State, folder: &str, recursive: bool) -> Vec<serde_json::Value> {
        let mut files = Vec::new();
        let mut folders = BTreeMap::new();
        for (lower, (display, _)) in &st.files {
            if !lower.starts_with(&format!("{folder}/")) {
                continue;
            }
            // Test paths are ASCII, so lower-casing keeps byte offsets.
            let rel = &display[folder.len() + 1..];
            let parts: Vec<&str> = rel.split('/').collect();
            for depth in 1..parts.len() {
                if recursive || depth == 1 {
                    let d = format!("{}/{}", &display[..folder.len()], parts[..depth].join("/"));
                    folders.entry(d.to_lowercase()).or_insert(d);
                }
            }
            if recursive || parts.len() == 1 {
                files.push(metadata("file", display));
            }
        }
        let mut all: Vec<serde_json::Value> = folders.values().map(|d| metadata("folder", d)).collect();
        all.extend(files);
        all.reverse();
        all
    }

    fn page(&self, entries: Vec<serde_json::Value>, offset: usize, limit: usize) -> Resp {
        let size = limit.min(self.page_size.load(Ordering::SeqCst) as usize).max(1);
        let end = (offset + size).min(entries.len());
        let page: Vec<_> = entries.get(offset..end).map(<[_]>::to_vec).unwrap_or_default();
        json(
            200,
            serde_json::json!({ "entries": page, "cursor": format!("{end}|{limit}"), "has_more": end < entries.len() }),
        )
    }

    fn token(&self, st: &mut State, f: &BTreeMap<String, String>) -> Resp {
        let invalid =
            |what: &str| json(400, serde_json::json!({ "error": "invalid_grant", "error_description": what }));
        if f.get("client_id") != Some(&self.app_key) {
            return json(400, serde_json::json!({ "error": "invalid_client", "error_description": "unknown app" }));
        }
        st.issued += 1;
        let access = format!("access-{}", st.issued);
        match f.get("grant_type").map(String::as_str) {
            Some("refresh_token") => {
                if !st.refresh_tokens.iter().any(|t| Some(t) == f.get("refresh_token")) {
                    return invalid("refresh token is invalid or revoked");
                }
                self.refreshes.fetch_add(1, Ordering::SeqCst);
                st.access_tokens.push(access.clone());
                json(200, serde_json::json!({ "access_token": access, "expires_in": 14400, "token_type": "bearer" }))
            }
            Some("authorization_code") => {
                let Some((chal, redirect)) = f.get("code").and_then(|c| st.codes.remove(c)) else {
                    return invalid("code doesn't exist or has expired");
                };
                if challenge(f.get("code_verifier").map_or("", String::as_str)) != chal {
                    return invalid("invalid code verifier");
                }
                if f.get("redirect_uri").map_or("", String::as_str) != redirect {
                    return invalid("redirect_uri mismatch");
                }
                let refresh = format!("refresh-{}", st.issued);
                st.refresh_tokens.push(refresh.clone());
                st.access_tokens.push(access.clone());
                json(
                    200,
                    serde_json::json!({ "access_token": access, "refresh_token": refresh, "expires_in": 14400,
                        "token_type": "bearer", "account_id": "dbid:fake-account" }),
                )
            }
            other => json(400, serde_json::json!({ "error": "unsupported_grant_type", "error_description": other })),
        }
    }
}

impl Resp {
    /// Puts the folder and mode in front of a page's `<offset>|<limit>` cursor.
    fn with_cursor_prefix(mut self, folder: &str, recursive: bool) -> Self {
        if let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(&self.body) {
            if let Some(c) = v["cursor"].as_str() {
                v["cursor"] = format!("{folder}|{recursive}|{c}").into();
                self.body = v.to_string().into_bytes();
            }
        }
        self
    }
}

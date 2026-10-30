use super::fake::{FakeDropbox, Injected};
use super::oauth::{self, SignInError};
use super::*;
use crate::{conformance, Credentials, StaticCredentials};

const KEY: &str = "fakeappkey";

fn remote(fake: &FakeDropbox, uri: &str) -> DropboxRemote {
    let token = fake.issue_refresh_token();
    let creds = Box::new(StaticCredentials(Credentials::OAuthRefresh(token)));
    let mut r = DropboxRemote::with_endpoints(DropboxConfig::parse(uri).unwrap(), creds, fake.endpoints()).unwrap();
    r.max_retry_after = Duration::from_millis(10);
    r
}

#[test]
fn parse_uris() {
    let c = DropboxConfig::parse("dropbox:/saves").unwrap();
    assert_eq!((c.folder.as_str(), c.app_key_param.as_deref()), ("saves", None));
    assert_eq!(c.uri(), "dropbox:/saves");
    assert_eq!(c.path("blobs/ab/cd").unwrap(), "/saves/blobs/ab/cd");
    assert_eq!(c.folder_path("").unwrap(), "/saves");
    assert_eq!(c.key_of("/saves/devices/x/device.json").as_deref(), Some("devices/x/device.json"));
    assert_eq!(c.key_of("/savesx/a"), None);

    let c = DropboxConfig::parse("dropbox:/").unwrap();
    assert_eq!((c.folder.as_str(), c.uri().as_str()), ("", "dropbox:/"));
    assert_eq!(c.path("repo.json").unwrap(), "/repo.json");
    assert_eq!(c.folder_path("").unwrap(), "");
    assert_eq!(c.key_of("/repo.json").as_deref(), Some("repo.json"));

    let c = DropboxConfig::parse("dropbox:My Saves/v2/?app_key=abc123").unwrap();
    assert_eq!(c.uri(), "dropbox:/My Saves/v2?app_key=abc123");
    assert_eq!(c.app_key().unwrap(), "abc123");
    assert_eq!(c.key_of("/my saves/v2/repo.json").as_deref(), Some("repo.json"));

    for bad in [
        "s3://b",
        "dropbox:/a//b",
        "dropbox:/a/../b",
        "dropbox:/s?app_key=",
        "dropbox:/s?app_key=a-b",
        "dropbox:/s?x=1",
    ] {
        assert!(DropboxConfig::parse(bad).is_err(), "{bad}");
    }
    // Keys the core never writes are refused rather than folded together.
    let c = DropboxConfig::parse("dropbox:/s").unwrap();
    for bad in ["", "/a", "a/", "a//b", "Blobs/x", "a\\b"] {
        assert!(c.path(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn header_json_is_ascii() {
    let v = serde_json::json!({ "path": "/sävës/😀" });
    assert_eq!(header_json(&v), r#"{"path":"/s\u00e4v\u00ebs/\ud83d\ude00"}"#);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&header_json(&v)).unwrap(), v);
}

#[test]
fn conformance_against_the_fake() {
    let fake = FakeDropbox::start(KEY);
    for uri in [format!("dropbox:/saves?app_key={KEY}"), format!("dropbox:/?app_key={KEY}")] {
        let r = remote(&fake, &uri);
        let report = conformance::run(&r).unwrap();
        assert!(report.conditional_writes, "strict_conflict makes create-only writes conditional");
    }
}

#[test]
fn identical_uploads_conflict() {
    let fake = FakeDropbox::start(KEY);
    let r = remote(&fake, &format!("dropbox:/s?app_key={KEY}"));
    assert_eq!(r.put("devices/d/log/0000000000000001.ndjson", b"same", true).unwrap(), PutOutcome::Created);
    // Without strict_conflict Dropbox would report success for the same bytes, and a cloned
    // device id would go unnoticed (DESIGN §30.3).
    assert_eq!(r.put("devices/d/log/0000000000000001.ndjson", b"same", true).unwrap(), PutOutcome::AlreadyExists);
    // Overwrite (a renamed device's device.json) replaces.
    r.put("devices/d/device.json", b"one", false).unwrap();
    r.put("devices/d/device.json", b"two", false).unwrap();
    assert_eq!(r.get("devices/d/device.json").unwrap().as_deref(), Some(&b"two"[..]));
    assert!(r.head("devices/d/device.json").unwrap());
    // A folder is not a file.
    assert!(!r.head("devices/d").unwrap());
    assert_eq!(fake.paths(), ["/s/devices/d/device.json", "/s/devices/d/log/0000000000000001.ndjson"]);
}

#[test]
fn listing_pages_and_filters() {
    let fake = FakeDropbox::start(KEY);
    fake.set_page_size(2);
    let r = remote(&fake, &format!("dropbox:/s?app_key={KEY}"));
    for seq in 1..=7u64 {
        r.put(&format!("devices/a/log/{seq:016x}.ndjson"), b"x", true).unwrap();
    }
    for dev in ["c", "b"] {
        r.put(&format!("devices/{dev}/device.json"), b"{}", true).unwrap();
    }
    r.put("blobs/ab/abcd", b"blob", true).unwrap();
    let all = r.list("devices/a/log/", None).unwrap();
    assert_eq!(all.len(), 7);
    assert!(all.windows(2).all(|w| w[0] < w[1]));
    assert_eq!(r.list("devices/a/log/", Some(&all[4])).unwrap(), all[5..]);
    assert_eq!(r.list_dirs("devices/").unwrap(), ["a", "b", "c"]);
    assert_eq!(r.list_dirs("blobs/").unwrap(), ["ab"]);
    // A string prefix that isn't a folder, and a missing folder.
    assert_eq!(r.list("devices/b", None).unwrap(), ["devices/b/device.json"]);
    assert!(r.list("nothing/", None).unwrap().is_empty());
    assert!(r.list_dirs("nothing/").unwrap().is_empty());
}

#[test]
fn tokens_refresh_once_and_revocation_is_auth() {
    let fake = FakeDropbox::start(KEY);
    let r = remote(&fake, &format!("dropbox:/s?app_key={KEY}"));
    r.put("a", b"1", true).unwrap();
    r.get("a").unwrap();
    assert_eq!(fake.refreshes.load(Ordering::SeqCst), 1, "the access token is reused");
    fake.expire_access_tokens();
    assert_eq!(r.get("a").unwrap().as_deref(), Some(&b"1"[..]));
    assert_eq!(fake.refreshes.load(Ordering::SeqCst), 2, "a 401 exchanges the refresh token again");
    fake.revoke_all();
    assert!(matches!(r.get("a"), Err(RemoteError::Auth(_))), "a revoked sign-in asks the user to sign in again");
    // S3 keys are refused.
    let s3 = Credentials::S3(crate::S3Credentials {
        access_key_id: "AK".into(),
        secret_access_key: "S".into(),
        session_token: None,
    });
    let cfg = DropboxConfig::parse(&format!("dropbox:/s?app_key={KEY}")).unwrap();
    let r = DropboxRemote::with_endpoints(cfg, Box::new(StaticCredentials(s3)), fake.endpoints()).unwrap();
    assert!(matches!(r.head("a"), Err(RemoteError::Auth(_))));
}

#[test]
fn rate_limits_and_failures() {
    let fake = FakeDropbox::start(KEY);
    let r = remote(&fake, &format!("dropbox:/s?app_key={KEY}"));
    let limited = |route: &str| Injected {
        route: route.into(),
        status: 429,
        body: r#"{"error_summary":"too_many_write_operations/","error":{".tag":"too_many_write_operations"}}"#.into(),
        retry_after: Some(0),
    };
    // Waits out a rate limit, three times at most.
    fake.inject(limited("upload"));
    assert_eq!(r.put("a", b"1", true).unwrap(), PutOutcome::Created);
    for _ in 0..4 {
        fake.inject(limited("upload"));
    }
    assert!(matches!(r.put("b", b"1", true), Err(RemoteError::Network(_))));
    // Server errors are retried, then reported as offline.
    let server_error = Injected { route: "download".into(), status: 500, body: "oops".into(), retry_after: None };
    fake.inject(server_error.clone());
    assert!(r.get("a").unwrap().is_some());
    for _ in 0..3 {
        fake.inject(server_error.clone());
    }
    assert!(r.get("a").unwrap_err().is_offline());
    // A full account.
    fake.inject(Injected {
        route: "upload".into(),
        status: 409,
        body: r#"{"error_summary":"path/insufficient_space/..","error":{".tag":"path"}}"#.into(),
        retry_after: None,
    });
    let e = r.put("c", b"1", true).unwrap_err();
    assert!(e.to_string().contains("Dropbox account is full"), "{e}");
    // A bad request is a bug, never retried.
    fake.inject(Injected { route: "get_metadata".into(), status: 400, body: "bad".into(), retry_after: None });
    assert!(matches!(r.head("a"), Err(RemoteError::Http { status: 400, .. })));
    // Unreachable.
    let cfg = DropboxConfig::parse(&format!("dropbox:/s?app_key={KEY}")).unwrap();
    let dead =
        DropboxEndpoints { api: "http://127.0.0.1:1".into(), content: "http://127.0.0.1:1".into(), ..fake.endpoints() };
    let creds = Box::new(StaticCredentials(Credentials::Bearer("t".into())));
    let r = DropboxRemote::with_endpoints(cfg, creds, dead).unwrap();
    assert!(r.head("a").unwrap_err().is_offline());
}

#[test]
fn sign_in_end_to_end() {
    let fake = FakeDropbox::start(KEY);
    let ep = fake.endpoints();
    let uri = format!("dropbox:/saves?app_key={KEY}");
    let redirect = "http://127.0.0.1:53682/dropbox";
    let s = oauth::start_with(&ep, &uri, redirect).unwrap();
    let code = fake.approve(&s.url);
    let state = s.url.split("state=").nth(1).unwrap().split('&').next().unwrap().to_string();

    // A redirect for another sign-in, and one the user declined, are refused.
    let other = format!("{redirect}?code={code}&state=someone-else");
    assert!(matches!(oauth::finish_with(&ep, &s.pending, &other), Err(SignInError::Declined(_))));
    let declined = format!("{redirect}?error=access_denied&error_description=no&state={state}");
    assert!(matches!(oauth::finish_with(&ep, &s.pending, &declined), Err(SignInError::Declined(_))));

    let signed = oauth::finish_with(&ep, &s.pending, &format!("{redirect}?code={code}&state={state}")).unwrap();
    assert_eq!((signed.account_id.as_str(), signed.email.as_str()), ("dbid:fake-account", "tester@example.com"));
    assert!(!format!("{signed:?}").contains(&signed.refresh_token));
    // A code works once.
    let again = format!("{redirect}?code={code}&state={state}");
    assert!(matches!(oauth::finish_with(&ep, &s.pending, &again), Err(SignInError::Declined(_))));

    // The refresh token works with the remote.
    let creds = Box::new(StaticCredentials(Credentials::OAuthRefresh(signed.refresh_token.clone())));
    let r = DropboxRemote::with_endpoints(DropboxConfig::parse(&uri).unwrap(), creds, ep.clone()).unwrap();
    r.put("repo.json", b"{}", true).unwrap();

    // Paste-the-code: no redirect URI, and the bare code.
    let s = oauth::start_with(&ep, &uri, "").unwrap();
    let code = fake.approve(&s.url);
    assert!(oauth::finish_with(&ep, &s.pending, &format!("  {code}\n")).is_ok());
    // A wrong verifier (another sign-in's pending) is refused.
    let s2 = oauth::start_with(&ep, &uri, "").unwrap();
    let code = fake.approve(&s2.url);
    assert!(matches!(oauth::finish_with(&ep, &s.pending, &code), Err(SignInError::Declined(_))));

    // Sign-out revokes; revoking again is fine.
    oauth::revoke_with(&ep, &uri, &signed.refresh_token).unwrap();
    oauth::revoke_with(&ep, &uri, &signed.refresh_token).unwrap();
    assert!(matches!(r.head("repo.json"), Err(RemoteError::Auth(_))));
}

#[test]
fn another_app_key_is_another_app() {
    let fake = FakeDropbox::start(KEY);
    let token = fake.issue_refresh_token();
    let cfg = DropboxConfig::parse("dropbox:/s?app_key=forkkey").unwrap();
    let r = DropboxRemote::with_endpoints(
        cfg,
        Box::new(StaticCredentials(Credentials::OAuthRefresh(token))),
        fake.endpoints(),
    )
    .unwrap();
    // A refresh token works only with the app it was issued to.
    assert!(matches!(r.head("a"), Err(RemoteError::Auth(_))));
}

use std::sync::atomic::Ordering;

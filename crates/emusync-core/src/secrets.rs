//! Secrets of the host application (S3 access keys, an encryption passphrase) in the operating
//! system's credential store, so desktop emulators don't each have to write their own
//! (DESIGN §24):
//!
//! - Windows: Credential Manager (a generic credential of the current user).
//! - macOS: the user's Keychain (a generic password).
//! - Linux: the Secret Service (GNOME Keyring, KWallet) over D-Bus. Where none is running (a
//!   headless machine, the Steam Deck in game mode), a file only the user can read in the
//!   host's fallback directory: the same protection as repository keys get there (§22).
//!
//! A secret is named by the host (`service`, e.g. `PCSX2`) and the setting (`name`, e.g.
//! `s3-secret-access-key`); that is also how it appears in Keychain Access or Credential
//! Manager. Android and iOS hosts keep secrets in their platform keystore from Kotlin or Swift,
//! so there every call fails with [`Error::InvalidArg`].
//!
//! Calls may block: the system can ask the user to unlock the keychain or keyring first.
//!
//! **While the Secret Service is away.** A secret set or deleted while it can't be reached goes
//! to (or is removed from) the fallback file, and the store's own copy, which can't be touched
//! then, is marked stale (`<dir>/secrets/.stale/<service>/<name>`). The first read that finds the
//! store again deletes that copy and answers from the file, so an old value never comes back. A
//! read while the store is away answers from the file; without a fallback directory it is an
//! error, never "no secret".

use crate::error::{Error, Result};
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

/// Where a secret was stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Store {
    CredentialManager,
    Keychain,
    SecretService,
    /// The fallback file (Linux without a Secret Service).
    File,
}

/// The longest secret accepted, in bytes. Credential Manager holds at most 2,560 bytes.
pub const MAX_SECRET_LEN: usize = 1024;

/// Whether a store that is not available falls back to the file. Only Linux: Credential Manager
/// and the Keychain are always there, and failing to reach them is an error worth reporting.
const FILE_FALLBACK: bool = cfg!(target_os = "linux");

/// Stores `value` as the secret `service`/`name`, replacing an earlier one. `fallback_dir` is
/// where the fallback file goes when the platform needs one (see the module docs); without
/// it, that case is an error.
pub fn set(service: &str, name: &str, value: &str, fallback_dir: Option<&Path>) -> Result<Store> {
    set_with(&os::System, FILE_FALLBACK, service, name, value, fallback_dir)
}

/// The secret `service`/`name`, or `None` if there is none.
pub fn get(service: &str, name: &str, fallback_dir: Option<&Path>) -> Result<Option<Zeroizing<String>>> {
    get_with(&os::System, FILE_FALLBACK, service, name, fallback_dir)
}

/// Deletes the secret `service`/`name` from the store and the fallback file. Not an error if
/// there is none.
pub fn delete(service: &str, name: &str, fallback_dir: Option<&Path>) -> Result<()> {
    delete_with(&os::System, FILE_FALLBACK, service, name, fallback_dir)
}

/// A credential store: the system's ([`os::System`]), or an in-memory one in the tests.
trait Backend {
    fn set(&self, service: &str, name: &str, value: &str) -> Result<Os<()>>;
    fn get(&self, service: &str, name: &str) -> Result<Os<Option<Zeroizing<String>>>>;
    fn delete(&self, service: &str, name: &str) -> Result<Os<()>>;
}

/// The outcome of a call to a store.
#[cfg_attr(not(any(windows, target_os = "macos", target_os = "linux")), allow(dead_code))]
enum Os<T> {
    Done(T, Store),
    /// No store to talk to (no D-Bus session, no Secret Service, a locked or denied keychain).
    Unavailable(String),
}

/// The fallback directory, where this platform has the fallback.
fn fallback(dir: Option<&Path>, file_fallback: bool) -> Option<&Path> {
    dir.filter(|_| file_fallback)
}

fn set_with(
    b: &dyn Backend,
    file_fallback: bool,
    service: &str,
    name: &str,
    value: &str,
    dir: Option<&Path>,
) -> Result<Store> {
    check(service, name)?;
    if value.is_empty() || value.len() > MAX_SECRET_LEN || value.contains('\0') {
        return Err(Error::InvalidArg(format!("a secret must be 1 to {MAX_SECRET_LEN} bytes, without NUL characters")));
    }
    match b.set(service, name, value)? {
        Os::Done((), store) => {
            // The store has the current value now: a file copy and a stale mark are obsolete.
            if let Some(d) = dir {
                file::delete(d, service, name)?;
                stale::clear(d, service, name)?;
            }
            Ok(store)
        }
        Os::Unavailable(why) => match fallback(dir, file_fallback) {
            Some(d) => {
                log::info!("no credential store ({why}); keeping {service}/{name} in a private file");
                // Marked first: a crash in between then loses the new value, rather than leaving
                // the store's old one to be read back.
                stale::mark(d, service, name)?;
                file::set(d, service, name, value)?;
                Ok(Store::File)
            }
            None => Err(unavailable(&why)),
        },
    }
}

fn get_with(
    b: &dyn Backend,
    file_fallback: bool,
    service: &str,
    name: &str,
    dir: Option<&Path>,
) -> Result<Option<Zeroizing<String>>> {
    check(service, name)?;
    let fb = fallback(dir, file_fallback);
    match b.get(service, name)? {
        Os::Done(v, _) => {
            if let Some(d) = fb {
                if stale::is_marked(d, service, name) {
                    // Set or deleted while the store was away: its copy is out of date.
                    if let Os::Done((), _) = b.delete(service, name)? {
                        stale::clear(d, service, name)?;
                    }
                    return file::get(d, service, name);
                }
            }
            match (v, fb) {
                (Some(v), _) => Ok(Some(v)),
                (None, Some(d)) => file::get(d, service, name),
                (None, None) => Ok(None),
            }
        }
        // A store that can't be reached says nothing about whether there is a secret: without
        // the file to answer instead, that is an error, not "no secret".
        Os::Unavailable(why) => match fb {
            Some(d) => file::get(d, service, name),
            None => Err(unavailable(&why)),
        },
    }
}

fn delete_with(b: &dyn Backend, file_fallback: bool, service: &str, name: &str, dir: Option<&Path>) -> Result<()> {
    check(service, name)?;
    match b.delete(service, name)? {
        Os::Done((), _) => {
            if let Some(d) = dir {
                file::delete(d, service, name)?;
                stale::clear(d, service, name)?;
            }
            Ok(())
        }
        // The store's copy can't be deleted now: mark it, so the first read that reaches the
        // store again deletes it instead of returning it.
        Os::Unavailable(why) => match fallback(dir, file_fallback) {
            Some(d) => {
                stale::mark(d, service, name)?;
                file::delete(d, service, name)
            }
            None => Err(unavailable(&why)),
        },
    }
}

fn unavailable(why: &str) -> Error {
    Error::Io(std::io::Error::other(format!("the system's credential store is not available: {why}")))
}

/// Service and name become file names in the fallback, and stay readable in the system's UI.
fn check(service: &str, name: &str) -> Result<()> {
    for (what, s) in [("service", service), ("name", name)] {
        let ok = (1..=64).contains(&s.len())
            && !s.starts_with('.')
            && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._- ".contains(&b));
        if !ok {
            return Err(Error::InvalidArg(format!(
                "secret {what} {s:?}: 1 to 64 letters, digits, spaces, '.', '_' or '-', not starting with '.'"
            )));
        }
    }
    Ok(())
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
mod os {
    use super::{Backend, Os, Store};
    use crate::error::{Error, Result};
    use zeroize::Zeroizing;

    const STORE: Store = if cfg!(windows) {
        Store::CredentialManager
    } else if cfg!(target_os = "macos") {
        Store::Keychain
    } else {
        Store::SecretService
    };

    fn run<T>(r: keyring::Result<T>, missing: impl FnOnce() -> T) -> Result<Os<T>> {
        match r {
            Ok(v) => Ok(Os::Done(v, STORE)),
            Err(keyring::Error::NoEntry) => Ok(Os::Done(missing(), STORE)),
            Err(keyring::Error::NoStorageAccess(e)) | Err(keyring::Error::PlatformFailure(e)) => {
                Ok(Os::Unavailable(e.to_string()))
            }
            Err(e) => Err(Error::Io(std::io::Error::other(format!("credential store: {e}")))),
        }
    }

    fn entry(service: &str, name: &str) -> Result<Os<keyring::Entry>> {
        run(keyring::Entry::new(service, name), || unreachable!("Entry::new reports no NoEntry"))
    }

    /// The system's credential store, through `keyring`.
    pub(super) struct System;

    impl Backend for System {
        fn set(&self, service: &str, name: &str, value: &str) -> Result<Os<()>> {
            Ok(match entry(service, name)? {
                Os::Done(e, _) => run(e.set_password(value), || ())?,
                Os::Unavailable(why) => Os::Unavailable(why),
            })
        }

        fn get(&self, service: &str, name: &str) -> Result<Os<Option<Zeroizing<String>>>> {
            Ok(match entry(service, name)? {
                Os::Done(e, _) => run(e.get_password().map(|v| Some(Zeroizing::new(v))), || None)?,
                Os::Unavailable(why) => Os::Unavailable(why),
            })
        }

        fn delete(&self, service: &str, name: &str) -> Result<Os<()>> {
            Ok(match entry(service, name)? {
                Os::Done(e, _) => run(e.delete_credential(), || ())?,
                Os::Unavailable(why) => Os::Unavailable(why),
            })
        }
    }
}

/// Android, iOS and the rest: the host's own keystore.
#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
mod os {
    use super::{Backend, Os};
    use crate::error::{Error, Result};
    use zeroize::Zeroizing;

    pub(super) struct System;

    fn unsupported<T>() -> Result<Os<T>> {
        Err(Error::InvalidArg("secrets are not stored by emusync on this platform: use the platform keystore".into()))
    }

    impl Backend for System {
        fn set(&self, _: &str, _: &str, _: &str) -> Result<Os<()>> {
            unsupported()
        }
        fn get(&self, _: &str, _: &str) -> Result<Os<Option<Zeroizing<String>>>> {
            unsupported()
        }
        fn delete(&self, _: &str, _: &str) -> Result<Os<()>> {
            unsupported()
        }
    }
}

/// The fallback: `<dir>/secrets/<service>/<name>`, holding the value, readable only by its owner.
mod file {
    use super::*;
    use std::fs;
    use std::io::ErrorKind;

    fn path(dir: &Path, service: &str, name: &str) -> PathBuf {
        dir.join("secrets").join(service).join(name)
    }

    pub(super) fn set(dir: &Path, service: &str, name: &str, value: &str) -> Result<()> {
        Ok(crate::fsutil::write_private(&path(dir, service, name), value.as_bytes())?)
    }

    pub(super) fn get(dir: &Path, service: &str, name: &str) -> Result<Option<Zeroizing<String>>> {
        let p = path(dir, service, name);
        match fs::read(&p) {
            Ok(bytes) => {
                let bytes = Zeroizing::new(bytes);
                match std::str::from_utf8(&bytes) {
                    Ok(s) => Ok(Some(Zeroizing::new(s.to_owned()))),
                    Err(_) => Err(Error::Integrity(format!("{}: not UTF-8", p.display()))),
                }
            }
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub(super) fn delete(dir: &Path, service: &str, name: &str) -> Result<()> {
        match fs::remove_file(path(dir, service, name)) {
            Err(e) if e.kind() != ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }
}

/// Marks of secrets whose copy in the store is out of date: `<dir>/secrets/.stale/<service>/
/// <name>`, empty files. `.stale` can't collide with a service, which can't start with a dot.
mod stale {
    use super::*;
    use std::fs;
    use std::io::ErrorKind;

    fn path(dir: &Path, service: &str, name: &str) -> PathBuf {
        dir.join("secrets").join(".stale").join(service).join(name)
    }

    pub(super) fn mark(dir: &Path, service: &str, name: &str) -> Result<()> {
        Ok(crate::fsutil::write_private(&path(dir, service, name), b"")?)
    }

    pub(super) fn is_marked(dir: &Path, service: &str, name: &str) -> bool {
        path(dir, service, name).exists()
    }

    pub(super) fn clear(dir: &Path, service: &str, name: &str) -> Result<()> {
        match fs::remove_file(path(dir, service, name)) {
            Err(e) if e.kind() != ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;

    /// An in-memory store that can be switched off, like a Secret Service that goes away.
    struct Fake {
        up: Cell<bool>,
        map: RefCell<HashMap<(String, String), String>>,
    }

    impl Fake {
        fn new() -> Fake {
            Fake { up: Cell::new(true), map: RefCell::default() }
        }
        fn has(&self, name: &str) -> Option<String> {
            self.map.borrow().get(&("svc".to_string(), name.to_string())).cloned()
        }
    }

    impl Backend for Fake {
        fn set(&self, service: &str, name: &str, value: &str) -> Result<Os<()>> {
            if !self.up.get() {
                return Ok(Os::Unavailable("down".into()));
            }
            self.map.borrow_mut().insert((service.into(), name.into()), value.into());
            Ok(Os::Done((), Store::SecretService))
        }
        fn get(&self, service: &str, name: &str) -> Result<Os<Option<Zeroizing<String>>>> {
            if !self.up.get() {
                return Ok(Os::Unavailable("down".into()));
            }
            let v = self.map.borrow().get(&(service.into(), name.into())).cloned().map(Zeroizing::new);
            Ok(Os::Done(v, Store::SecretService))
        }
        fn delete(&self, service: &str, name: &str) -> Result<Os<()>> {
            if !self.up.get() {
                return Ok(Os::Unavailable("down".into()));
            }
            self.map.borrow_mut().remove(&(service.into(), name.into()));
            Ok(Os::Done((), Store::SecretService))
        }
    }

    fn value(r: Result<Option<Zeroizing<String>>>) -> Option<String> {
        r.unwrap().map(|v| v.to_string())
    }

    #[test]
    fn names_are_checked() {
        assert!(check("PCSX2", "s3-secret-access-key").is_ok());
        assert!(check("Dolphin Emulator", "passphrase").is_ok());
        for (s, n) in [("", "x"), ("x", ""), ("../x", "y"), ("x", "a/b"), (".hidden", "y"), ("x", "a\\b")] {
            assert!(matches!(check(s, n), Err(Error::InvalidArg(_))), "{s:?} {n:?}");
        }
        assert!(matches!(check(&"x".repeat(65), "y"), Err(Error::InvalidArg(_))));
    }

    #[test]
    fn values_are_checked() {
        let dir = tempfile::tempdir().unwrap();
        let b = Fake::new();
        for v in ["", "a\0b"] {
            assert!(matches!(set_with(&b, true, "svc", "v", v, Some(dir.path())), Err(Error::InvalidArg(_))));
        }
        let long = "x".repeat(MAX_SECRET_LEN + 1);
        assert!(matches!(set_with(&b, true, "svc", "v", &long, Some(dir.path())), Err(Error::InvalidArg(_))));
    }

    #[test]
    fn the_store_is_used_while_it_is_up() {
        let dir = tempfile::tempdir().unwrap();
        let (b, d) = (Fake::new(), Some(dir.path()));
        assert_eq!(set_with(&b, true, "svc", "k", "one", d).unwrap(), Store::SecretService);
        assert_eq!(value(get_with(&b, true, "svc", "k", d)), Some("one".into()));
        assert!(!dir.path().join("secrets/svc/k").exists(), "nothing in the file while the store is up");
        delete_with(&b, true, "svc", "k", d).unwrap();
        assert_eq!(value(get_with(&b, true, "svc", "k", d)), None);
        assert_eq!(b.has("k"), None);
    }

    /// Codex review on PR 16: a store that can't be reached is not "no secret".
    #[test]
    fn an_unreachable_store_is_an_error_without_a_fallback() {
        let b = Fake::new();
        set_with(&b, true, "svc", "k", "one", None).unwrap();
        b.up.set(false);
        // Linux without a fallback directory, and Windows/macOS with or without one.
        let dir = tempfile::tempdir().unwrap();
        for (fb, d) in [(true, None), (false, None), (false, Some(dir.path()))] {
            assert!(matches!(get_with(&b, fb, "svc", "k", d), Err(Error::Io(_))), "get {fb} {d:?}");
            assert!(matches!(set_with(&b, fb, "svc", "k", "two", d), Err(Error::Io(_))), "set {fb} {d:?}");
            assert!(matches!(delete_with(&b, fb, "svc", "k", d), Err(Error::Io(_))), "delete {fb} {d:?}");
        }
        assert_eq!(b.has("k"), Some("one".into()), "nothing changed");
    }

    /// Set while the store is away: the file holds the new value, and when the store is back
    /// its old copy is not read back.
    #[test]
    fn a_value_set_while_the_store_is_away_wins_when_it_is_back() {
        let dir = tempfile::tempdir().unwrap();
        let (b, d) = (Fake::new(), Some(dir.path()));
        set_with(&b, true, "svc", "k", "old", d).unwrap();
        b.up.set(false);
        assert_eq!(set_with(&b, true, "svc", "k", "new", d).unwrap(), Store::File);
        assert_eq!(value(get_with(&b, true, "svc", "k", d)), Some("new".into()));
        b.up.set(true);
        assert_eq!(value(get_with(&b, true, "svc", "k", d)), Some("new".into()));
        assert_eq!(b.has("k"), None, "the store's out-of-date copy is deleted");
        assert_eq!(value(get_with(&b, true, "svc", "k", d)), Some("new".into()));
        // Storing again moves it into the store.
        assert_eq!(set_with(&b, true, "svc", "k", "newer", d).unwrap(), Store::SecretService);
        assert!(!dir.path().join("secrets/svc/k").exists());
        assert_eq!(value(get_with(&b, true, "svc", "k", d)), Some("newer".into()));
    }

    /// Codex review on PR 16: a deletion while the store is away must not let the store's copy
    /// come back.
    #[test]
    fn a_secret_deleted_while_the_store_is_away_stays_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let (b, d) = (Fake::new(), Some(dir.path()));
        set_with(&b, true, "svc", "k", "secret", d).unwrap();
        b.up.set(false);
        delete_with(&b, true, "svc", "k", d).unwrap();
        assert_eq!(value(get_with(&b, true, "svc", "k", d)), None);
        b.up.set(true);
        assert_eq!(b.has("k"), Some("secret".into()), "still in the store until it is reached");
        assert_eq!(value(get_with(&b, true, "svc", "k", d)), None);
        assert_eq!(b.has("k"), None, "deleted on the first read that reached the store");
        assert!(!stale::is_marked(dir.path(), "svc", "k"));
    }

    #[test]
    fn the_fallback_file_round_trips_and_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        assert_eq!(file::get(d, "PCSX2", "secret").unwrap(), None);
        file::set(d, "PCSX2", "secret", "first value").unwrap();
        file::set(d, "PCSX2", "secret", "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY").unwrap();
        assert_eq!(
            file::get(d, "PCSX2", "secret").unwrap().as_deref().map(|s| s.as_str()),
            Some("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY")
        );
        assert_eq!(file::get(d, "PCSX2", "other").unwrap(), None);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let p = d.join("secrets/PCSX2/secret");
            assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::metadata(p.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        }
        file::delete(d, "PCSX2", "secret").unwrap();
        file::delete(d, "PCSX2", "secret").unwrap();
        assert_eq!(file::get(d, "PCSX2", "secret").unwrap(), None);
    }

    /// Writes to the real store of this machine (Credential Manager, the Keychain, the Secret
    /// Service), so it runs only when asked: EMUSYNC_TEST_OS_SECRETS=1 (CI sets it). Linux
    /// runners have no Secret Service, which tests the fallback instead.
    #[test]
    fn the_system_store_round_trips() {
        if std::env::var_os("EMUSYNC_TEST_OS_SECRETS").is_none() {
            eprintln!("skipped: set EMUSYNC_TEST_OS_SECRETS=1 to use this machine's credential store");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let d = Some(dir.path());
        let name = format!("test-{}", uuid::Uuid::now_v7());
        let service = "emusync-test";
        assert_eq!(get(service, &name, d).unwrap(), None);
        let store = set(service, &name, "s3cr3t value", d).unwrap();
        eprintln!("stored in {store:?}");
        if cfg!(windows) {
            assert_eq!(store, Store::CredentialManager);
        } else if cfg!(target_os = "macos") {
            assert_eq!(store, Store::Keychain);
        }
        let check_value = |want: &str| {
            assert_eq!(get(service, &name, d).unwrap().as_deref().map(|s| s.as_str()), Some(want));
        };
        check_value("s3cr3t value");
        set(service, &name, "replaced", d).unwrap();
        check_value("replaced");
        delete(service, &name, d).unwrap();
        assert_eq!(get(service, &name, d).unwrap(), None);
        delete(service, &name, d).unwrap();
    }
}

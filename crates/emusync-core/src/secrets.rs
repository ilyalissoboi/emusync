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
    check(service, name)?;
    if value.is_empty() || value.len() > MAX_SECRET_LEN || value.contains('\0') {
        return Err(Error::InvalidArg(format!("a secret must be 1 to {MAX_SECRET_LEN} bytes, without NUL characters")));
    }
    match os::set(service, name, value)? {
        Os::Done((), store) => {
            // A copy kept while the store was unavailable is stale now.
            if let Some(dir) = fallback_dir {
                file::delete(dir, service, name)?;
            }
            Ok(store)
        }
        Os::Unavailable(why) => match fallback_dir {
            Some(dir) if FILE_FALLBACK => {
                log::info!("no credential store ({why}); keeping {service}/{name} in a private file");
                file::set(dir, service, name, value)?;
                Ok(Store::File)
            }
            _ => Err(unavailable(&why)),
        },
    }
}

/// The secret `service`/`name`, or `None` if there is none.
pub fn get(service: &str, name: &str, fallback_dir: Option<&Path>) -> Result<Option<Zeroizing<String>>> {
    check(service, name)?;
    let from_file = || match fallback_dir {
        Some(dir) if FILE_FALLBACK => file::get(dir, service, name),
        _ => Ok(None),
    };
    match os::get(service, name)? {
        Os::Done(Some(v), _) => Ok(Some(v)),
        // Stored in the file while the store was unavailable, and not moved since.
        Os::Done(None, _) => from_file(),
        Os::Unavailable(why) => {
            let v = from_file()?;
            if v.is_none() && !FILE_FALLBACK {
                return Err(unavailable(&why));
            }
            Ok(v)
        }
    }
}

/// Deletes the secret `service`/`name` from the store and the fallback file. Not an error if
/// there is none.
pub fn delete(service: &str, name: &str, fallback_dir: Option<&Path>) -> Result<()> {
    check(service, name)?;
    if let Some(dir) = fallback_dir {
        file::delete(dir, service, name)?;
    }
    match os::delete(service, name)? {
        Os::Done((), _) => Ok(()),
        Os::Unavailable(_) if FILE_FALLBACK => Ok(()),
        Os::Unavailable(why) => Err(unavailable(&why)),
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

/// The outcome of a call to the system's store.
enum Os<T> {
    Done(T, Store),
    /// No store to talk to (no D-Bus session, no Secret Service, a locked or denied keychain).
    Unavailable(String),
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
mod os {
    use super::{Os, Store};
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

    pub(super) fn set(service: &str, name: &str, value: &str) -> Result<Os<()>> {
        Ok(match entry(service, name)? {
            Os::Done(e, _) => run(e.set_password(value), || ())?,
            Os::Unavailable(why) => Os::Unavailable(why),
        })
    }

    pub(super) fn get(service: &str, name: &str) -> Result<Os<Option<Zeroizing<String>>>> {
        Ok(match entry(service, name)? {
            Os::Done(e, _) => run(e.get_password().map(|v| Some(Zeroizing::new(v))), || None)?,
            Os::Unavailable(why) => Os::Unavailable(why),
        })
    }

    pub(super) fn delete(service: &str, name: &str) -> Result<Os<()>> {
        Ok(match entry(service, name)? {
            Os::Done(e, _) => run(e.delete_credential(), || ())?,
            Os::Unavailable(why) => Os::Unavailable(why),
        })
    }
}

/// Android, iOS and the rest: the host's own keystore.
#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
mod os {
    use super::Os;
    use crate::error::{Error, Result};
    use zeroize::Zeroizing;

    fn unsupported<T>() -> Result<Os<T>> {
        Err(Error::InvalidArg("secrets are not stored by emusync on this platform: use the platform keystore".into()))
    }
    pub(super) fn set(_: &str, _: &str, _: &str) -> Result<Os<()>> {
        unsupported()
    }
    pub(super) fn get(_: &str, _: &str) -> Result<Os<Option<Zeroizing<String>>>> {
        unsupported()
    }
    pub(super) fn delete(_: &str, _: &str) -> Result<Os<()>> {
        unsupported()
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

#[cfg(test)]
mod tests {
    use super::*;

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
        for v in ["", "a\0b"] {
            assert!(matches!(set("emusync-test", "v", v, Some(dir.path())), Err(Error::InvalidArg(_))));
        }
        let long = "x".repeat(MAX_SECRET_LEN + 1);
        assert!(matches!(set("emusync-test", "v", &long, Some(dir.path())), Err(Error::InvalidArg(_))));
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

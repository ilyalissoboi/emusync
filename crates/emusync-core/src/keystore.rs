//! This device's copy of a repository key (DESIGN §22), so the passphrase is needed once per
//! data directory.
//!
//! The key lives in `<data_dir>/keys/<repo_id>.key`:
//! - on Windows, protected with DPAPI (`CryptProtectData`): only the same Windows user on the
//!   same machine can decrypt it, and a copy of the data directory is useless elsewhere;
//! - elsewhere, in a file only its owner can read (mode 0600, in a 0700 directory). On Android
//!   and iOS the data directory is the app's private storage, which the system already keeps
//!   from other apps. On desktop Linux and macOS this is the documented fallback: anyone who
//!   can read the user's files can read the key, as they could read the saves themselves.

use crate::crypto::RepoKey;
use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

#[derive(Serialize, Deserialize)]
struct KeyFile {
    repo_id: String,
    /// `dpapi` or `none`.
    protection: String,
    /// Hex: the key, or its DPAPI blob.
    key: String,
}

fn path(data_dir: &Path, repo_id: &str) -> Result<PathBuf> {
    // repo_id comes from repo.json; it becomes a file name only if it is a UUID.
    if uuid::Uuid::parse_str(repo_id).is_err() {
        return Err(Error::Incompatible(format!("repo.json: repository id {repo_id:?} is not a UUID")));
    }
    Ok(data_dir.join("keys").join(format!("{repo_id}.key")))
}

/// The stored key for `repo_id`, if there is one this device can read.
pub(crate) fn load(data_dir: &Path, repo_id: &str) -> Result<Option<RepoKey>> {
    let p = path(data_dir, repo_id)?;
    let body = match fs::read(&p) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let unreadable = |why: &str| {
        log::warn!("{}: {why}; the repository has to be unlocked again", p.display());
        Ok(None)
    };
    let Ok(file) = serde_json::from_slice::<KeyFile>(&body) else { return unreadable("not a key file") };
    if file.repo_id != repo_id {
        return unreadable("the key belongs to another repository");
    }
    let Ok(blob) = hex::decode(&file.key).map(Zeroizing::new) else { return unreadable("malformed key") };
    let plain = match file.protection.as_str() {
        "none" => blob,
        #[cfg(windows)]
        "dpapi" => match dpapi::unprotect(&blob, repo_id) {
            Some(k) => k,
            None => return unreadable("DPAPI cannot decrypt the key (another Windows user or machine)"),
        },
        _ => return unreadable("unsupported key protection"),
    };
    match RepoKey::from_bytes(&plain) {
        Some(k) => Ok(Some(k)),
        None => unreadable("the key has the wrong size"),
    }
}

/// Stores `key` for `repo_id`, replacing any earlier copy.
pub(crate) fn save(data_dir: &Path, repo_id: &str, key: &RepoKey) -> Result<()> {
    let p = path(data_dir, repo_id)?;
    #[cfg(windows)]
    let (protection, blob) = ("dpapi", dpapi::protect(key.as_bytes(), repo_id)?);
    #[cfg(not(windows))]
    let (protection, blob) = ("none", Zeroizing::new(key.as_bytes().to_vec()));
    let body = Zeroizing::new(
        serde_json::to_vec(&KeyFile {
            repo_id: repo_id.into(),
            protection: protection.into(),
            key: hex::encode(&*blob),
        })
        .expect("key file serializes"),
    );
    let dir = p.parent().expect("key path has a parent");
    fs::create_dir_all(dir)?;
    let tmp = crate::fsutil::temp_path(&p);
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        opts.mode(0o600);
    }
    let result = (|| {
        let mut f = opts.open(&tmp)?;
        f.write_all(&body)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, &p)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    Ok(result?)
}

/// Deletes the stored key for `repo_id`, if any.
pub(crate) fn remove(data_dir: &Path, repo_id: &str) -> Result<()> {
    match fs::remove_file(path(data_dir, repo_id)?) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

#[cfg(windows)]
mod dpapi {
    use crate::error::{Error, Result};
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    use zeroize::{Zeroize, Zeroizing};

    fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 }
    }

    /// Copies DPAPI's output and frees it (wiping it first).
    unsafe fn take(out: CRYPT_INTEGER_BLOB) -> Zeroizing<Vec<u8>> {
        let s = std::slice::from_raw_parts_mut(out.pbData, out.cbData as usize);
        let v = Zeroizing::new(s.to_vec());
        s.zeroize();
        LocalFree(out.pbData.cast());
        v
    }

    /// The repository id is DPAPI's "optional entropy", so a key file can't be passed off
    /// as another repository's.
    pub fn protect(data: &[u8], repo_id: &str) -> Result<Zeroizing<Vec<u8>>> {
        let input = blob(data);
        let entropy = blob(repo_id.as_bytes());
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: null_mut() };
        let description: Vec<u16> = "emusync repository key\0".encode_utf16().collect();
        // SAFETY: every pointer is valid for the call; `out` is freed by `take`.
        let ok = unsafe {
            CryptProtectData(
                &input,
                description.as_ptr(),
                &entropy,
                null(),
                null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if ok == 0 {
            return Err(Error::Io(std::io::Error::last_os_error()));
        }
        Ok(unsafe { take(out) })
    }

    pub fn unprotect(data: &[u8], repo_id: &str) -> Option<Zeroizing<Vec<u8>>> {
        let input = blob(data);
        let entropy = blob(repo_id.as_bytes());
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: null_mut() };
        // SAFETY: as above.
        let ok = unsafe {
            CryptUnprotectData(&input, null_mut(), &entropy, null(), null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
        };
        (ok != 0).then(|| unsafe { take(out) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_load() {
        let dir = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::now_v7().to_string();
        assert!(load(dir.path(), &id).unwrap().is_none());
        let key = RepoKey::generate();
        save(dir.path(), &id, &key).unwrap();
        assert_eq!(load(dir.path(), &id).unwrap().unwrap().as_bytes(), key.as_bytes());
        let body = fs::read_to_string(path(dir.path(), &id).unwrap()).unwrap();
        if cfg!(windows) {
            // DPAPI-protected: the raw key isn't in the file.
            assert!(body.contains("\"dpapi\""));
            assert!(!body.contains(&hex::encode(key.as_bytes())));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(path(dir.path(), &id).unwrap()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Copied under another repository's name: refused, not used.
        let other = uuid::Uuid::now_v7().to_string();
        fs::copy(path(dir.path(), &id).unwrap(), path(dir.path(), &other).unwrap()).unwrap();
        assert!(load(dir.path(), &other).unwrap().is_none());
        assert!(load(dir.path(), "../evil").is_err());
    }
}

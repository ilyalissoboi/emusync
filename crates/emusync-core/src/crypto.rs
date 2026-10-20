//! End-to-end encryption (DESIGN §7, §22).
//!
//! A repository created with a passphrase has a random 256-bit **repository key**. `repo.json`
//! holds it wrapped (XChaCha20-Poly1305) with a key derived from the passphrase by Argon2id, so
//! any device that knows the passphrase can unwrap it, and the storage provider can't. From the
//! repository key two subkeys are derived with BLAKE3's key derivation mode:
//!
//! - the **data key** encrypts every object a device writes besides `repo.json` (blobs, log
//!   segments, `device.json`) with XChaCha20-Poly1305, a random 24-byte nonce per object, and
//!   the object's key in the repository as associated data, so an object can't be passed off
//!   as another;
//! - the **address key** names blobs: `blobs/<hh>/<keyed BLAKE3 of the chunk's hash>`, so the
//!   provider can't tell whether a repository holds a given save by computing its hash.

use crate::error::{Error, Result};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

/// `encryption.scheme` in `repo.json`.
pub const SCHEME: &str = "xchacha20poly1305-blake3";
/// First byte of every sealed object, so a later scheme can be told apart.
const SEALED_V1: u8 = 1;
const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;
const KEY_LEN: usize = 32;
const SALT_LEN: usize = 16;

const DATA_KEY_CONTEXT: &str = "emusync 2026-10 repository data encryption key";
const ADDRESS_KEY_CONTEXT: &str = "emusync 2026-10 repository blob address key";

/// Argon2id cost parameters, stored in `repo.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct KdfParams {
    /// Memory, in KiB.
    pub m_kib: u32,
    /// Passes.
    pub t: u32,
    /// Lanes.
    pub p: u32,
}

impl Default for KdfParams {
    /// 64 MiB, 3 passes, 1 lane (RFC 9106 §4's second recommendation, single-threaded): under
    /// 0.1 s on a desktop CPU; not yet measured on a phone.
    fn default() -> Self {
        Self { m_kib: 64 * 1024, t: 3, p: 1 }
    }
}

impl KdfParams {
    /// Parameters a repository may ask for. A `repo.json` beyond these is refused rather than
    /// letting whoever can write the bucket make every device allocate gigabytes.
    fn check(&self) -> Result<argon2::Params> {
        if self.m_kib > 1024 * 1024 || self.t == 0 || self.t > 64 || self.p == 0 || self.p > 16 {
            return Err(Error::Incompatible(format!("unsupported key derivation parameters {self:?}")));
        }
        argon2::Params::new(self.m_kib, self.t, self.p, Some(KEY_LEN))
            .map_err(|e| Error::Incompatible(format!("invalid key derivation parameters {self:?}: {e}")))
    }

    fn derive(&self, passphrase: &str, salt: &[u8]) -> Result<Zeroizing<[u8; KEY_LEN]>> {
        let params = self.check()?;
        let mut out = Zeroizing::new([0u8; KEY_LEN]);
        argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
            .hash_password_into(passphrase.as_bytes(), salt, &mut *out)
            .map_err(|e| Error::InvalidArg(format!("key derivation: {e}")))?;
        Ok(out)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Kdf {
    /// Always `argon2id`.
    pub alg: String,
    #[serde(flatten)]
    pub params: KdfParams,
    /// Hex.
    pub salt: String,
}

/// The `encryption` object of `repo.json` (DESIGN §6.1). Public: everything in it may be read
/// by the provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptionHeader {
    pub scheme: String,
    pub kdf: Kdf,
    /// Hex of nonce ‖ ciphertext ‖ tag: the repository key sealed with the passphrase key, with
    /// `emusync repository key <repo_id>` as associated data.
    pub wrapped_key: String,
}

fn wrap_aad(repo_id: &str) -> Vec<u8> {
    format!("emusync repository key {repo_id}").into_bytes()
}

fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("the operating system's random number generator is unavailable");
    b
}

/// A repository key. Wiped from memory when dropped.
#[derive(Clone)]
pub struct RepoKey(Zeroizing<[u8; KEY_LEN]>);

impl std::fmt::Debug for RepoKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RepoKey(…)")
    }
}

impl RepoKey {
    pub fn generate() -> Self {
        let mut b = random::<KEY_LEN>();
        let key = Self(Zeroizing::new(b));
        b.zeroize();
        key
    }

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        let arr: [u8; KEY_LEN] = b.try_into().ok()?;
        Some(Self(Zeroizing::new(arr)))
    }

    pub fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }

    /// Wraps this key with `passphrase` for `repo.json`.
    pub fn wrap(&self, passphrase: &str, params: KdfParams, repo_id: &str) -> Result<EncryptionHeader> {
        if passphrase.is_empty() {
            return Err(Error::InvalidArg("the passphrase is empty".into()));
        }
        let salt = random::<SALT_LEN>();
        let kek = params.derive(passphrase, &salt)?;
        let sealed = seal_with(&kek, &wrap_aad(repo_id), &self.0[..]);
        Ok(EncryptionHeader {
            scheme: SCHEME.into(),
            kdf: Kdf { alg: "argon2id".into(), params, salt: hex::encode(salt) },
            wrapped_key: hex::encode(&sealed[1..]),
        })
    }

    /// Unwraps the key in `header` with `passphrase`. [`Error::WrongPassphrase`] if it doesn't
    /// open it.
    pub fn unwrap(header: &EncryptionHeader, passphrase: &str, repo_id: &str) -> Result<Self> {
        header.check()?;
        let salt = hex::decode(&header.kdf.salt).map_err(|_| Error::Incompatible("repo.json: bad salt".into()))?;
        let wrapped =
            hex::decode(&header.wrapped_key).map_err(|_| Error::Incompatible("repo.json: bad wrapped key".into()))?;
        let kek = header.kdf.params.derive(passphrase, &salt)?;
        let mut sealed = Vec::with_capacity(wrapped.len() + 1);
        sealed.push(SEALED_V1);
        sealed.extend_from_slice(&wrapped);
        let plain = Zeroizing::new(open_with(&kek, &wrap_aad(repo_id), &sealed).map_err(|_| Error::WrongPassphrase)?);
        Self::from_bytes(&plain)
            .ok_or_else(|| Error::Incompatible("repo.json: the wrapped key has the wrong size".into()))
    }
}

impl EncryptionHeader {
    /// Whether this version can use the header (scheme and KDF known, parameters sane).
    pub fn check(&self) -> Result<()> {
        if self.scheme != SCHEME || self.kdf.alg != "argon2id" {
            return Err(Error::Incompatible(format!(
                "the repository is encrypted with {} / {}, which this version of emusync does not support",
                self.scheme, self.kdf.alg
            )));
        }
        self.kdf.params.check().map(|_| ())
    }
}

fn seal_with(key: &[u8; KEY_LEN], aad: &[u8], plain: &[u8]) -> Vec<u8> {
    let nonce = random::<NONCE_LEN>();
    let ct = XChaCha20Poly1305::new(key.into())
        .encrypt(XNonce::from_slice(&nonce), Payload { msg: plain, aad })
        .expect("XChaCha20-Poly1305 encryption cannot fail for in-memory buffers");
    let mut out = Vec::with_capacity(1 + NONCE_LEN + ct.len());
    out.push(SEALED_V1);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    out
}

/// `Err(())` if `sealed` is malformed or does not authenticate.
fn open_with(key: &[u8; KEY_LEN], aad: &[u8], sealed: &[u8]) -> std::result::Result<Vec<u8>, ()> {
    if sealed.len() < 1 + NONCE_LEN + TAG_LEN || sealed[0] != SEALED_V1 {
        return Err(());
    }
    let (nonce, ct) = sealed[1..].split_at(NONCE_LEN);
    XChaCha20Poly1305::new(key.into()).decrypt(XNonce::from_slice(nonce), Payload { msg: ct, aad }).map_err(|_| ())
}

/// Seals and opens a repository's objects (see the module docs).
pub struct Cipher {
    data_key: Zeroizing<[u8; KEY_LEN]>,
    address_key: Zeroizing<[u8; KEY_LEN]>,
}

impl Cipher {
    pub fn new(key: &RepoKey) -> Self {
        Self {
            data_key: Zeroizing::new(blake3::derive_key(DATA_KEY_CONTEXT, key.as_bytes())),
            address_key: Zeroizing::new(blake3::derive_key(ADDRESS_KEY_CONTEXT, key.as_bytes())),
        }
    }

    /// Encrypts the object stored at `key`.
    pub fn seal(&self, key: &str, plain: &[u8]) -> Vec<u8> {
        seal_with(&self.data_key, key.as_bytes(), plain)
    }

    /// Decrypts the object stored at `key`. [`Error::Integrity`] if it doesn't authenticate:
    /// it was altered, moved from another key, or written with another repository key.
    pub fn open(&self, key: &str, sealed: &[u8]) -> Result<Vec<u8>> {
        open_with(&self.data_key, key.as_bytes(), sealed)
            .map_err(|()| Error::Integrity(format!("{key} does not decrypt with this repository's key")))
    }

    /// Remote key of the blob holding chunk `id` (a `b3:` content hash).
    pub fn blob_key(&self, id: &str) -> String {
        let hex = blake3::keyed_hash(&self.address_key, id.as_bytes()).to_hex();
        format!("blobs/{}/{hex}", &hex[..2])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAST: KdfParams = KdfParams { m_kib: 64, t: 1, p: 1 };

    #[test]
    fn wrap_and_unwrap() {
        let key = RepoKey::generate();
        let h = key.wrap("correct horse", FAST, "repo-1").unwrap();
        assert_eq!(RepoKey::unwrap(&h, "correct horse", "repo-1").unwrap().as_bytes(), key.as_bytes());
        assert!(matches!(RepoKey::unwrap(&h, "wrong horse", "repo-1"), Err(Error::WrongPassphrase)));
        // Bound to the repository id.
        assert!(matches!(RepoKey::unwrap(&h, "correct horse", "repo-2"), Err(Error::WrongPassphrase)));
        assert!(key.wrap("", FAST, "repo-1").is_err());
        // The header round-trips through JSON.
        let json = serde_json::to_value(&h).unwrap();
        assert_eq!(json["kdf"]["alg"], "argon2id");
        assert_eq!(json["kdf"]["m_kib"], 64);
        assert_eq!(serde_json::from_value::<EncryptionHeader>(json).unwrap(), h);
    }

    #[test]
    fn hostile_headers_are_refused() {
        let key = RepoKey::generate();
        let mut h = key.wrap("pw", FAST, "r").unwrap();
        h.kdf.params.m_kib = 64 * 1024 * 1024;
        assert!(matches!(RepoKey::unwrap(&h, "pw", "r"), Err(Error::Incompatible(_))));
        let mut h = key.wrap("pw", FAST, "r").unwrap();
        h.scheme = "rot13".into();
        assert!(matches!(RepoKey::unwrap(&h, "pw", "r"), Err(Error::Incompatible(_))));
    }

    #[test]
    fn seal_open() {
        let c = Cipher::new(&RepoKey::generate());
        let sealed = c.seal("devices/x/log/1", b"hello");
        assert_eq!(sealed.len(), 1 + NONCE_LEN + 5 + TAG_LEN);
        assert_eq!(c.open("devices/x/log/1", &sealed).unwrap(), b"hello");
        // Random nonces: sealing twice gives different bytes.
        assert_ne!(c.seal("devices/x/log/1", b"hello"), sealed);
        // Moved to another key, tampered with, truncated, or another repository's key.
        assert!(c.open("devices/x/log/2", &sealed).is_err());
        let mut bad = sealed.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(c.open("devices/x/log/1", &bad).is_err());
        assert!(c.open("devices/x/log/1", &sealed[..10]).is_err());
        assert!(Cipher::new(&RepoKey::generate()).open("devices/x/log/1", &sealed).is_err());
    }

    #[test]
    fn blob_keys_are_keyed() {
        let id = crate::model::content_id(b"save");
        let a = Cipher::new(&RepoKey::generate());
        let b = Cipher::new(&RepoKey::generate());
        assert_eq!(a.blob_key(&id), a.blob_key(&id));
        assert_ne!(a.blob_key(&id), b.blob_key(&id));
        assert_ne!(a.blob_key(&id), crate::chunk::blob_key(&id));
        assert!(!a.blob_key(&id).contains(id.strip_prefix("b3:").unwrap()));
        let name = a.blob_key(&id);
        let (dir, hex) = name.strip_prefix("blobs/").unwrap().split_once('/').unwrap();
        assert_eq!(hex.len(), 64);
        assert_eq!(&hex[..2], dir);
    }
}

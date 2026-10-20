//! End-to-end encryption (DESIGN §7, §22) over the in-memory remote: devices syncing through an
//! encrypted repository, what the storage provider can see, passphrases, locked repositories,
//! and unencrypted repositories staying as they were.

use emusync_core::{chunk, ContainerDesc, Encryption, Engine, EngineOptions, Error, KdfParams};
use emusync_remote::MemoryRemote;
use std::path::PathBuf;
use std::sync::Arc;

/// Cheap Argon2id parameters, so the tests don't spend their time deriving keys.
const FAST: KdfParams = KdfParams { m_kib: 256, t: 1, p: 1 };
const PASS: &str = "correct horse battery staple";

struct Device {
    engine: Engine,
    card: PathBuf,
    data: PathBuf,
    _dir: tempfile::TempDir,
}

impl Device {
    /// A device with a card, not connected to any remote yet.
    fn new(name: &str, card: &[u8]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let card_path = dir.path().join("memcards").join("Mcd001.ps2");
        std::fs::create_dir_all(card_path.parent().unwrap()).unwrap();
        std::fs::write(&card_path, card).unwrap();
        let data = dir.path().join("data");
        let mut engine = open(&data, name);
        engine.add_container(ContainerDesc { location: card_path.clone(), ..Default::default() }).unwrap();
        Device { engine, card: card_path, data, _dir: dir }
    }

    /// A device with no container: it only reads versions.
    fn bare(name: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        Device { engine: open(&data, name), card: dir.path().join("none"), data, _dir: dir }
    }

    fn encrypted(remote: &Arc<MemoryRemote>, name: &str, card: &[u8]) -> Self {
        let mut d = Self::new(name, card);
        d.engine.add_remote_encrypted(remote.clone(), PASS, FAST).unwrap();
        assert_eq!(d.engine.encryption(), Encryption::Unlocked);
        d
    }

    fn read(&self) -> Vec<u8> {
        std::fs::read(&self.card).unwrap()
    }

    fn write(&self, bytes: &[u8]) {
        std::fs::write(&self.card, bytes).unwrap();
    }

    /// Closes and reopens the engine on the same data directory, as a restarted app would.
    fn restart(&mut self, remote: &Arc<MemoryRemote>) {
        let name = self.engine.device_name().unwrap();
        self.engine = open(&self.data, &name);
        self.engine.attach_remote(remote.clone());
    }
}

fn open(data: &std::path::Path, name: &str) -> Engine {
    Engine::open(data, EngineOptions { device_name: Some(name.into()), ..Default::default() }).unwrap()
}

/// Deterministic pseudo-random bytes, large enough to be split into several chunks.
fn card_image(seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..600 * 1024)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

fn repo_json(remote: &MemoryRemote) -> serde_json::Value {
    serde_json::from_slice(&remote.raw_get("repo.json").unwrap()).unwrap()
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// Every content address the device knows: version ids, content hashes and chunk ids.
fn addresses(dev: &Device) -> Vec<String> {
    let mut out = Vec::new();
    for e in dev.engine.history("Mcd001").unwrap() {
        out.push(e.version.id.clone());
        out.push(e.version.content.clone());
        out.extend(e.version.chunks.iter().cloned());
    }
    out
}

#[test]
fn two_devices_sync_through_an_encrypted_repository() {
    let remote = Arc::new(MemoryRemote::default());
    let a1 = card_image(1);
    let mut a = Device::encrypted(&remote, "windows", &a1);
    let r = a.engine.sync().unwrap();
    assert_eq!((r.new_local_versions, r.uploaded_versions), (1, 1));
    assert!(r.uploaded_blobs > 1);

    // B joins with the passphrase, holding an identical copy: adopted without a new version.
    let mut b = Device::new("android", &a1);
    b.engine.add_remote_encrypted(remote.clone(), PASS, FAST).unwrap();
    let r = b.engine.sync().unwrap();
    assert_eq!((r.new_local_versions, r.conflicts.len()), (0, 0));
    assert_eq!(b.engine.cards().unwrap()[0].units[0].tips, a.engine.cards().unwrap()[0].units[0].tips);
    // The other device's name came through its encrypted device.json.
    let names: Vec<String> = b.engine.history("Mcd001").unwrap().into_iter().map(|e| e.device).collect();
    assert!(names.iter().any(|n| n == "windows"), "{names:?}");

    // Edits flow both ways; only the changed chunks upload.
    let mut a2 = a1.clone();
    a2[300_000..300_064].copy_from_slice(&[7; 64]);
    a.write(&a2);
    let r = a.engine.sync().unwrap();
    assert!(r.uploaded_blobs <= 3, "{}", r.uploaded_blobs);
    let r = b.engine.sync().unwrap();
    assert_eq!(r.applied.len(), 1);
    assert_eq!(b.read(), a2);
    let b3 = card_image(3);
    b.write(&b3);
    b.engine.sync().unwrap();
    a.engine.sync().unwrap();
    assert_eq!(a.read(), b3);

    // Every version is readable from the remote alone, by a device with no local copies.
    let mut c = Device::bare("reader");
    c.engine.add_remote_encrypted(remote.clone(), PASS, FAST).unwrap();
    c.engine.sync().unwrap();
    let history = a.engine.history("Mcd001").unwrap();
    assert_eq!(history.len(), 3);
    for e in history {
        assert_eq!(c.engine.read_version(&e.version.id).unwrap().len() as u64, e.version.size);
    }
    assert_eq!(c.engine.read_version(&a.engine.history("Mcd001").unwrap()[0].version.id).unwrap(), b3);
}

#[test]
fn the_provider_sees_only_names_and_sizes() {
    let remote = Arc::new(MemoryRemote::default());
    let a1 = card_image(10);
    let mut a = Device::encrypted(&remote, "Living room PC", &a1);
    a.engine.sync().unwrap();
    let addrs = addresses(&a);
    assert!(addrs.len() >= 4);

    let repo = repo_json(&remote);
    assert_eq!(repo["format"], 2);
    assert_eq!(repo["encryption"]["scheme"], "xchacha20poly1305-blake3");
    assert_eq!(repo["encryption"]["kdf"]["alg"], "argon2id");

    for key in remote.keys() {
        if key == "repo.json" || key.starts_with("_probe/") {
            continue;
        }
        let body = remote.raw_get(&key).unwrap();
        // No card bytes, no log JSON, no device name, no content address, in names or bodies.
        assert!(!contains(&body, &a1[1000..1032]), "{key} holds card bytes");
        for marker in [&b"\"op\""[..], b"Living room PC", b"Mcd001"] {
            assert!(!contains(&body, marker), "{key} holds {:?}", String::from_utf8_lossy(marker));
        }
        for id in &addrs {
            let hex = id.strip_prefix("b3:").unwrap();
            assert!(!key.contains(hex), "{key} is named after {id}");
            assert!(!contains(&body, hex.as_bytes()), "{key} holds {id}");
        }
    }
    // Blobs are not where an unencrypted repository would keep them.
    for e in a.engine.history("Mcd001").unwrap() {
        for id in &e.version.chunks {
            assert!(remote.raw_get(&chunk::blob_key(id)).is_none());
        }
    }
}

#[test]
fn passphrase_once_per_device() {
    let remote = Arc::new(MemoryRemote::default());
    let mut a = Device::encrypted(&remote, "a", &card_image(20));
    a.engine.sync().unwrap();

    // Restarting needs no passphrase: the key was kept in the data directory.
    a.restart(&remote);
    assert_eq!(a.engine.encryption(), Encryption::Unlocked);
    a.write(&card_image(21));
    assert_eq!(a.engine.sync().unwrap().uploaded_versions, 1);

    // Neither does adding the same remote again (as an embedded host does on every start).
    a.engine.add_remote(remote.clone()).unwrap();
    assert_eq!(a.engine.encryption(), Encryption::Unlocked);
    let keys = std::fs::read_dir(a.data.join("keys")).unwrap().count();
    assert_eq!(keys, 1);

    // Forgetting the key locks the repository again.
    a.engine.forget_key().unwrap();
    assert_eq!(a.engine.encryption(), Encryption::Locked);
    assert!(matches!(a.engine.sync(), Err(Error::Locked)));
    a.restart(&remote);
    assert_eq!(a.engine.encryption(), Encryption::Locked);
    a.engine.unlock(PASS).unwrap();
    a.engine.sync().unwrap();
}

#[test]
fn locked_repository_and_wrong_passphrase() {
    let remote = Arc::new(MemoryRemote::default());
    let a1 = card_image(30);
    let mut a = Device::encrypted(&remote, "a", &a1);
    a.engine.sync().unwrap();

    // Adding the remote without the passphrase binds it, locked.
    let mut b = Device::new("b", &a1);
    b.engine.add_remote(remote.clone()).unwrap();
    assert_eq!(b.engine.encryption(), Encryption::Locked);
    let e = b.engine.sync().unwrap_err();
    assert!(matches!(e, Error::Locked));
    assert!(e.to_string().contains("unlock it with the passphrase"), "{e}");
    assert!(matches!(b.engine.push_pending(), Err(Error::Locked)));
    // Local snapshots still work while locked.
    b.write(&card_image(31));
    assert_eq!(b.engine.snapshot_containers(None).unwrap().new_local_versions, 1);

    let e = b.engine.unlock("wrong").unwrap_err();
    assert!(matches!(e, Error::WrongPassphrase));
    assert_eq!(e.to_string(), "wrong passphrase");
    assert_eq!(b.engine.encryption(), Encryption::Locked);
    b.engine.unlock(PASS).unwrap();
    let r = b.engine.sync().unwrap();
    // B's offline edit and A's version diverged; both are kept.
    assert_eq!(r.conflicts.len(), 1, "{r:?}");

    // The wrong passphrase on a new device: nothing is bound.
    let mut c = Device::bare("c");
    let e = c.engine.add_remote_encrypted(remote.clone(), "nope", FAST).unwrap_err();
    assert!(matches!(e, Error::WrongPassphrase));
    assert_eq!(c.engine.remote_uri().unwrap(), None);
    assert!(!c.data.join("keys").exists());
}

#[test]
fn versions_made_before_the_remote_was_added_upload_encrypted() {
    let remote = Arc::new(MemoryRemote::default());
    let a1 = card_image(40);
    let mut a = Device::new("a", &a1);
    // Offline first: a local version with no remote.
    assert_eq!(a.engine.sync().unwrap().new_local_versions, 1);
    a.engine.add_remote_encrypted(remote.clone(), PASS, FAST).unwrap();
    assert_eq!(a.engine.sync().unwrap().uploaded_versions, 1);

    let mut b = Device::bare("b");
    b.engine.add_remote_encrypted(remote.clone(), PASS, FAST).unwrap();
    assert_eq!(b.engine.sync().unwrap().pulled_records, 1);
    let id = a.engine.history("Mcd001").unwrap()[0].version.id.clone();
    assert_eq!(b.engine.read_version(&id).unwrap(), a1);
}

#[test]
fn tampered_objects_are_rejected() {
    let remote = Arc::new(MemoryRemote::default());
    let a1 = card_image(50);
    let mut a = Device::encrypted(&remote, "a", &a1);
    a.engine.sync().unwrap();
    let seg = remote.keys().into_iter().find(|k| k.ends_with(".ndjson")).unwrap();

    // A segment that doesn't authenticate is skipped, with a warning, and nothing from it is used.
    let good = remote.raw_get(&seg).unwrap();
    let mut bad = good.clone();
    let n = bad.len();
    bad[n - 1] ^= 1;
    remote.raw_put(&seg, &bad);
    let mut b = Device::bare("b");
    b.engine.add_remote_encrypted(remote.clone(), PASS, FAST).unwrap();
    let r = b.engine.sync().unwrap();
    assert_eq!(r.pulled_records, 0);
    assert!(r.warnings.iter().any(|w| w.contains("does not decrypt")), "{:?}", r.warnings);

    // A plaintext segment injected into an encrypted repository is rejected the same way.
    remote.raw_put(
        &seg,
        b"{\"op\":\"card\",\"card\":\"ps2:Evil\",\"mode\":\"units\",\"hlc\":\"2026-01-01T00:00:00.000Z/0000\"}\n",
    );
    let mut c = Device::bare("c");
    c.engine.add_remote_encrypted(remote.clone(), PASS, FAST).unwrap();
    let r = c.engine.sync().unwrap();
    assert_eq!(r.pulled_records, 0);
    assert!(r.warnings.iter().any(|w| w.contains("does not decrypt")), "{:?}", r.warnings);
    assert!(c.engine.card_modes().unwrap().iter().all(|(card, _)| card != "ps2:Evil"));

    // A blob swapped for another one fails its check rather than being applied.
    remote.raw_put(&seg, &good);
    let blobs: Vec<String> = remote.keys().into_iter().filter(|k| k.starts_with("blobs/")).collect();
    assert!(blobs.len() >= 2);
    let other = remote.raw_get(&blobs[1]).unwrap();
    remote.raw_put(&blobs[0], &other);
    let mut d = Device::bare("d");
    d.engine.add_remote_encrypted(remote.clone(), PASS, FAST).unwrap();
    d.engine.sync().unwrap();
    let id = a.engine.history("Mcd001").unwrap()[0].version.id.clone();
    assert!(matches!(d.engine.read_version(&id), Err(Error::Integrity(_))));
}

#[test]
fn unencrypted_repositories_are_unchanged() {
    let remote = Arc::new(MemoryRemote::default());
    let a1 = card_image(60);
    let mut a = Device::new("Living room PC", &a1);
    a.engine.add_remote(remote.clone()).unwrap();
    assert_eq!(a.engine.encryption(), Encryption::None);
    a.engine.sync().unwrap();

    // The same format as before encryption existed: format 1, no encryption, blobs under their
    // plain content hash in the old encoding, plaintext logs and device.json.
    let repo = repo_json(&remote);
    assert_eq!(repo["format"], 1);
    assert!(repo["encryption"].is_null());
    assert!(uuid::Uuid::parse_str(repo["repo_id"].as_str().unwrap()).is_ok());
    for e in a.engine.history("Mcd001").unwrap() {
        for id in &e.version.chunks {
            let blob = remote.raw_get(&chunk::blob_key(id)).unwrap();
            assert!(chunk::decode_blob(&blob, id).unwrap().len() <= chunk::MAX_CHUNK);
        }
    }
    let seg = remote.keys().into_iter().find(|k| k.ends_with(".ndjson")).unwrap();
    let body = String::from_utf8(remote.raw_get(&seg).unwrap()).unwrap();
    assert!(body.starts_with("{\"op\":"), "{body}");
    let dev = remote.keys().into_iter().find(|k| k.ends_with("device.json")).unwrap();
    assert!(contains(&remote.raw_get(&dev).unwrap(), b"Living room PC"));
    assert!(!a.data.join("keys").exists());

    // A passphrase can't be added to it later...
    let mut b = Device::new("b", &a1);
    let e = b.engine.add_remote_encrypted(remote.clone(), PASS, FAST).unwrap_err();
    assert!(matches!(e, Error::Incompatible(_)), "{e}");
    assert!(e.to_string().contains("unencrypted repository"), "{e}");
    assert!(matches!(a.engine.unlock(PASS), Err(Error::InvalidArg(_))));
    // ...and it syncs as before.
    b.engine.add_remote(remote.clone()).unwrap();
    b.engine.sync().unwrap();
    b.engine.sync().unwrap();
    assert_eq!(b.engine.encryption(), Encryption::None);
}

#[test]
fn repository_formats() {
    // What an older emusync checks before using a repository: `format` must be 1. An
    // encrypted repository has format 2, so an older version refuses it rather than mixing
    // plaintext into it.
    let remote = Arc::new(MemoryRemote::default());
    Device::encrypted(&remote, "a", b"x");
    assert_ne!(repo_json(&remote)["format"], 1);

    // A repository from a newer version is refused with a clear message.
    let newer = Arc::new(MemoryRemote::default());
    newer.raw_put("repo.json", br#"{"format":3,"repo_id":"0192e0f4-0000-7000-8000-000000000000"}"#);
    let mut d = Device::bare("d");
    let e = d.engine.add_remote(newer.clone()).unwrap_err();
    assert!(matches!(e, Error::Incompatible(_)));
    assert!(e.to_string().contains("update emusync"), "{e}");

    // An unknown encryption scheme, or hostile key-derivation costs, likewise.
    let mut repo = repo_json(&remote);
    repo["encryption"]["scheme"] = "aes-ecb".into();
    let odd = Arc::new(MemoryRemote::default());
    odd.raw_put("repo.json", repo.to_string().as_bytes());
    assert!(matches!(d.engine.add_remote(odd), Err(Error::Incompatible(_))));
    let mut repo = repo_json(&remote);
    repo["encryption"]["kdf"]["m_kib"] = (64u64 << 20).into();
    let hostile = Arc::new(MemoryRemote::default());
    hostile.raw_put("repo.json", repo.to_string().as_bytes());
    assert!(matches!(d.engine.add_remote(hostile), Err(Error::Incompatible(_))));
    assert_eq!(d.engine.remote_uri().unwrap(), None);
}

#[test]
fn default_key_derivation() {
    // The parameters hosts get (64 MiB, 3 passes) work end to end.
    let remote = Arc::new(MemoryRemote::default());
    let mut a = Device::new("a", &card_image(70));
    let started = std::time::Instant::now();
    a.engine.add_remote_encrypted(remote.clone(), PASS, KdfParams::default()).unwrap();
    eprintln!("Argon2id with the default parameters: {:?}", started.elapsed());
    a.engine.sync().unwrap();
    assert_eq!(repo_json(&remote)["encryption"]["kdf"]["m_kib"], 65536);
    let mut b = Device::bare("b");
    b.engine.add_remote(remote.clone()).unwrap();
    b.engine.unlock(PASS).unwrap();
    assert_eq!(b.engine.sync().unwrap().pulled_records, 1);
}

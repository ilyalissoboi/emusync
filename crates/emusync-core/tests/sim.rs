//! Multi-device simulations over the in-memory remote (DESIGN §13 "Simulated sync").
//!
//! The invariant checked throughout: every version ever created stays reachable from every
//! device, and devices converge once they are connected.

use emusync_core::{ConflictPolicy, ContainerDesc, Engine, EngineOptions, Resolution, SyncReport};
use emusync_remote::{MemoryRemote, MemoryRemoteOptions, PutOutcome, Remote};
use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// One device's connection to the shared remote, with its own partition switch.
struct Link {
    inner: Arc<MemoryRemote>,
    up: AtomicBool,
}

impl Link {
    fn check(&self) -> emusync_remote::Result<()> {
        if self.up.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(emusync_remote::RemoteError::Network("partitioned".into()))
        }
    }
}

impl Remote for Link {
    fn describe(&self) -> String {
        self.inner.describe()
    }
    fn put(&self, key: &str, data: &[u8], if_none_match: bool) -> emusync_remote::Result<PutOutcome> {
        self.check()?;
        self.inner.put(key, data, if_none_match)
    }
    fn get(&self, key: &str) -> emusync_remote::Result<Option<Vec<u8>>> {
        self.check()?;
        self.inner.get(key)
    }
    fn head(&self, key: &str) -> emusync_remote::Result<bool> {
        self.check()?;
        self.inner.head(key)
    }
    fn list(&self, prefix: &str, start_after: Option<&str>) -> emusync_remote::Result<Vec<String>> {
        self.check()?;
        self.inner.list(prefix, start_after)
    }
    fn list_dirs(&self, prefix: &str) -> emusync_remote::Result<Vec<String>> {
        self.check()?;
        self.inner.list_dirs(prefix)
    }
}

struct Device {
    engine: Engine,
    link: Arc<Link>,
    card: PathBuf,
    data: PathBuf,
    _dir: tempfile::TempDir,
}

impl Device {
    fn new(remote: &Arc<MemoryRemote>, name: &str, card: &[u8], policy: ConflictPolicy) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let card_path = dir.path().join("memcards").join("Mcd001.ps2");
        std::fs::create_dir_all(card_path.parent().unwrap()).unwrap();
        std::fs::write(&card_path, card).unwrap();
        let data = dir.path().join("data");
        let link = Arc::new(Link { inner: remote.clone(), up: AtomicBool::new(true) });
        let mut engine = open_engine(&data, name, policy);
        engine.add_remote(link.clone()).unwrap();
        engine.add_container(ContainerDesc { location: card_path.clone(), ..Default::default() }).unwrap();
        Device { engine, link, card: card_path, data, _dir: dir }
    }

    fn sync(&mut self) -> SyncReport {
        self.engine.sync().unwrap()
    }

    fn write(&self, bytes: &[u8]) {
        std::fs::write(&self.card, bytes).unwrap();
    }

    fn read(&self) -> Vec<u8> {
        std::fs::read(&self.card).unwrap()
    }

    fn set_online(&self, up: bool) {
        self.link.up.store(up, Ordering::SeqCst);
    }

    fn versions(&self) -> BTreeSet<String> {
        self.engine.all_version_ids().unwrap().into_iter().collect()
    }

    fn tips(&self) -> Vec<String> {
        self.engine.cards().unwrap()[0].tips.clone()
    }
}

fn open_engine(data: &Path, name: &str, policy: ConflictPolicy) -> Engine {
    Engine::open(data, EngineOptions { device_name: Some(name.into()), policy, ..Default::default() }).unwrap()
}

/// Deterministic pseudo-random bytes.
fn bytes(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

fn card_image(seed: u64) -> Vec<u8> {
    // Large enough to be split into several chunks.
    bytes(600 * 1024, seed)
}

fn assert_all_readable(dev: &Device, ids: &BTreeSet<String>) {
    for id in ids {
        dev.engine.read_version(id).unwrap_or_else(|e| panic!("version {id} unreadable: {e}"));
    }
}

#[test]
fn fast_forward_between_two_devices() {
    let remote = Arc::new(MemoryRemote::default());
    let a1 = card_image(1);
    let mut a = Device::new(&remote, "windows", &a1, ConflictPolicy::Ask);
    let r = a.sync();
    assert!(!r.offline);
    assert_eq!(r.new_local_versions, 1);
    assert_eq!(r.uploaded_versions, 1);

    // B starts with an identical copy: first sync adopts the cloud version silently.
    let mut b = Device::new(&remote, "android", &a1, ConflictPolicy::Ask);
    let r = b.sync();
    assert_eq!(r.new_local_versions, 0);
    assert!(r.conflicts.is_empty());
    assert_eq!(b.tips(), a.tips());

    // A edits; B fast-forwards.
    let mut a2 = a1.clone();
    a2[300_000..300_064].copy_from_slice(&[7; 64]);
    a.write(&a2);
    let r = a.sync();
    assert_eq!(r.new_local_versions, 1);
    assert!(r.uploaded_blobs <= 3, "only changed chunks upload, got {}", r.uploaded_blobs);
    let r = b.sync();
    assert_eq!(r.applied.len(), 1);
    assert_eq!(b.read(), a2);

    // And back the other way.
    let b3 = card_image(3);
    b.write(&b3);
    b.sync();
    let r = a.sync();
    assert_eq!(r.applied.len(), 1);
    assert_eq!(a.read(), b3);
    assert_eq!(a.engine.history("Mcd001").unwrap().len(), 3);
    assert_eq!(a.versions(), b.versions());

    // A quiet cycle does nothing and costs one LIST per other device plus device discovery.
    let lists = remote.lists.load(Ordering::SeqCst);
    let puts = remote.puts.load(Ordering::SeqCst);
    let r = a.sync();
    assert!(r.applied.is_empty() && r.new_local_versions == 0 && r.uploaded_versions == 0);
    assert_eq!(remote.puts.load(Ordering::SeqCst), puts);
    assert_eq!(remote.lists.load(Ordering::SeqCst) - lists, 2);
}

#[test]
fn offline_divergence_creates_conflict_and_loses_nothing() {
    let remote = Arc::new(MemoryRemote::default());
    let base = card_image(10);
    let mut a = Device::new(&remote, "mac", &base, ConflictPolicy::Ask);
    a.sync();
    let mut b = Device::new(&remote, "android", &base, ConflictPolicy::Ask);
    b.sync();

    // Both play offline.
    a.set_online(false);
    b.set_online(false);
    let a2 = card_image(11);
    let b2 = card_image(12);
    a.write(&a2);
    b.write(&b2);
    let r = a.sync();
    assert!(r.offline);
    assert_eq!(r.new_local_versions, 1);
    assert_eq!(r.pending_uploads, 1);
    assert!(b.sync().offline);

    // Back online.
    a.set_online(true);
    b.set_online(true);
    assert_eq!(a.sync().pending_uploads, 0);
    let r = b.sync();
    assert_eq!(r.conflicts.len(), 1, "true divergence must be a conflict");
    assert!(r.applied.is_empty(), "ask policy leaves the local container alone");
    assert_eq!(b.read(), b2);
    let r = a.sync();
    assert_eq!(r.conflicts.len(), 1);
    assert_eq!(a.read(), a2);

    let all = a.versions();
    assert_eq!(all, b.versions());
    assert_eq!(all.len(), 3);
    assert_all_readable(&a, &all);
    assert_all_readable(&b, &all);

    // The conflict shows both sides.
    let conflicts = b.engine.conflicts().unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].tips.len(), 2);
    assert_eq!(conflicts[0].tips.iter().map(|t| t.device.as_str()).collect::<BTreeSet<_>>(), ["android", "mac"].into());

    // Resolve on the phone in favour of the Mac.
    let mac_tip = conflicts[0].tips.iter().find(|t| t.device == "mac").unwrap().version.id.clone();
    b.engine.resolve(conflicts[0].id, Resolution::Take(mac_tip[..10].into())).unwrap();
    let r = b.sync();
    assert_eq!(r.applied.len(), 1);
    assert_eq!(b.read(), a2);
    let r = a.sync();
    assert!(r.conflicts.is_empty());
    assert!(a.engine.conflicts().unwrap().is_empty());
    assert_eq!(a.read(), a2);
    assert_eq!(a.tips(), b.tips());
    assert_eq!(a.tips().len(), 1);

    // The phone's losing save is still in history everywhere, and restorable.
    let all = a.versions();
    assert_eq!(all.len(), 4);
    assert_eq!(all, b.versions());
    assert_all_readable(&a, &all);
    let b2_version = b
        .engine
        .history("Mcd001")
        .unwrap()
        .into_iter()
        .find(|h| h.device == "android" && !h.version.parents.is_empty() && h.version.parents.len() == 1)
        .unwrap();
    assert_eq!(a.engine.read_version(&b2_version.version.id).unwrap(), b2);
    // The overwritten bytes also went into the phone's local snapshot store.
    let snap = b.data.join("snapshots").join(b2_version.version.content.trim_start_matches("b3:"));
    assert_eq!(std::fs::read(snap).unwrap(), b2);

    a.engine.restore("Mcd001", &b2_version.version.id).unwrap();
    a.sync();
    b.sync();
    assert_eq!(a.read(), b2);
    assert_eq!(b.read(), b2);
}

#[test]
fn newest_policy_converges_with_one_deterministic_merge() {
    let remote = Arc::new(MemoryRemote::default());
    let base = card_image(20);
    let mut a = Device::new(&remote, "a", &base, ConflictPolicy::Newest);
    a.sync();
    let mut b = Device::new(&remote, "b", &base, ConflictPolicy::Newest);
    b.sync();
    a.set_online(false);
    b.set_online(false);
    a.write(&card_image(21));
    a.sync();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let newest = card_image(22);
    b.write(&newest);
    b.sync();
    a.set_online(true);
    b.set_online(true);

    // With delayed listing both devices push before either sees the other, then both merge
    // independently.
    remote.set_list_delay(2);
    for _ in 0..4 {
        a.sync();
        b.sync();
    }
    remote.set_list_delay(0);
    for _ in 0..2 {
        a.sync();
        b.sync();
    }
    assert_eq!(a.read(), newest);
    assert_eq!(b.read(), newest);
    assert_eq!(a.tips(), b.tips());
    assert_eq!(a.tips().len(), 1);
    // base + two edits + exactly one merge (both devices computed the same record).
    assert_eq!(a.versions().len(), 4);
    assert_eq!(a.versions(), b.versions());
    assert!(a.engine.conflicts().unwrap().is_empty());
}

#[test]
fn first_sync_with_a_different_card_keeps_both() {
    let remote = Arc::new(MemoryRemote::default());
    let cloud = card_image(30);
    let mut a = Device::new(&remote, "desktop", &cloud, ConflictPolicy::Newest);
    a.sync();
    // A freshly formatted card on the new device, registered later (so it has a newer HLC).
    let blank = vec![0xFFu8; 600 * 1024];
    let mut b = Device::new(&remote, "laptop", &blank, ConflictPolicy::Newest);
    b.sync();
    // Under "newest", a first-sync root never beats a version with history.
    assert_eq!(b.read(), cloud);
    a.sync();
    assert_eq!(a.read(), cloud);
    // The blank card is still in history.
    let blank_id = blake3::hash(&blank).to_hex().to_string();
    assert!(a.engine.history("ps2:Mcd001").unwrap().iter().any(|h| h.version.content == format!("b3:{blank_id}")));
}

#[test]
fn cloned_device_id_is_reminted() {
    let remote = Arc::new(MemoryRemote::default());
    let base = card_image(40);
    let mut a = Device::new(&remote, "phone", &base, ConflictPolicy::Ask);
    a.sync();

    // Restore the phone's app backup onto a second phone: same data dir, same device id.
    let clone_dir = tempfile::tempdir().unwrap();
    let Device { engine, link, card, data, _dir } = a;
    drop(engine);
    copy_dir(&data, &clone_dir.path().join("data"));
    let clone_card = clone_dir.path().join("Mcd001.ps2");
    std::fs::copy(&card, &clone_card).unwrap();
    let mut a = Device { engine: open_engine(&data, "phone", ConflictPolicy::Ask), link, card, data, _dir };
    a.engine.attach_remote(a.link.clone());
    let mut clone_engine = open_engine(&clone_dir.path().join("data"), "phone 2", ConflictPolicy::Ask);
    clone_engine.attach_remote(remote.clone());
    // The copied DB still points at the original card path; re-register the clone's own copy.
    let old = clone_engine.containers().unwrap()[0].id;
    clone_engine.remove_container(old).unwrap();
    clone_engine.add_container(ContainerDesc { location: clone_card.clone(), ..Default::default() }).unwrap();
    assert_eq!(a.engine.device_id().unwrap(), clone_engine.device_id().unwrap());

    a.write(&card_image(41));
    assert!(a.sync().reminted.is_none());
    std::fs::write(&clone_card, card_image(42)).unwrap();
    let r = clone_engine.sync().unwrap();
    let (old_id, new_id) = r.reminted.expect("clone must detect the 412 on its log segment and mint a new id");
    assert_ne!(old_id, new_id);
    assert_eq!(clone_engine.device_id().unwrap(), new_id);
    // From now on the old id is another device's log; the original's edit is a divergence.
    let r = clone_engine.sync().unwrap();
    assert_eq!(r.conflicts.len(), 1);

    // Nothing was overwritten: a third device sees all three versions.
    let mut c = Device::new(&remote, "tablet", &base, ConflictPolicy::Ask);
    c.sync();
    assert_eq!(c.versions().len(), 3);
    assert_all_readable(&c, &c.versions());
    a.sync();
    assert_eq!(a.versions(), c.versions());
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dst = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &dst);
        } else {
            std::fs::copy(e.path(), dst).unwrap();
        }
    }
}

#[test]
fn interrupted_pushes_resume_without_loss_or_duplicates() {
    let remote = Arc::new(MemoryRemote::default());
    let v1 = card_image(50);
    let mut a = Device::new(&remote, "a", &v1, ConflictPolicy::Ask);
    a.sync();

    // Connection drops after a couple of chunk uploads.
    let v2 = card_image(51);
    a.write(&v2);
    remote.fail_puts_after(Some(2));
    let r = a.sync();
    assert!(r.offline);
    assert_eq!(r.pending_uploads, 1);
    remote.fail_puts_after(None);
    let r = a.sync();
    assert_eq!(r.pending_uploads, 0);
    assert_eq!(r.uploaded_versions, 1);

    // The segment write commits on the server but the response is lost. The retry must
    // recognise its own segment instead of treating it as a cloned device.
    let hist = a.engine.history("Mcd001").unwrap();
    let first = hist.last().unwrap().version.id.clone();
    a.engine.restore("Mcd001", &first).unwrap();
    remote.lose_next_put_response();
    let r = a.sync();
    assert!(r.offline);
    let r = a.sync();
    assert!(r.reminted.is_none());
    assert_eq!(r.pending_uploads, 0);
    let me = a.engine.device_id().unwrap();
    let segments = remote.list(&format!("devices/{me}/log/"), None).unwrap();
    assert_eq!(segments.len(), 3, "{segments:?}");

    let mut b = Device::new(&remote, "b", &v1, ConflictPolicy::Ask);
    b.sync();
    assert_eq!(b.read(), v1);
    assert_eq!(b.versions(), a.versions());
}

#[test]
fn busy_container_waits_in_inbox() {
    let remote = Arc::new(MemoryRemote::default());
    let v1 = card_image(60);
    let mut a = Device::new(&remote, "a", &v1, ConflictPolicy::Ask);
    a.sync();
    let mut b = Device::new(&remote, "b", &v1, ConflictPolicy::Ask);
    b.sync();
    let v2 = card_image(61);
    a.write(&v2);
    a.sync();

    let cid = b.engine.containers().unwrap()[0].id;
    b.engine.set_busy(cid, true);
    let r = b.sync();
    assert_eq!(r.blocked.len(), 1);
    assert_eq!(b.read(), v1);
    assert_eq!(b.engine.inbox().unwrap().len(), 1);
    b.engine.set_busy(cid, false);
    let r = b.sync();
    assert_eq!(r.applied.len(), 1);
    assert_eq!(b.read(), v2);
    assert!(b.engine.inbox().unwrap().is_empty());
}

#[test]
fn delayed_listing_and_no_conditional_writes() {
    let remote = Arc::new(MemoryRemote::new(MemoryRemoteOptions { conditional_writes: false, list_delay: 3 }));
    let v1 = card_image(70);
    let mut a = Device::new(&remote, "a", &v1, ConflictPolicy::Ask);
    assert!(!a.engine.conditional_writes());
    a.sync();
    let mut b = Device::new(&remote, "b", b"blank", ConflictPolicy::Newest);
    let mut synced = false;
    for _ in 0..6 {
        b.sync();
        if b.read() == v1 {
            synced = true;
            break;
        }
    }
    assert!(synced, "B must pick up A's segment once the listing catches up");
}

#[test]
fn corrupt_remote_blob_is_never_applied() {
    let remote = Arc::new(MemoryRemote::default());
    let v1 = card_image(80);
    let mut a = Device::new(&remote, "a", &v1, ConflictPolicy::Ask);
    a.sync();
    let mut b = Device::new(&remote, "b", &v1, ConflictPolicy::Ask);
    b.sync();
    let v2 = card_image(81);
    a.write(&v2);
    a.sync();
    for k in remote.keys().into_iter().filter(|k| k.starts_with("blobs/")) {
        let mut blob = remote.raw_get(&k).unwrap();
        let last = blob.len() - 1;
        blob[last] ^= 0xFF;
        remote.raw_put(&k, &blob);
    }
    let r = b.sync();
    assert!(r.applied.is_empty());
    assert_eq!(r.blocked.len(), 1);
    assert_eq!(b.read(), v1, "a container is never written from unverified data");
}

/// Small xorshift RNG so the simulation is reproducible without extra dependencies.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[test]
fn randomized_partitions_never_lose_a_version() {
    // EMUSYNC_SIM_SEEDS=200 for a longer soak.
    let seeds: u64 = std::env::var("EMUSYNC_SIM_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(4);
    for seed in 1..=seeds {
        run_random(seed, 120);
    }
}

fn run_random(seed: u64, steps: usize) {
    let remote = Arc::new(MemoryRemote::default());
    let mut rng = Rng(seed * 0x2545_F491_4F6C_DD1D);
    let base = bytes(4096, seed);
    let mut devs: Vec<Device> =
        (0..3).map(|i| Device::new(&remote, &format!("dev{i}"), &base, ConflictPolicy::Newest)).collect();
    // Content each device held at the moment it synced: these must all be versions.
    let mut captured: HashSet<String> = HashSet::new();
    let mut counter = 0u64;
    for _ in 0..steps {
        let i = rng.below(3) as usize;
        match rng.below(10) {
            0..=3 => {
                counter += 1;
                devs[i].write(&bytes(1024 + rng.below(8192) as usize, seed * 1000 + counter));
            }
            4..=7 => {
                captured.insert(format!("b3:{}", blake3::hash(&devs[i].read()).to_hex()));
                devs[i].sync();
            }
            _ => {
                let up = rng.below(2) == 0;
                devs[i].set_online(up);
            }
        }
    }
    // Heal the partitions and let everyone catch up.
    for d in &devs {
        d.set_online(true);
    }
    for _ in 0..3 {
        for d in devs.iter_mut() {
            captured.insert(format!("b3:{}", blake3::hash(&d.read()).to_hex()));
            d.sync();
        }
    }
    let ids = devs[0].versions();
    for d in &devs {
        assert_eq!(d.versions(), ids, "seed {seed}: devices disagree on the version set");
        assert_eq!(d.tips().len(), 1, "seed {seed}: unresolved tips remain");
        assert_eq!(d.tips(), devs[0].tips());
        assert_eq!(d.read(), devs[0].read(), "seed {seed}: card files differ");
        assert!(d.engine.conflicts().unwrap().is_empty());
        assert_eq!(d.engine.pending_uploads().unwrap(), 0);
    }
    let contents: HashSet<String> =
        devs[0].engine.history("Mcd001").unwrap().into_iter().map(|h| h.version.content).collect();
    for c in &captured {
        assert!(contents.contains(c), "seed {seed}: content {c} was synced but is not in history");
    }
    assert_all_readable(&devs[1], &ids);
}

#[test]
fn a_later_segment_visible_before_an_earlier_one_is_not_skipped() {
    let remote = Arc::new(MemoryRemote::default());
    let mut a = Device::new(&remote, "a", &card_image(90), ConflictPolicy::Ask);
    a.sync();
    a.write(&card_image(91));
    a.sync();
    let mut b = Device::new(&remote, "b", &card_image(90), ConflictPolicy::Ask);
    // Segment 1 of A isn't listed yet, segment 2 is.
    let me = a.engine.device_id().unwrap();
    let seg1 = format!("devices/{me}/log/{:016x}.ndjson", 1);
    let hidden = remote.raw_delete(&seg1).unwrap();
    let r = b.sync();
    assert_eq!(r.pulled_records, 0, "nothing past the gap may be read");
    remote.raw_put(&seg1, &hidden);
    b.sync();
    // B also published its own first-sync version meanwhile; what matters is that segment 1
    // was read once it became visible.
    assert!(a.versions().is_subset(&b.versions()), "a version from segment 1 was skipped");
}

/// A second, distinct repository.
struct OtherRemote(MemoryRemote);

impl Remote for OtherRemote {
    fn describe(&self) -> String {
        "memory://other".into()
    }
    fn put(&self, key: &str, data: &[u8], if_none_match: bool) -> emusync_remote::Result<PutOutcome> {
        self.0.put(key, data, if_none_match)
    }
    fn get(&self, key: &str) -> emusync_remote::Result<Option<Vec<u8>>> {
        self.0.get(key)
    }
    fn head(&self, key: &str) -> emusync_remote::Result<bool> {
        self.0.head(key)
    }
    fn list(&self, prefix: &str, start_after: Option<&str>) -> emusync_remote::Result<Vec<String>> {
        self.0.list(prefix, start_after)
    }
    fn list_dirs(&self, prefix: &str) -> emusync_remote::Result<Vec<String>> {
        self.0.list_dirs(prefix)
    }
}

#[test]
fn a_data_dir_stays_bound_to_its_remote() {
    let remote = Arc::new(MemoryRemote::default());
    let mut a = Device::new(&remote, "a", &card_image(95), ConflictPolicy::Ask);
    a.sync();
    a.engine.add_remote(a.link.clone()).expect("re-adding the same remote is fine");
    let err = a.engine.add_remote(Arc::new(OtherRemote(MemoryRemote::default()))).unwrap_err();
    assert!(err.to_string().contains("already bound"), "{err}");
}

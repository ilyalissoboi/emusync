//! The sync engine (DESIGN §6): local snapshots, push, pull, reconciliation and apply.
//!
//! A logical card syncs in one of two modes (DESIGN §2.1):
//! - **unit mode**: every save on the card is its own unit (`ps2:Mcd001:BASLUS-20062GTA3`),
//!   extracted and rebuilt by the card's format adapter, so saves of different games merge;
//! - **whole-file mode**: the card image is the only unit (`ps2:Mcd001:@card`).

use crate::chunk;
use crate::crypto::{Cipher, EncryptionHeader, KdfParams, RepoKey};
use crate::error::{Error, Result};
use crate::fsutil;
use crate::hlc::Hlc;
use crate::keystore;
use crate::model::{
    content_id, parse_record, split_unit, unit_id, whole_file_unit, CardMode, CardRecord, LogRecord, VersionRecord,
};
use crate::store::{ContainerRow, LocalUnit, Store, StoredVersion, CF_READ_ONLY};
use emusync_formats::{self as formats, ApplyOp, Canonical, ContainerFormat, ContainerKind, FormatError};
use emusync_remote::{create_only, probe_conditional_writes, PutOutcome, Remote, RemoteError};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Local content hash of a unit that is not on the card (deleted, or not placed yet).
const ABSENT: &str = "absent";

/// Reads a container's image: a file's bytes, or a directory as a tree image (DESIGN §19).
/// With `recover`, an interrupted directory replace is completed first.
fn read_image(path: &Path, fmt: &dyn ContainerFormat, recover: bool) -> std::io::Result<Vec<u8>> {
    match fmt.kind() {
        ContainerKind::File => fs::read(path),
        ContainerKind::Directory => {
            if recover {
                fsutil::recover_dir_replace(path)?;
            }
            Ok(fsutil::read_tree(path)?.encode())
        }
    }
}

/// Atomically replaces a container with `image` (DESIGN §5 step 5; §19 for directories).
fn write_image(path: &Path, fmt: &dyn ContainerFormat, image: &[u8]) -> std::io::Result<()> {
    match fmt.kind() {
        ContainerKind::File => fsutil::atomic_replace(path, image),
        ContainerKind::Directory => {
            let tree = formats::tree::Tree::decode(image)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
            fsutil::replace_dir(path, &tree)
        }
    }
}

/// Reason reported for a container another program has locked (see [`fsutil::is_locked`]). It is
/// read again on the next cycle; nothing else in the cycle is affected.
pub const LOCKED: &str = "locked by another program";

/// What to do when a unit has more than one tip (DESIGN §6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConflictPolicy {
    /// Record the conflict and leave the container alone until [`Engine::resolve`].
    #[default]
    Ask,
    /// The tip with the highest HLC wins; a merge version records the choice and the losing
    /// tip stays in history. A first-sync upload never beats a version with history (see
    /// [`newest`]).
    Newest,
}

#[derive(Debug, Clone)]
pub struct EngineOptions {
    /// Shown to other devices. Defaults to the host name.
    pub device_name: Option<String>,
    pub emulator_id: String,
    pub policy: ConflictPolicy,
    /// Local snapshot store budget (DESIGN §7). A unit's latest snapshot is never evicted.
    pub snapshot_budget_bytes: u64,
    /// Whether sync cycles snapshot containers marked busy. The sidecar does: it watches the
    /// files and debounces changes itself. An embedded host reports its quiet points
    /// ([`Engine::snapshot_containers`] from `es_container_touched` / `es_container_flushed`)
    /// and sets this to false, so a cycle never reads a card the emulator may be writing.
    /// Remote changes for a skipped container still go to the inbox.
    pub scan_busy: bool,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            device_name: None,
            emulator_id: "emusync".into(),
            policy: ConflictPolicy::Ask,
            snapshot_budget_bytes: 100 * 1024 * 1024,
            scan_busy: true,
        }
    }
}

/// A container to register (mirrors `es_container_desc`).
#[derive(Debug, Clone, Default)]
pub struct ContainerDesc {
    pub location: PathBuf,
    /// Adapter id; `None` = detected from the file (see [`formats::detect`]).
    pub format: Option<String>,
    /// `None` = inferred from the file extension.
    pub platform: Option<String>,
    /// Logical card name; `None` = the file stem (`Mcd001.ps2` → `Mcd001`).
    pub card: Option<String>,
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub unit: String,
    pub version: String,
    pub container: i64,
    pub location: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocked {
    pub unit: String,
    /// The container that couldn't be read or written.
    pub container: i64,
    pub kind: BlockKind,
    pub reason: String,
}

/// Why a container couldn't be synced or a unit couldn't be applied, for hosts that react
/// differently to each (the C ABI's `ES_EV_APPLY_BLOCKED` status). `reason` has the details.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlockKind {
    /// Not enough free space on the card.
    NoSpace,
    /// The emulator has the container open ([`Engine::set_busy`]), or another program locked it.
    Busy,
    /// The container does not exist.
    Missing,
    /// The container, or the incoming content, failed validation; nothing was written.
    Invalid,
    /// The content could not be fetched (offline, or missing from the remote).
    Unavailable,
    /// The container is read-only, or could not be read or replaced.
    Io,
    /// The card's mode needs an adapter this build lacks.
    Unsupported,
    /// The container changed while the cycle ran; it is snapshotted next cycle.
    Changed,
}

/// Outcome of one sync cycle.
#[derive(Debug, Clone, Default)]
pub struct SyncReport {
    /// The remote was unreachable (or none is configured); local work still happened.
    pub offline: bool,
    pub pulled_records: usize,
    pub new_local_versions: usize,
    pub uploaded_versions: usize,
    pub uploaded_blobs: usize,
    pub applied: Vec<Applied>,
    pub blocked: Vec<Blocked>,
    /// Ids of open conflicts touched in this cycle.
    pub conflicts: Vec<i64>,
    /// Units whose diverged versions the `newest` policy merged in this cycle (the losing
    /// versions stay in history as merge parents).
    pub resolved: Vec<String>,
    /// Cards whose mode another device changed.
    pub mode_changes: Vec<String>,
    /// `(old, new)` when this installation found its device id in use elsewhere and minted a new one.
    pub reminted: Option<(String, String)>,
    /// `(unit, version)` for every version uploaded in this cycle.
    pub uploaded: Vec<(String, String)>,
    /// Log records still waiting to upload.
    pub pending_uploads: usize,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CardStatus {
    pub card_id: String,
    pub mode: CardMode,
    pub container_id: Option<i64>,
    pub location: Option<String>,
    /// The card's units: every save in unit mode, the single `@card` unit in whole-file mode.
    pub units: Vec<UnitStatus>,
}

#[derive(Debug, Clone)]
pub struct UnitStatus {
    pub unit: String,
    /// Name inside the card (`BASLUS-20062GTA3`, or `@card`).
    pub name: String,
    pub titles: Vec<String>,
    /// Whether the unit is on this device's card.
    pub present: bool,
    /// The version this device's card holds (or last held, if the unit was deleted here).
    pub local_version: Option<String>,
    pub tips: Vec<String>,
    /// Every tip is a deletion.
    pub deleted: bool,
    pub conflict: Option<i64>,
    pub inbox: Option<String>,
}

impl UnitStatus {
    /// One-word state for display.
    pub fn state(&self) -> &'static str {
        if self.conflict.is_some() {
            "conflict"
        } else if self.inbox.is_some() {
            "waiting"
        } else if self.deleted && !self.present {
            "deleted"
        } else if self.local_version.is_none() {
            if self.present {
                "not synced yet"
            } else {
                "not on this device"
            }
        } else if self.tips.len() == 1 && self.local_version.as_ref() == self.tips.first() {
            "in sync"
        } else {
            "behind"
        }
    }
}

#[derive(Debug, Clone)]
pub struct HistoryEntry {
    pub version: VersionRecord,
    pub device: String,
    pub is_tip: bool,
    pub is_local: bool,
}

#[derive(Debug, Clone)]
pub struct ConflictInfo {
    pub id: i64,
    pub unit: String,
    pub location: Option<String>,
    pub local: Option<HistoryEntry>,
    pub tips: Vec<HistoryEntry>,
}

#[derive(Debug, Clone)]
pub enum Resolution {
    KeepLocal,
    /// Take the tip whose id starts with this prefix.
    Take(String),
}

/// What [`Engine::prepare_remote`] found out about a remote.
#[derive(Debug, Clone)]
pub struct RemoteSetup {
    pub uri: String,
    pub conditional_writes: bool,
    /// `repo_id` from `repo.json`.
    pub repo_id: Option<String>,
    /// `encryption` from `repo.json`: `Some` for an encrypted repository.
    pub encryption: Option<EncryptionHeader>,
    /// The repository key, if the setup had the passphrase
    /// ([`Engine::prepare_remote_encrypted`]).
    pub key: Option<RepoKey>,
}

/// Whether the bound repository is encrypted, and whether this device can read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encryption {
    /// Not encrypted (or no remote yet).
    None,
    /// Encrypted, and this device doesn't have the key: [`Engine::unlock`] with the passphrase.
    Locked,
    Unlocked,
}

/// `repo.json` format of an unencrypted repository; older versions of emusync know only this.
const FORMAT_PLAIN: u64 = 1;
/// `repo.json` format of an encrypted repository. Versions without encryption refuse it
/// ("unsupported repository format"), so they never write plaintext into it.
const FORMAT_ENCRYPTED: u64 = 2;

/// Parses `repo.json`: the repository id and its encryption header, if any.
fn parse_repo_json(body: &[u8]) -> Result<(Option<String>, Option<EncryptionHeader>)> {
    let v: serde_json::Value = serde_json::from_slice(body)?;
    let id = v.get("repo_id").and_then(|i| i.as_str()).map(str::to_string);
    let enc = v.get("encryption").filter(|e| !e.is_null());
    match v.get("format").and_then(|f| f.as_u64()) {
        Some(FORMAT_PLAIN) if enc.is_none() => Ok((id, None)),
        Some(FORMAT_ENCRYPTED) => {
            let header: EncryptionHeader = enc
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .map_err(|e| Error::Incompatible(format!("repo.json: unreadable encryption header: {e}")))?
                .ok_or_else(|| Error::Incompatible("repo.json: format 2 without an encryption header".into()))?;
            header.check()?;
            let id = id.ok_or_else(|| Error::Incompatible("repo.json: encrypted repository without an id".into()))?;
            Ok((Some(id), Some(header)))
        }
        Some(f) if f > FORMAT_ENCRYPTED => Err(Error::Incompatible(format!(
            "the repository has format {f}, newer than this version of emusync supports; update emusync"
        ))),
        _ => Err(Error::Incompatible(format!("unsupported repository format in repo.json: {v}"))),
    }
}

#[derive(Serialize, Deserialize)]
struct PendingSegment {
    body: String,
    last_outbox_id: i64,
}

pub struct Engine {
    dir: PathBuf,
    store: Store,
    remote: Option<Arc<dyn Remote>>,
    conditional_writes: bool,
    opts: EngineOptions,
    busy: BusyGate,
    /// `Some` when the bound repository is encrypted.
    encryption: Option<EncryptionHeader>,
    /// Its key, once this device has it.
    cipher: Option<Arc<Cipher>>,
}

/// The containers the emulator has open ([`Engine::set_busy`]), shareable with other threads.
/// The engine holds the gate while it replaces a container, so once [`BusyGate::set`] marks a
/// container busy, the engine does not write to it any more.
#[derive(Clone, Default)]
pub struct BusyGate(Arc<Mutex<HashSet<i64>>>);

impl BusyGate {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashSet<i64>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Marks a container as open or closed. Waits for a write to it that is in progress.
    pub fn set(&self, container: i64, busy: bool) {
        let mut set = self.lock();
        if busy {
            set.insert(container);
        } else {
            set.remove(&container);
        }
    }

    pub fn contains(&self, container: i64) -> bool {
        self.lock().contains(&container)
    }

    /// Runs `write` while holding the gate, unless the container is busy (`None`).
    fn write_unless_busy<T>(&self, container: i64, write: impl FnOnce() -> T) -> Option<T> {
        let set = self.lock();
        if set.contains(&container) {
            return None;
        }
        Some(write())
    }
}

fn segment_key(device: &str, seq: u64) -> String {
    format!("devices/{device}/log/{seq:016x}.ndjson")
}

/// Sequence number of a log segment key (`…/log/<seq:016x>.ndjson`).
fn segment_seq(key: &str) -> Option<u64> {
    let name = key.rsplit('/').next()?.strip_suffix(".ndjson")?;
    if name.len() != 16 {
        return None;
    }
    u64::from_str_radix(name, 16).ok()
}

fn default_device_name() -> String {
    ["COMPUTERNAME", "HOSTNAME"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .unwrap_or_else(|| "unknown device".into())
}

/// Winner under [`ConflictPolicy::Newest`]. A tip with history beats a root (a first-sync
/// upload); among tips with history the highest HLC wins; among roots the oldest wins, since the
/// newer root is the device that joined later. Ties break on the version id, so every device
/// picks the same winner.
fn newest(tips: &[StoredVersion]) -> &StoredVersion {
    tips.iter()
        .max_by(|a, b| {
            let key = |v: &StoredVersion| {
                let has_history = !v.rec.parents.is_empty();
                let t = (v.rec.hlc.ms, v.rec.hlc.counter as i64);
                (has_history, if has_history { t } else { (-t.0, -t.1) })
            };
            key(a).cmp(&key(b)).then_with(|| a.rec.id.cmp(&b.rec.id))
        })
        .unwrap()
}

fn hex_of(id: &str) -> &str {
    id.strip_prefix("b3:").unwrap_or(id)
}

impl Engine {
    /// Opens (or initialises) the local state in `data_dir`.
    pub fn open(data_dir: impl AsRef<Path>, opts: EngineOptions) -> Result<Self> {
        let dir = data_dir.as_ref().to_path_buf();
        fs::create_dir_all(dir.join("staging"))?;
        fs::create_dir_all(dir.join("snapshots"))?;
        let store = Store::open(&dir.join("state.db"))?;
        if store.meta("device_id")?.is_none() {
            store.set_meta("device_id", &uuid::Uuid::now_v7().to_string())?;
            store.set_meta("next_seq", "1")?;
        }
        let name = match &opts.device_name {
            Some(n) => n.clone(),
            None => store.meta("device_name")?.unwrap_or_else(default_device_name),
        };
        store.set_meta("device_name", &name)?;
        let me = store.meta("device_id")?.unwrap();
        store.set_device_name(&me, &name)?;
        let conditional_writes = store.meta("conditional_writes")?.as_deref() == Some("1");
        let encryption: Option<EncryptionHeader> = match store.meta("encryption")? {
            Some(h) => Some(serde_json::from_str(&h)?),
            None => None,
        };
        let mut cipher = None;
        if encryption.is_some() {
            let id =
                store.meta("repo_id")?.ok_or_else(|| Error::Integrity("encrypted repository without an id".into()))?;
            cipher = keystore::load(&dir, &id)?.map(|k| Arc::new(Cipher::new(&k)));
        }
        Ok(Self { dir, store, remote: None, conditional_writes, opts, busy: BusyGate::default(), encryption, cipher })
    }

    pub fn device_id(&self) -> Result<String> {
        Ok(self.store.meta("device_id")?.expect("device id is set at open"))
    }

    pub fn device_name(&self) -> Result<String> {
        Ok(self.store.meta("device_name")?.unwrap_or_default())
    }

    pub fn set_policy(&mut self, policy: ConflictPolicy) {
        self.opts.policy = policy;
    }

    // ------------------------------------------------------------------------------------------
    // Remote
    // ------------------------------------------------------------------------------------------

    /// Registers a remote (DESIGN §6.2 setup): validates access with a LIST, probes
    /// conditional-write support once, and creates `repo.json` if the repository is new.
    /// Blocks on the network.
    ///
    /// A data directory belongs to one repository: its cursors, known blobs and drained outbox
    /// describe that repository only. Adding a different remote is refused; use a new data
    /// directory for it. Re-adding the same remote is allowed.
    pub fn add_remote(&mut self, remote: Arc<dyn Remote>) -> Result<bool> {
        self.check_remote(&remote.describe())?;
        let setup = Self::prepare_remote(&*remote)?;
        self.bind_remote(remote, setup)
    }

    /// Fails if this data directory is bound to a remote other than `uri` (see
    /// [`Engine::add_remote`]).
    pub fn check_remote(&self, uri: &str) -> Result<()> {
        match self.store.meta("remote")? {
            Some(existing) if existing != uri => Err(Error::InvalidArg(format!(
                "this data directory is already bound to {existing}; use a new data directory for {uri}"
            ))),
            _ => Ok(()),
        }
    }

    /// Like [`Engine::add_remote`], with a passphrase: a new repository is created encrypted
    /// (DESIGN §7), an existing encrypted one is unlocked, and an existing unencrypted one is
    /// refused ([`Error::Incompatible`]).
    pub fn add_remote_encrypted(&mut self, remote: Arc<dyn Remote>, passphrase: &str, kdf: KdfParams) -> Result<bool> {
        self.check_remote(&remote.describe())?;
        let setup = Self::prepare_remote_encrypted(&*remote, passphrase, kdf)?;
        self.bind_remote(remote, setup)
    }

    /// The network half of [`Engine::add_remote`], which needs no engine, so a host can run it
    /// on another thread: validates access, probes conditional writes, and creates `repo.json`
    /// if the repository is new. Finish with [`Engine::bind_remote`].
    ///
    /// An existing encrypted repository is reported in [`RemoteSetup::encryption`]; unless
    /// this data directory already has its key, it is bound locked.
    pub fn prepare_remote(remote: &dyn Remote) -> Result<RemoteSetup> {
        Self::prepare(remote, None)
    }

    /// [`Engine::prepare_remote`] for [`Engine::add_remote_encrypted`]. Runs Argon2id (`kdf`:
    /// under 0.1 s on a desktop CPU with the defaults). [`Error::WrongPassphrase`] if the repository
    /// exists and the passphrase doesn't unlock it.
    pub fn prepare_remote_encrypted(remote: &dyn Remote, passphrase: &str, kdf: KdfParams) -> Result<RemoteSetup> {
        if passphrase.is_empty() {
            return Err(Error::InvalidArg("the passphrase is empty".into()));
        }
        Self::prepare(remote, Some((passphrase, kdf)))
    }

    fn prepare(remote: &dyn Remote, encrypt: Option<(&str, KdfParams)>) -> Result<RemoteSetup> {
        remote.list_dirs("devices/")?;
        let cond = probe_conditional_writes(remote)?;
        let uri = remote.describe();
        loop {
            if let Some(body) = remote.get("repo.json")? {
                let (repo_id, encryption) = parse_repo_json(&body)?;
                let key = match (&encryption, encrypt) {
                    (None, Some(_)) => {
                        return Err(Error::Incompatible(format!(
                            "{uri} already holds an unencrypted repository; encryption is chosen when a \
                             repository is created (use a new, empty prefix for an encrypted one)"
                        )))
                    }
                    (Some(h), Some((pass, _))) => {
                        Some(RepoKey::unwrap(h, pass, repo_id.as_deref().unwrap_or_default())?)
                    }
                    _ => None,
                };
                return Ok(RemoteSetup { uri, conditional_writes: cond, repo_id, encryption, key });
            }
            let repo_id = uuid::Uuid::now_v7().to_string();
            let mut setup = RemoteSetup {
                uri: uri.clone(),
                conditional_writes: cond,
                repo_id: Some(repo_id.clone()),
                encryption: None,
                key: None,
            };
            let repo = match encrypt {
                None => serde_json::json!({ "format": FORMAT_PLAIN, "repo_id": repo_id, "encryption": null }),
                Some((pass, kdf)) => {
                    let key = RepoKey::generate();
                    let header = key.wrap(pass, kdf, &repo_id)?;
                    let repo =
                        serde_json::json!({ "format": FORMAT_ENCRYPTED, "repo_id": repo_id, "encryption": header });
                    setup.encryption = Some(header);
                    setup.key = Some(key);
                    repo
                }
            };
            match create_only(remote, "repo.json", repo.to_string().as_bytes(), cond)? {
                PutOutcome::Created => return Ok(setup),
                // Another device created it first: use theirs.
                PutOutcome::AlreadyExists => continue,
            }
        }
    }

    /// The local half of [`Engine::add_remote`]: binds this data directory to the remote that
    /// [`Engine::prepare_remote`] set up. Returns whether it supports conditional writes.
    ///
    /// For an encrypted repository, the key in `setup` is kept on this device (DESIGN §22);
    /// without one, the key kept earlier is used, or the repository stays locked
    /// ([`Engine::encryption`]).
    pub fn bind_remote(&mut self, remote: Arc<dyn Remote>, setup: RemoteSetup) -> Result<bool> {
        self.check_remote(&setup.uri)?;
        if let (Some(old), Some(new)) = (self.store.meta("repo_id")?, &setup.repo_id) {
            if old != *new {
                return Err(Error::Incompatible(format!(
                    "the repository at {} was replaced by another one (id {new}, was {old}); use a new data \
                     directory for it",
                    setup.uri
                )));
            }
            // repo.json is create-only, so its encryption never changes. If it reads differently
            // now, someone rewrote it (to make this device upload plaintext, say): refuse it and
            // keep what was bound.
            if setup.encryption != self.encryption {
                let change = match (&self.encryption, &setup.encryption) {
                    (Some(_), None) => "it was encrypted and now says it is not",
                    (None, Some(_)) => "it was not encrypted and now says it is",
                    _ => "its encryption header is different",
                };
                return Err(Error::Incompatible(format!(
                    "repo.json at {} changed since this data directory was bound to it ({change}); refusing it",
                    setup.uri
                )));
            }
        }
        let cipher = match (&setup.encryption, &setup.repo_id) {
            (Some(_), Some(id)) => {
                let key = match &setup.key {
                    Some(k) => {
                        keystore::save(&self.dir, id, k)?;
                        Some(k.clone())
                    }
                    None => keystore::load(&self.dir, id)?,
                };
                key.map(|k| Arc::new(Cipher::new(&k)))
            }
            _ => None,
        };
        let cond = setup.conditional_writes;
        let header = setup.encryption.as_ref().map(serde_json::to_string).transpose()?;
        self.store.tx(|| {
            self.store.set_meta("remote", &setup.uri)?;
            self.store.set_meta("conditional_writes", if cond { "1" } else { "0" })?;
            if let Some(id) = &setup.repo_id {
                self.store.set_meta("repo_id", id)?;
            }
            match &header {
                Some(h) => self.store.set_meta("encryption", h),
                None => self.store.delete_meta("encryption"),
            }
        })?;
        self.conditional_writes = cond;
        self.encryption = setup.encryption;
        self.cipher = cipher;
        self.remote = Some(remote);
        Ok(cond)
    }

    /// Whether the bound repository is encrypted, and whether this device has its key.
    pub fn encryption(&self) -> Encryption {
        match (&self.encryption, &self.cipher) {
            (None, _) => Encryption::None,
            (Some(_), None) => Encryption::Locked,
            (Some(_), Some(_)) => Encryption::Unlocked,
        }
    }

    /// Unlocks the bound encrypted repository with its passphrase and keeps the key on this
    /// device (DESIGN §22), so the passphrase is needed only once. Runs Argon2id; no network.
    /// [`Error::WrongPassphrase`], or [`Error::InvalidArg`] if the repository isn't encrypted
    /// or hasn't been set up yet.
    pub fn unlock(&mut self, passphrase: &str) -> Result<()> {
        let Some(header) = &self.encryption else {
            return Err(Error::InvalidArg(match self.store.meta("repo_id")?.and(self.remote_uri()?) {
                Some(uri) => format!("the repository at {uri} is not encrypted"),
                None => "no remote has been set up yet".into(),
            }));
        };
        let id =
            self.store.meta("repo_id")?.ok_or_else(|| Error::Integrity("encrypted repository without an id".into()))?;
        let key = RepoKey::unwrap(header, passphrase, &id)?;
        keystore::save(&self.dir, &id, &key)?;
        self.cipher = Some(Arc::new(Cipher::new(&key)));
        Ok(())
    }

    /// Deletes this device's copy of the repository key: the repository is locked until the
    /// next [`Engine::unlock`].
    pub fn forget_key(&mut self) -> Result<()> {
        if let Some(id) = self.store.meta("repo_id")? {
            keystore::remove(&self.dir, &id)?;
        }
        self.cipher = None;
        Ok(())
    }

    /// The cipher for remote objects: `None` for an unencrypted repository, [`Error::Locked`]
    /// if this device doesn't have the key.
    fn cipher(&self) -> Result<Option<&Cipher>> {
        match (&self.encryption, &self.cipher) {
            (None, _) => Ok(None),
            (Some(_), Some(c)) => Ok(Some(c)),
            (Some(_), None) => Err(Error::Locked),
        }
    }

    /// Attaches a remote for this session without network setup, using the cached
    /// conditional-write probe (or HEAD-before-PUT if it was never probed).
    pub fn attach_remote(&mut self, remote: Arc<dyn Remote>) {
        self.remote = Some(remote);
    }

    /// The remote registered with [`Engine::add_remote`], as described by the backend.
    pub fn remote_uri(&self) -> Result<Option<String>> {
        self.store.meta("remote")
    }

    pub fn conditional_writes(&self) -> bool {
        self.conditional_writes
    }

    // ------------------------------------------------------------------------------------------
    // Containers and cards
    // ------------------------------------------------------------------------------------------

    /// Registers a container for a logical card. Idempotent per location; re-adding a known
    /// location updates its format if detection now finds a better adapter (a card registered
    /// as `raw` by M0 becomes `ps2.card`). Publishes the card (in unit mode when the format has
    /// an adapter, else whole-file mode) unless the repository already knows it, in which case
    /// the repository's mode is adopted.
    ///
    /// A directory is registered as a PCSX2 folder card (`ps2.card.folder`, platform `ps2`) if
    /// it has a `_pcsx2_superblock`, and refused otherwise.
    pub fn add_container(&mut self, desc: ContainerDesc) -> Result<i64> {
        let location = std::path::absolute(&desc.location)?;
        let loc = location.to_string_lossy().into_owned();
        let file_name = location
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| Error::InvalidArg(format!("{loc} has no file name")))?;
        let is_dir = location.is_dir();
        let format = match &desc.format {
            Some(f) => f.clone(),
            None if is_dir => {
                let image = fsutil::read_tree(&location)?.encode();
                formats::detect_dir(&image).map(|f| f.id().to_string()).ok_or_else(|| {
                    Error::InvalidArg(format!(
                        "{loc} is a directory but not a PCSX2 folder memory card (it has no _pcsx2_superblock)"
                    ))
                })?
            }
            None => match fs::read(&location) {
                Ok(image) => formats::detect(&file_name, &image).id().to_string(),
                Err(_) if formats::infer_platform(&file_name) == "ps2" => "ps2.card".into(),
                Err(_) => "raw".into(),
            },
        };
        let fmt = formats::by_id(&format)
            .ok_or_else(|| Error::InvalidArg(format!("format {format:?} is not available in this build")))?;
        if location.exists() && is_dir != (fmt.kind() == ContainerKind::Directory) {
            return Err(Error::InvalidArg(format!(
                "{loc} is {}, which format {format:?} can't use",
                if is_dir { "a directory" } else { "a file" }
            )));
        }
        let containers = self.store.containers()?;
        if let Some(c) = containers.iter().find(|c| c.location == loc) {
            if c.format != format {
                if self.card_mode(&c.card_id)? == CardMode::Units && !fmt.supports_units() {
                    return Err(Error::InvalidArg(format!(
                        "card {} syncs per save, which format {format:?} can't do",
                        c.card_id
                    )));
                }
                self.store.set_container_format(c.id, &format)?;
            }
            return Ok(c.id);
        }
        let platform = desc.platform.clone().unwrap_or_else(|| match fmt.id() {
            "ps2.card.folder" => "ps2".to_string(),
            _ => formats::infer_platform(&file_name).to_string(),
        });
        let name = match &desc.card {
            Some(n) => n.clone(),
            None => location.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
        };
        for (what, v) in [("platform", &platform), ("card name", &name)] {
            if v.is_empty() || v.contains(':') || v.contains('/') || v.contains(char::is_whitespace) {
                return Err(Error::InvalidArg(format!("invalid {what} {v:?}")));
            }
        }
        let card_id = format!("{platform}:{name}");
        if let Some(c) = containers.iter().find(|c| c.card_id == card_id) {
            return Err(Error::InvalidArg(format!(
                "card {card_id} already has a container on this device ({}); pick another --name",
                c.location
            )));
        }
        let flags = if desc.read_only { CF_READ_ONLY } else { 0 };
        self.store.tx(|| {
            match self.store.card(&card_id)? {
                Some((CardMode::Units, _)) if !fmt.supports_units() => {
                    return Err(Error::InvalidArg(format!(
                        "card {card_id} is in unit mode in the repository, which needs an adapter this build lacks"
                    )))
                }
                Some(_) => {}
                None => {
                    let mode = if fmt.supports_units() { CardMode::Units } else { CardMode::WholeFile };
                    self.publish_card(&card_id, mode)?;
                }
            }
            self.store.insert_container(&loc, &format, &card_id, flags)
        })
    }

    fn publish_card(&self, card_id: &str, mode: CardMode) -> Result<()> {
        let hlc = self.tick()?;
        self.store.set_card(card_id, mode, hlc)?;
        let rec = LogRecord::Card(CardRecord { card: card_id.to_string(), mode, hlc });
        self.store.outbox_push(&rec.to_line())
    }

    /// Switches a logical card between unit and whole-file mode for every device (DESIGN
    /// §2.1). Local changes are first captured in the old mode, so the switch loses nothing.
    /// Run [`Engine::sync`] to publish it.
    pub fn set_card_mode(&mut self, card: &str, mode: CardMode) -> Result<()> {
        let card_id = self.resolve_card(card)?;
        if self.card_mode(&card_id)? == mode {
            return Ok(());
        }
        let containers: Vec<ContainerRow> =
            self.store.containers()?.into_iter().filter(|c| c.card_id == card_id).collect();
        if mode == CardMode::Units {
            for c in &containers {
                if !self.format_of(c)?.supports_units() {
                    return Err(Error::InvalidArg(format!(
                        "{} is not in a format emusync can split into saves ({})",
                        c.location, c.format
                    )));
                }
            }
        }
        if containers.iter().any(|c| self.busy.contains(c.id)) {
            return Err(Error::InvalidArg(format!("card {card_id} is in use")));
        }
        let mut report = SyncReport::default();
        for c in &containers {
            self.scan(c, &mut report)?;
        }
        self.store.tx(|| self.publish_card(&card_id, mode))
    }

    /// The mode of a card, by card id (`ps2:Mcd001`). A card the repository doesn't know
    /// yet is in whole-file mode.
    pub fn card_mode(&self, card_id: &str) -> Result<CardMode> {
        Ok(self.store.card(card_id)?.map_or(CardMode::WholeFile, |(m, _)| m))
    }

    /// Every card the repository knows, with its mode.
    pub fn card_modes(&self) -> Result<Vec<(String, CardMode)>> {
        self.store.cards()
    }

    pub fn remove_container(&mut self, id: i64) -> Result<()> {
        self.store.tx(|| self.store.delete_container(id))
    }

    /// Marks a container as open by the emulator. Results for busy containers go to the inbox
    /// and are applied by the first sync after it is released.
    pub fn set_busy(&mut self, container: i64, busy: bool) {
        self.busy.set(container, busy);
    }

    /// A handle on the busy set for other threads (see [`BusyGate`]).
    pub fn busy_gate(&self) -> BusyGate {
        self.busy.clone()
    }

    pub fn containers(&self) -> Result<Vec<ContainerRow>> {
        self.store.containers()
    }

    /// Units of a card: the `@card` unit in whole-file mode; in unit mode, every save known
    /// from the repository or from this device's container.
    fn units_of_card(&self, card_id: &str, mode: CardMode, container: Option<i64>) -> Result<Vec<String>> {
        let card_unit = whole_file_unit(card_id);
        if mode == CardMode::WholeFile {
            return Ok(vec![card_unit]);
        }
        let prefix = format!("{card_id}:");
        let mut set: BTreeSet<String> = self.store.units_with_prefix(&prefix)?.into_iter().collect();
        if let Some(id) = container {
            for (u, _) in self.store.local_units_of(id)? {
                if u.starts_with(&prefix) {
                    set.insert(u);
                }
            }
        }
        set.remove(&card_unit);
        Ok(set.into_iter().collect())
    }

    pub fn cards(&self) -> Result<Vec<CardStatus>> {
        let containers = self.store.containers()?;
        let conflicts = self.store.open_conflicts()?;
        let inbox = self.store.inbox()?;
        let mut out = Vec::new();
        for (card_id, mode) in self.store.cards()? {
            let c = containers.iter().find(|c| c.card_id == card_id);
            // What the card file holds right now, so saves that haven't synced yet show up too.
            let on_card: Option<HashMap<String, Vec<String>>> = match (c, mode) {
                (Some(c), CardMode::Units) => self.format_of(c).ok().and_then(|fmt| {
                    let image = read_image(Path::new(&c.location), fmt, false).ok()?;
                    let units = fmt.extract_all(&image).ok()?;
                    Some(units.into_iter().map(|u| (unit_id(&card_id, &u.name), fmt.titles(&u.name))).collect())
                }),
                _ => None,
            };
            let mut all: BTreeSet<String> = self.units_of_card(&card_id, mode, c.map(|c| c.id))?.into_iter().collect();
            all.extend(on_card.iter().flat_map(|m| m.keys().cloned()));
            let mut units = Vec::new();
            for unit in all {
                let lu = match c {
                    Some(c) => self.store.local_unit(&unit, c.id)?,
                    None => None,
                };
                let tips = self.tips(&unit)?;
                let present = match &on_card {
                    Some(m) => m.contains_key(&unit),
                    None => lu.as_ref().is_some_and(|l| l.content_hash != ABSENT),
                };
                let titles = match tips.last() {
                    Some(t) => t.rec.titles.clone(),
                    None => on_card.as_ref().and_then(|m| m.get(&unit).cloned()).unwrap_or_default(),
                };
                units.push(UnitStatus {
                    name: split_unit(&unit).map(|(_, n)| n.to_string()).unwrap_or_default(),
                    titles,
                    present,
                    local_version: lu.and_then(|l| l.base),
                    deleted: !tips.is_empty() && tips.iter().all(|t| t.rec.tombstone),
                    tips: tips.into_iter().map(|t| t.rec.id).collect(),
                    conflict: conflicts.iter().find(|x| x.unit_id == unit).map(|x| x.id),
                    inbox: inbox.iter().find(|i| i.0 == unit).map(|i| i.3.clone()),
                    unit,
                });
            }
            out.push(CardStatus {
                card_id,
                mode,
                container_id: c.map(|c| c.id),
                location: c.map(|c| c.location.clone()),
                units,
            });
        }
        Ok(out)
    }

    // ------------------------------------------------------------------------------------------
    // Sync cycle
    // ------------------------------------------------------------------------------------------

    /// One full cycle: pull, snapshot local changes, reconcile and apply, push.
    ///
    /// Pulling first lets a first sync adopt identical remote content instead of publishing a
    /// duplicate. Without a reachable remote the local steps still run and versions wait in the
    /// outbox.
    pub fn sync(&mut self) -> Result<SyncReport> {
        self.cycle(true)
    }

    /// [`Engine::sync`] without the push: pull, snapshot, reconcile and apply. For a session
    /// that is about to start, where only the incoming changes matter; local changes wait in
    /// the outbox for the next push.
    pub fn pull_and_apply(&mut self) -> Result<SyncReport> {
        self.cycle(false)
    }

    /// Quiescent snapshot only (DESIGN §6.4): local changes in `containers` (or every
    /// container) become versions in the outbox. No network, nothing is written to a container.
    pub fn snapshot_containers(&mut self, containers: Option<&[i64]>) -> Result<SyncReport> {
        let mut report = SyncReport::default();
        for c in self.store.containers()? {
            if containers.map_or(true, |ids| ids.contains(&c.id)) {
                self.scan(&c, &mut report)?;
            }
        }
        report.offline = true;
        report.pending_uploads = self.store.outbox()?.len();
        Ok(report)
    }

    /// Uploads the outbox, if a remote is attached and reachable. `offline` in the report is
    /// set when it isn't; the records stay queued.
    pub fn push_pending(&mut self) -> Result<SyncReport> {
        let mut report = SyncReport { offline: true, ..Default::default() };
        if let Some(r) = self.remote.clone() {
            match self.push(&*r, &mut report) {
                Ok(()) => report.offline = false,
                Err(e) if e.is_offline() => log::info!("push failed, will retry: {e}"),
                Err(e) => return Err(e),
            }
        }
        report.pending_uploads = self.store.outbox()?.len();
        Ok(report)
    }

    fn cycle(&mut self, push: bool) -> Result<SyncReport> {
        let mut report = SyncReport::default();
        let remote = self.remote.clone();
        let mut online = remote.is_some();
        if let Some(r) = &remote {
            if let Err(e) = self.pull(&**r, &mut report) {
                if !e.is_offline() {
                    return Err(e);
                }
                log::info!("pull failed, continuing offline: {e}");
                online = false;
            }
        }
        let containers = self.store.containers()?;
        let mut scanned = HashMap::new();
        for c in &containers {
            if !self.opts.scan_busy && self.busy.contains(c.id) {
                // Not read: reconciled against what the last snapshot recorded. The empty image
                // hash never matches, so `apply` writes nothing even if the container is no
                // longer busy by then; changes for it wait in the inbox.
                scanned.insert(c.id, String::new());
                continue;
            }
            if let Some(image_hash) = self.scan(c, &mut report)? {
                scanned.insert(c.id, image_hash);
            }
        }
        for c in &containers {
            // A container that couldn't be read or validated is never written to.
            if let Some(image_hash) = scanned.get(&c.id) {
                self.reconcile(c, image_hash, online, &mut report)?;
            }
        }
        if online && push {
            if let Some(r) = &remote {
                if let Err(e) = self.push(&**r, &mut report) {
                    if !e.is_offline() {
                        return Err(e);
                    }
                    log::info!("push failed, will retry: {e}");
                    online = false;
                }
            }
        }
        report.offline = !online;
        report.pending_uploads = self.store.outbox()?.len();
        self.evict_snapshots()?;
        Ok(report)
    }

    fn format_of(&self, c: &ContainerRow) -> Result<&'static dyn ContainerFormat> {
        formats::by_id(&c.format).ok_or_else(|| Error::InvalidArg(format!("unknown format {:?}", c.format)))
    }

    /// Whether this build can sync the card in the repository's mode.
    fn card_syncable(&self, c: &ContainerRow) -> Result<bool> {
        let fmt = self.format_of(c)?;
        Ok(!(self.card_mode(&c.card_id)? == CardMode::Units && !fmt.supports_units()))
    }

    /// Quiescent snapshot of one container: every unit whose content changed since its base
    /// becomes a new local version (parent = base), queued in the outbox. Returns the hash of
    /// the image that was scanned, or `None` if the container can't be synced right now.
    fn scan(&self, c: &ContainerRow, report: &mut SyncReport) -> Result<Option<String>> {
        let card_unit = whole_file_unit(&c.card_id);
        if !self.card_syncable(c)? {
            report.blocked.push(Blocked {
                unit: card_unit,
                container: c.id,
                kind: BlockKind::Unsupported,
                reason: format!("card syncs per save, which this build can't do for format {:?}", c.format),
            });
            return Ok(None);
        }
        let path = Path::new(&c.location);
        let fmt = self.format_of(c)?;
        // An interrupted replace of a folder card is finished first, unless the emulator has it.
        let image = match read_image(path, fmt, !self.busy.contains(c.id)) {
            Ok(b) => b,
            Err(e) if fsutil::is_locked(&e) => {
                report.blocked.push(Blocked {
                    unit: card_unit,
                    container: c.id,
                    kind: BlockKind::Busy,
                    reason: LOCKED.into(),
                });
                return Ok(None);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let reason = format!("{} does not exist", c.location);
                report.blocked.push(Blocked { unit: card_unit, container: c.id, kind: BlockKind::Missing, reason });
                return Ok(None);
            }
            Err(e) if fmt.kind() == ContainerKind::Directory => {
                let reason = format!("cannot read {}: {e}", c.location);
                report.blocked.push(Blocked { unit: card_unit, container: c.id, kind: BlockKind::Io, reason });
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
        if let Err(e) = fmt.validate(&image) {
            // A container that fails validation is never synced, so corruption doesn't spread.
            let reason = e.to_string();
            report.blocked.push(Blocked { unit: card_unit, container: c.id, kind: BlockKind::Invalid, reason });
            return Ok(None);
        }
        match self.card_mode(&c.card_id)? {
            CardMode::WholeFile => {
                let mut meta = BTreeMap::new();
                if let Some(name) = path.file_name() {
                    meta.insert("file".to_string(), name.to_string_lossy().into_owned());
                }
                if let Ok(modified) = fs::metadata(path).and_then(|m| m.modified()) {
                    if let Ok(d) = modified.duration_since(std::time::UNIX_EPOCH) {
                        let s = Hlc { ms: d.as_millis() as i64, counter: 0 }.to_string();
                        meta.insert("mtime".to_string(), s.split('/').next().unwrap_or_default().to_string());
                    }
                }
                self.snapshot_unit(c, &card_unit, &image, meta, vec![], report)?;
            }
            CardMode::Units => {
                let units = match fmt.extract_all(&image) {
                    Ok(u) => u,
                    Err(e) => {
                        let reason = e.to_string();
                        report.blocked.push(Blocked {
                            unit: card_unit,
                            container: c.id,
                            kind: BlockKind::Invalid,
                            reason,
                        });
                        return Ok(None);
                    }
                };
                let mut present = HashSet::new();
                for canon in units {
                    let unit = unit_id(&c.card_id, &canon.name);
                    let titles = fmt.titles(&canon.name);
                    self.snapshot_unit(c, &unit, &canon.bytes, canon.meta, titles, report)?;
                    present.insert(unit);
                }
                self.scan_deletions(c, &present, report)?;
            }
        }
        Ok(Some(content_id(&image)))
    }

    /// Saves that were on the card at the last sync and are gone now. Normally each becomes a
    /// deletion (tombstone) that syncs to other devices. If every save vanished at once (two or
    /// more), the card was most likely replaced or reformatted: the saves are restored from
    /// the cloud instead of being deleted everywhere.
    fn scan_deletions(&self, c: &ContainerRow, present: &HashSet<String>, report: &mut SyncReport) -> Result<()> {
        let prefix = format!("{}:", c.card_id);
        let card_unit = whole_file_unit(&c.card_id);
        let gone: Vec<(String, LocalUnit)> = self
            .store
            .local_units_of(c.id)?
            .into_iter()
            .filter(|(u, lu)| {
                u.starts_with(&prefix) && *u != card_unit && lu.content_hash != ABSENT && !present.contains(u)
            })
            .collect();
        if present.is_empty() && gone.len() >= 2 {
            report.warnings.push(format!(
                "every save on {} disappeared (new or reformatted card?); restoring them from the cloud \
                 instead of deleting them everywhere",
                c.location
            ));
            for (u, lu) in gone {
                self.store.set_local_unit(&u, c.id, ABSENT, lu.base.as_deref())?;
            }
            return Ok(());
        }
        for (u, lu) in gone {
            match lu.base {
                Some(base) => {
                    let rec = self.new_tombstone(&u, base)?;
                    self.store.set_local_unit(&u, c.id, ABSENT, Some(&rec.id))?;
                    report.new_local_versions += 1;
                }
                // Never synced: nothing to delete elsewhere.
                None => self.store.delete_local_unit(&u, c.id)?,
            }
        }
        Ok(())
    }

    /// Records the local content of one unit: a new version if it changed since its base.
    fn snapshot_unit(
        &self,
        c: &ContainerRow,
        unit: &str,
        bytes: &[u8],
        meta: BTreeMap<String, String>,
        titles: Vec<String>,
        report: &mut SyncReport,
    ) -> Result<()> {
        let hash = content_id(bytes);
        let lu = self.store.local_unit(unit, c.id)?;
        if let Some(LocalUnit { content_hash, base: Some(_) }) = &lu {
            if *content_hash == hash {
                return Ok(());
            }
        }
        // Same bytes reached independently: adopt the tip as base, no new version.
        if let Some(t) = self.tips(unit)?.into_iter().find(|t| !t.rec.tombstone && t.rec.content == hash) {
            self.store.set_local_unit(unit, c.id, &hash, Some(&t.rec.id))?;
            return Ok(());
        }
        // First sync of a container holding an older version (a stale copy of the card): adopt
        // that version, so reconciliation fast-forwards it instead of raising a conflict.
        if lu.as_ref().and_then(|l| l.base.as_ref()).is_none() {
            let mut known = self.store.versions_of(unit)?;
            known.sort_by(|a, b| (b.rec.hlc, &b.rec.id).cmp(&(a.rec.hlc, &a.rec.id)));
            if let Some(v) = known.into_iter().find(|v| !v.rec.tombstone && v.rec.content == hash) {
                self.store.set_local_unit(unit, c.id, &hash, Some(&v.rec.id))?;
                return Ok(());
            }
        }
        let parents: Vec<String> = lu.and_then(|l| l.base).into_iter().collect();
        let rec = self.new_version(unit, parents, bytes, meta, titles)?;
        self.store.set_local_unit(unit, c.id, &hash, Some(&rec.id))?;
        report.new_local_versions += 1;
        Ok(())
    }

    /// Creates a version from local content: stages its chunks and queues the record.
    fn new_version(
        &self,
        unit: &str,
        parents: Vec<String>,
        content: &[u8],
        meta: BTreeMap<String, String>,
        titles: Vec<String>,
    ) -> Result<VersionRecord> {
        let mut chunks = Vec::new();
        for piece in chunk::split(content) {
            let id = content_id(piece);
            if !self.store.blob_known(&id)? {
                fsutil::write_once(&self.staging_path(&id), piece)?;
            }
            chunks.push(id);
        }
        let rec = VersionRecord {
            unit: unit.to_string(),
            id: String::new(),
            parents,
            content: content_id(content),
            chunks,
            size: content.len() as u64,
            meta,
            hlc: self.tick()?,
            playtime_ms: 0,
            titles,
            tombstone: false,
        }
        .seal();
        self.publish_version(&rec)?;
        Ok(rec)
    }

    /// Records the deletion of a unit as a version (DESIGN §4): its history stays restorable.
    fn new_tombstone(&self, unit: &str, base: String) -> Result<VersionRecord> {
        let titles = self.store.version(&base)?.map(|v| v.rec.titles).unwrap_or_default();
        let rec = VersionRecord {
            unit: unit.to_string(),
            id: String::new(),
            parents: vec![base],
            content: content_id(b""),
            chunks: vec![],
            size: 0,
            meta: BTreeMap::new(),
            hlc: self.tick()?,
            playtime_ms: 0,
            titles,
            tombstone: true,
        }
        .seal();
        self.publish_version(&rec)?;
        Ok(rec)
    }

    /// Records a version locally and queues it for upload (unless already known).
    fn publish_version(&self, rec: &VersionRecord) -> Result<()> {
        let me = self.device_id()?;
        self.store.tx(|| {
            if self.store.insert_version(rec, &me)? {
                self.store.outbox_push(&LogRecord::Version(rec.clone()).to_line())?;
            }
            Ok(())
        })
    }

    /// Versions of `unit` that no other version names as a parent, oldest first.
    fn tips(&self, unit: &str) -> Result<Vec<StoredVersion>> {
        let all = self.store.versions_of(unit)?;
        let parents: HashSet<String> = all.iter().flat_map(|v| v.rec.parents.iter().cloned()).collect();
        let mut tips: Vec<StoredVersion> = all.into_iter().filter(|v| !parents.contains(&v.rec.id)).collect();
        tips.sort_by(|a, b| (a.rec.hlc, &a.rec.id).cmp(&(b.rec.hlc, &b.rec.id)));
        Ok(tips)
    }

    /// Reconciliation table (DESIGN §6.3) for every unit of one container, then one apply for
    /// all the changes. Local changes were already turned into versions by [`Self::scan`], so
    /// divergence shows up as more than one tip.
    fn reconcile(&self, c: &ContainerRow, image_hash: &str, online: bool, report: &mut SyncReport) -> Result<()> {
        let mode = self.card_mode(&c.card_id)?;
        let mut pending = Vec::new();
        for unit in self.units_of_card(&c.card_id, mode, Some(c.id))? {
            let lu = self.store.local_unit(&unit, c.id)?;
            if mode == CardMode::WholeFile && lu.as_ref().and_then(|l| l.base.as_ref()).is_none() {
                continue;
            }
            let tips = self.tips(&unit)?;
            let Some(target) = self.decide(c, &unit, &tips, lu.as_ref(), report)? else { continue };
            self.store.close_conflicts(&unit)?;
            let local = lu.as_ref().map_or(ABSENT, |l| l.content_hash.as_str());
            let wanted = if target.rec.tombstone { ABSENT } else { target.rec.content.as_str() };
            if local == wanted {
                if lu.as_ref().and_then(|l| l.base.as_deref()) != Some(target.rec.id.as_str()) {
                    self.store.set_local_unit(&unit, c.id, wanted, Some(&target.rec.id))?;
                }
                self.store.inbox_remove(&unit, c.id)?;
                continue;
            }
            pending.push((unit, target));
        }
        if pending.is_empty() {
            return Ok(());
        }
        self.apply(c, mode, image_hash, pending, online, report)
    }

    /// The version a unit should hold, or `None` while an `ask` conflict is open.
    fn decide(
        &self,
        c: &ContainerRow,
        unit: &str,
        tips: &[StoredVersion],
        lu: Option<&LocalUnit>,
        report: &mut SyncReport,
    ) -> Result<Option<StoredVersion>> {
        Ok(Some(match tips.len() {
            0 => return Ok(None),
            // Nothing to do, or a fast-forward: the single tip descends from every version.
            1 => tips[0].clone(),
            _ => {
                let contents: HashSet<(&str, bool)> =
                    tips.iter().map(|t| (t.rec.content.as_str(), t.rec.tombstone)).collect();
                if contents.len() == 1 {
                    // Divergent histories with identical bytes: collapse them, no decision needed.
                    self.auto_merge(unit, tips, tips.last().unwrap())?
                } else {
                    match self.opts.policy {
                        ConflictPolicy::Ask => {
                            let ids: Vec<String> = tips.iter().map(|t| t.rec.id.clone()).collect();
                            let local = lu.and_then(|l| l.base.clone()).unwrap_or_default();
                            let id = self.store.upsert_conflict(unit, c.id, &local, &ids)?;
                            if !report.conflicts.contains(&id) {
                                report.conflicts.push(id);
                            }
                            return Ok(None);
                        }
                        ConflictPolicy::Newest => {
                            report.resolved.push(unit.to_string());
                            self.auto_merge(unit, tips, newest(tips))?
                        }
                    }
                }
            }
        }))
    }

    /// Deterministic merge of `tips` choosing `winner`'s content. Every device computing the
    /// same merge produces the same record (same id), so concurrent automatic resolutions
    /// converge instead of creating a new conflict.
    fn auto_merge(&self, unit: &str, tips: &[StoredVersion], winner: &StoredVersion) -> Result<StoredVersion> {
        let mut parents: Vec<String> = tips.iter().map(|t| t.rec.id.clone()).collect();
        parents.sort();
        let hlc = tips.iter().map(|t| t.rec.hlc).max().unwrap().successor();
        let w = &winner.rec;
        let rec = VersionRecord {
            unit: unit.to_string(),
            id: String::new(),
            parents,
            content: w.content.clone(),
            chunks: w.chunks.clone(),
            size: w.size,
            meta: w.meta.clone(),
            hlc,
            playtime_ms: w.playtime_ms,
            titles: w.titles.clone(),
            tombstone: w.tombstone,
        }
        .seal();
        self.observe(hlc)?;
        self.publish_version(&rec)?;
        Ok(StoredVersion { rec, origin: self.device_id()? })
    }

    fn block(
        &self,
        c: &ContainerRow,
        unit: &str,
        target: &str,
        kind: BlockKind,
        reason: String,
        report: &mut SyncReport,
    ) -> Result<()> {
        self.store.inbox_put(unit, c.id, target, &reason)?;
        report.blocked.push(Blocked { unit: unit.to_string(), container: c.id, kind, reason });
        Ok(())
    }

    /// Writes the `pending` target versions into the container in one rebuild (DESIGN §5
    /// apply steps). Units that can't be applied (content unavailable, card full, malformed)
    /// wait in the inbox; the rest are still applied.
    fn apply(
        &self,
        c: &ContainerRow,
        mode: CardMode,
        image_hash: &str,
        mut pending: Vec<(String, StoredVersion)>,
        online: bool,
        report: &mut SyncReport,
    ) -> Result<()> {
        if c.flags & CF_READ_ONLY != 0 || self.busy.contains(c.id) {
            let (kind, reason) = if c.flags & CF_READ_ONLY != 0 {
                (BlockKind::Io, "container is read-only")
            } else {
                (BlockKind::Busy, "container is in use")
            };
            for (unit, t) in &pending {
                self.block(c, unit, &t.rec.id, kind, reason.into(), report)?;
            }
            return Ok(());
        }
        let path = Path::new(&c.location);
        let fmt = self.format_of(c)?;
        // 1. Read the current container. It must still be what we scanned (and validated).
        let current = match read_image(path, fmt, false) {
            Ok(b) => b,
            Err(e) if fsutil::is_locked(&e) => {
                for (unit, t) in &pending {
                    self.block(c, unit, &t.rec.id, BlockKind::Busy, LOCKED.into(), report)?;
                }
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        if content_id(&current) != image_hash {
            for (unit, _) in &pending {
                report.blocked.push(Blocked {
                    unit: unit.clone(),
                    container: c.id,
                    kind: BlockKind::Changed,
                    reason: "container changed during sync; it will be snapshotted next cycle".into(),
                });
            }
            return Ok(());
        }
        // Deletions first, so they free space for what comes next.
        pending.sort_by_key(|(_, t)| !t.rec.tombstone);
        let mut ready = Vec::new();
        for (unit, t) in pending {
            if t.rec.tombstone {
                ready.push((unit, t, None));
                continue;
            }
            match self.assemble(&t.rec, online) {
                Ok(b) => ready.push((unit, t, Some(b))),
                Err(e) => {
                    let reason = format!("content not available: {e}");
                    self.block(c, &unit, &t.rec.id, BlockKind::Unavailable, reason, report)?
                }
            }
        }
        if ready.is_empty() {
            return Ok(());
        }
        // 2. Build the new image.
        let (image, applied) = match mode {
            CardMode::WholeFile => {
                let (unit, t, content) = ready.pop().expect("whole-file mode has one unit");
                let content = content.unwrap_or_default();
                if let Err(e) = fmt.validate(&content) {
                    let reason = format!("the synced card image is not valid: {e}");
                    return self.block(c, &unit, &t.rec.id, BlockKind::Invalid, reason, report);
                }
                (content, vec![(unit, t)])
            }
            CardMode::Units => self.build_units(c, fmt, &current, ready, report)?,
        };
        if applied.is_empty() {
            return Ok(());
        }
        // 3. Re-parse and verify every unit, touched or not.
        self.verify(c, fmt, mode, &image, &applied)?;
        // 4. Keep what we are about to overwrite.
        match mode {
            CardMode::WholeFile => {
                let (unit, _) = &applied[0];
                let base = self.store.local_unit(unit, c.id)?.and_then(|l| l.base);
                self.snapshot(unit, c.id, &current, base.as_deref())?;
            }
            CardMode::Units => {
                let before: HashMap<String, Vec<u8>> =
                    fmt.extract_all(&current)?.into_iter().map(|u| (unit_id(&c.card_id, &u.name), u.bytes)).collect();
                for (unit, _) in &applied {
                    if let Some(bytes) = before.get(unit) {
                        let base = self.store.local_unit(unit, c.id)?.and_then(|l| l.base);
                        self.snapshot(unit, c.id, bytes, base.as_deref())?;
                    }
                }
            }
        }
        // 5. Temp file (or directory), fsync, atomic replace.
        // A host thread may have marked the container busy since the check above (a session
        // that stopped waiting for this cycle). The gate is held across the write, so once
        // `BusyGate::set` returns nothing writes to the container any more.
        let failure = match self.busy.write_unless_busy(c.id, || write_image(path, fmt, &image)) {
            None => Some((BlockKind::Busy, "container is in use".to_string())),
            Some(Err(e)) => Some((BlockKind::Io, format!("cannot replace {}: {e}", c.location))),
            Some(Ok(())) => None,
        };
        if let Some((kind, reason)) = failure {
            for (unit, t) in &applied {
                self.block(c, unit, &t.rec.id, kind, reason.clone(), report)?;
            }
            return Ok(());
        }
        self.store.tx(|| {
            for (unit, t) in &applied {
                let wanted = if t.rec.tombstone { ABSENT } else { t.rec.content.as_str() };
                self.store.set_local_unit(unit, c.id, wanted, Some(&t.rec.id))?;
                self.store.inbox_remove(unit, c.id)?;
            }
            Ok(())
        })?;
        for (unit, t) in applied {
            report.applied.push(Applied { unit, version: t.rec.id, container: c.id, location: c.location.clone() });
        }
        Ok(())
    }

    /// Unit mode: applies every change in one rebuild, or, if that fails (card full, or a
    /// malformed save from the remote), one change at a time, holding back the ones that fail.
    #[allow(clippy::type_complexity)]
    fn build_units(
        &self,
        c: &ContainerRow,
        fmt: &dyn ContainerFormat,
        current: &[u8],
        ready: Vec<(String, StoredVersion, Option<Vec<u8>>)>,
        report: &mut SyncReport,
    ) -> Result<(Vec<u8>, Vec<(String, StoredVersion)>)> {
        let ops: Vec<ApplyOp> = ready
            .iter()
            .map(|(unit, t, content)| {
                let name = split_unit(unit).map(|(_, n)| n).unwrap_or_default().to_string();
                match content {
                    None => ApplyOp::Delete(name),
                    Some(bytes) => ApplyOp::Put(Canonical { name, bytes: bytes.clone(), meta: t.rec.meta.clone() }),
                }
            })
            .collect();
        if let Ok(image) = fmt.apply(current, &ops) {
            return Ok((image, ready.into_iter().map(|(u, t, _)| (u, t)).collect()));
        }
        let mut image = current.to_vec();
        let mut applied = Vec::new();
        for ((unit, t, _), op) in ready.into_iter().zip(ops) {
            match fmt.apply(&image, std::slice::from_ref(&op)) {
                Ok(next) => {
                    image = next;
                    applied.push((unit, t));
                }
                Err(FormatError::NoSpace) => {
                    let reason = "not enough free space on the card".into();
                    self.block(c, &unit, &t.rec.id, BlockKind::NoSpace, reason, report)?
                }
                Err(e) => {
                    let reason = format!("cannot write this save: {e}");
                    self.block(c, &unit, &t.rec.id, BlockKind::Invalid, reason, report)?
                }
            }
        }
        Ok((image, applied))
    }

    /// Checks a rebuilt image before it replaces the original: it validates, and holds exactly
    /// the expected content for every unit, both those just applied and those left alone.
    fn verify(
        &self,
        c: &ContainerRow,
        fmt: &dyn ContainerFormat,
        mode: CardMode,
        image: &[u8],
        applied: &[(String, StoredVersion)],
    ) -> Result<()> {
        fmt.validate(image).map_err(|e| Error::Integrity(format!("rebuilt {} does not validate: {e}", c.location)))?;
        let ok = match mode {
            CardMode::WholeFile => content_id(image) == applied[0].1.rec.content,
            CardMode::Units => {
                let prefix = format!("{}:", c.card_id);
                let card_unit = whole_file_unit(&c.card_id);
                let mut expected: BTreeMap<String, String> = self
                    .store
                    .local_units_of(c.id)?
                    .into_iter()
                    .filter(|(u, lu)| u.starts_with(&prefix) && *u != card_unit && lu.content_hash != ABSENT)
                    .map(|(u, lu)| (u, lu.content_hash))
                    .collect();
                for (unit, t) in applied {
                    if t.rec.tombstone {
                        expected.remove(unit);
                    } else {
                        expected.insert(unit.clone(), t.rec.content.clone());
                    }
                }
                let got: BTreeMap<String, String> = fmt
                    .extract_all(image)?
                    .into_iter()
                    .map(|u| (unit_id(&c.card_id, &u.name), content_id(&u.bytes)))
                    .collect();
                got == expected
            }
        };
        if !ok {
            return Err(Error::Integrity(format!("rebuilt {} does not hold exactly the expected content", c.location)));
        }
        Ok(())
    }

    /// Rebuilds a version's content from the snapshot store, staged chunks, or the remote,
    /// verifying every hash.
    fn assemble(&self, v: &VersionRecord, online: bool) -> Result<Vec<u8>> {
        if let Ok(b) = fs::read(self.snapshot_path(&v.content)) {
            if content_id(&b) == v.content {
                return Ok(b);
            }
        }
        let mut out = Vec::with_capacity(v.size as usize);
        for id in &v.chunks {
            let data = match fs::read(self.staging_path(id)) {
                Ok(b) if content_id(&b) == *id => b,
                _ => {
                    let remote = self.remote.as_ref().ok_or(Error::NoRemote)?;
                    if !online {
                        return Err(RemoteError::Network("offline".into()).into());
                    }
                    let cipher = self.cipher()?;
                    let key = match cipher {
                        Some(c) => c.blob_key(id),
                        None => chunk::blob_key(id),
                    };
                    let mut blob = remote
                        .get(&key)?
                        .ok_or_else(|| Error::Integrity(format!("blob {id} is missing from the remote")))?;
                    if let Some(c) = cipher {
                        blob = c.open(&key, &blob)?;
                    }
                    chunk::decode_blob(&blob, id)?
                }
            };
            out.extend_from_slice(&data);
        }
        if content_id(&out) != v.content {
            return Err(Error::Integrity(format!("version {} reassembles to different content", v.id)));
        }
        Ok(out)
    }

    /// A version's record, if it is known locally.
    pub fn version(&self, id: &str) -> Result<Option<VersionRecord>> {
        Ok(self.store.version(id)?.map(|v| v.rec))
    }

    /// Reads a version's content (from local stores or the remote).
    pub fn read_version(&self, id: &str) -> Result<Vec<u8>> {
        let v = self.store.version(id)?.ok_or_else(|| Error::NotFound(id.to_string()))?;
        self.assemble(&v.rec, self.remote.is_some())
    }

    // ------------------------------------------------------------------------------------------
    // Push and pull
    // ------------------------------------------------------------------------------------------

    /// Push (DESIGN §6.1 step 1): blobs first, then one new log segment with every queued
    /// record. The segment body is fixed before the first attempt, so a retry after a crash
    /// either finds its own earlier write or writes the same bytes.
    fn push(&self, remote: &dyn Remote, report: &mut SyncReport) -> Result<()> {
        let cipher = self.cipher()?;
        let seg: PendingSegment = match self.store.meta("pending_segment")? {
            Some(s) => serde_json::from_str(&s)?,
            None => {
                let rows = self.store.outbox()?;
                let Some(last) = rows.last() else { return Ok(()) };
                let body: String = rows.iter().map(|(_, l)| format!("{l}\n")).collect();
                let seg = PendingSegment { body, last_outbox_id: last.0 };
                self.store.set_meta("pending_segment", &serde_json::to_string(&seg)?)?;
                seg
            }
        };
        let mut units = Vec::new();
        for line in seg.body.lines() {
            if let Ok(Some(LogRecord::Version(v))) = parse_record(line.as_bytes()) {
                units.push((v.unit.clone(), v.id.clone()));
                for id in &v.chunks {
                    self.upload_blob(remote, id, report)?;
                }
            }
        }
        let seq = loop {
            let dev = self.device_id()?;
            let seq = self.next_seq()?;
            self.ensure_device_json(remote, &dev)?;
            let key = segment_key(&dev, seq);
            // Encrypted for the key it is written under; a fresh nonce on every attempt.
            let body = match cipher {
                Some(c) => c.seal(&key, seg.body.as_bytes()),
                None => seg.body.as_bytes().to_vec(),
            };
            match create_only(remote, &key, &body, self.conditional_writes)? {
                PutOutcome::Created => break seq,
                PutOutcome::AlreadyExists => {
                    let existing = remote.get(&key)?;
                    let existing = match (cipher, existing) {
                        (Some(c), Some(b)) => c.open(&key, &b).ok(),
                        (_, b) => b,
                    };
                    if existing.as_deref() == Some(seg.body.as_bytes()) {
                        break seq; // our own write from an interrupted attempt
                    }
                    // Another installation is using this device id (DESIGN §6.2): a restored
                    // app backup or a copied data dir. Mint a new id and write under that.
                    let new = self.remint_device(&dev, seq)?;
                    report.reminted = Some((dev, new));
                }
            }
        };
        self.store.tx(|| {
            self.store.outbox_delete_upto(seg.last_outbox_id)?;
            self.store.set_meta("next_seq", &(seq + 1).to_string())?;
            self.store.delete_meta("pending_segment")
        })?;
        report.uploaded_versions += units.len();
        report.uploaded.extend(units);
        self.clean_staging()?;
        Ok(())
    }

    fn upload_blob(&self, remote: &dyn Remote, id: &str, report: &mut SyncReport) -> Result<()> {
        if self.store.blob_known(id)? {
            return Ok(());
        }
        let data = fs::read(self.staging_path(id))
            .map_err(|e| Error::Integrity(format!("staged chunk {id} is unreadable: {e}")))?;
        if content_id(&data) != id {
            return Err(Error::Integrity(format!("staged chunk {id} is corrupt")));
        }
        // Blobs are content-addressed, so "already exists" counts as success.
        let (key, body) = match self.cipher()? {
            Some(c) => {
                let key = c.blob_key(id);
                let body = c.seal(&key, &chunk::encode_blob(&data));
                (key, body)
            }
            None => (chunk::blob_key(id), chunk::encode_blob(&data)),
        };
        create_only(remote, &key, &body, self.conditional_writes)?;
        self.store.mark_blob_known(id)?;
        report.uploaded_blobs += 1;
        Ok(())
    }

    fn ensure_device_json(&self, remote: &dyn Remote, dev: &str) -> Result<()> {
        let flag = format!("device_json:{dev}");
        if self.store.meta(&flag)?.is_some() {
            return Ok(());
        }
        let body = serde_json::json!({
            "name": self.device_name()?,
            "emulator": self.opts.emulator_id,
            "os": std::env::consts::OS,
            "created": Hlc { ms: Hlc::wall_ms(), counter: 0 }.to_string(),
        });
        let key = format!("devices/{dev}/device.json");
        let body = match self.cipher()? {
            Some(c) => c.seal(&key, body.to_string().as_bytes()),
            None => body.to_string().into_bytes(),
        };
        create_only(remote, &key, &body, self.conditional_writes)?;
        self.store.set_meta(&flag, "1")
    }

    fn remint_device(&self, old: &str, seq: u64) -> Result<String> {
        let new = uuid::Uuid::now_v7().to_string();
        log::warn!("device id {old} is used by another installation; continuing as {new}");
        let name = self.device_name()?;
        self.store.tx(|| {
            self.store.set_meta("device_id", &new)?;
            self.store.set_meta("next_seq", "1")?;
            self.store.set_device_name(&new, &name)?;
            // Everything up to our last segment under the old id is already in our DB; pull
            // the rest (the other installation's records) from there on.
            if seq > 1 {
                self.store.set_cursor(old, &segment_key(old, seq - 1))?;
            }
            Ok(())
        })?;
        Ok(new)
    }

    fn next_seq(&self) -> Result<u64> {
        Ok(self.store.meta("next_seq")?.and_then(|s| s.parse().ok()).unwrap_or(1))
    }

    /// Pull (DESIGN §6.1 step 2): for every other device, fetch log segments past the cursor
    /// and merge their records. A segment that is listed but can't be read yet stops that
    /// device's pull until the next cycle; absence is never read as deletion.
    fn pull(&self, remote: &dyn Remote, report: &mut SyncReport) -> Result<()> {
        let cipher = self.cipher()?;
        let me = self.device_id()?;
        for dev in remote.list_dirs("devices/")? {
            // Device ids end up in keys and the DB: accept only UUID-shaped names.
            if dev == me || uuid::Uuid::parse_str(&dev).is_err() {
                continue;
            }
            if self.store.device_name(&dev)?.is_none() {
                let key = format!("devices/{dev}/device.json");
                let body = match (remote.get(&key)?, cipher) {
                    (Some(b), Some(c)) => c.open(&key, &b).ok(),
                    (b, _) => b,
                };
                if let Some(body) = body {
                    let name = serde_json::from_slice::<serde_json::Value>(&body)
                        .ok()
                        .and_then(|v| v.get("name").and_then(|n| n.as_str()).map(str::to_string));
                    if let Some(name) = name {
                        self.store.set_device_name(&dev, &name)?;
                    }
                }
            }
            let cursor = self.store.cursor(&dev)?;
            // Segments are numbered contiguously from 1. With delayed or out-of-order listing a
            // later segment can show up before an earlier one; stop at the gap rather than move
            // the cursor past a segment we haven't read.
            let mut expected = match &cursor {
                Some(c) => segment_seq(c).map_or(1, |s| s + 1),
                None => 1,
            };
            for key in remote.list(&format!("devices/{dev}/log/"), cursor.as_deref())? {
                let Some(seq) = segment_seq(&key) else { continue };
                if seq != expected {
                    log::info!("{key}: waiting for segment {expected:016x} of {dev} to become visible");
                    break;
                }
                let Some(mut body) = remote.get(&key)? else { break };
                expected += 1;
                if let Some(c) = cipher {
                    // Not written with this repository's key (or altered): nothing in it can be
                    // trusted, so it is skipped like a malformed record.
                    match c.open(&key, &body) {
                        Ok(plain) => body = plain,
                        Err(e) => {
                            log::warn!("skipping segment: {e}");
                            report.warnings.push(format!("skipped segment: {e}"));
                            self.store.set_cursor(&dev, &key)?;
                            continue;
                        }
                    }
                }
                self.store.tx(|| {
                    for line in body.split(|b| *b == b'\n').filter(|l| !l.iter().all(u8::is_ascii_whitespace)) {
                        match parse_record(line) {
                            Ok(Some(rec)) => self.ingest(&dev, rec, report)?,
                            Ok(None) => {}
                            Err(e) => {
                                log::warn!("{key}: skipping record: {e}");
                                report.warnings.push(format!("{key}: skipped record: {e}"));
                            }
                        }
                    }
                    self.store.set_cursor(&dev, &key)
                })?;
            }
        }
        Ok(())
    }

    fn ingest(&self, origin: &str, rec: LogRecord, report: &mut SyncReport) -> Result<()> {
        match rec {
            LogRecord::Card(cr) => {
                self.observe(cr.hlc)?;
                match self.store.card(&cr.card)? {
                    Some((_, hlc)) if hlc >= cr.hlc => {}
                    existing => {
                        self.store.set_card(&cr.card, cr.mode, cr.hlc)?;
                        if matches!(existing, Some((m, _)) if m != cr.mode) {
                            report.mode_changes.push(cr.card.clone());
                        }
                    }
                }
            }
            LogRecord::Version(v) => {
                self.observe(v.hlc)?;
                for id in &v.chunks {
                    self.store.mark_blob_known(id)?;
                }
                if self.store.insert_version(&v, origin)? {
                    report.pulled_records += 1;
                }
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------------------------------
    // Clock
    // ------------------------------------------------------------------------------------------

    fn last_hlc(&self) -> Result<Hlc> {
        Ok(self.store.meta("hlc")?.and_then(|s| Hlc::parse(&s)).unwrap_or_default())
    }

    fn tick(&self) -> Result<Hlc> {
        let next = Hlc::tick(self.last_hlc()?, Hlc::wall_ms());
        self.store.set_meta("hlc", &next.to_string())?;
        Ok(next)
    }

    fn observe(&self, seen: Hlc) -> Result<()> {
        if seen > self.last_hlc()? {
            self.store.set_meta("hlc", &seen.to_string())?;
        }
        Ok(())
    }

    // ------------------------------------------------------------------------------------------
    // Local stores
    // ------------------------------------------------------------------------------------------

    fn staging_path(&self, id: &str) -> PathBuf {
        self.dir.join("staging").join(hex_of(id))
    }

    fn snapshot_path(&self, content: &str) -> PathBuf {
        self.dir.join("snapshots").join(hex_of(content))
    }

    fn clean_staging(&self) -> Result<()> {
        for entry in fs::read_dir(self.dir.join("staging"))? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if self.store.blob_known(&format!("b3:{name}"))? {
                let _ = fs::remove_file(entry.path());
            }
        }
        Ok(())
    }

    /// Saves content that is about to be overwritten in the local history ring.
    fn snapshot(&self, unit: &str, container: i64, bytes: &[u8], version: Option<&str>) -> Result<()> {
        let hash = content_id(bytes);
        fsutil::write_once(&self.snapshot_path(&hash), bytes)?;
        self.store.add_snapshot(unit, container, &hash, version, bytes.len() as u64)
    }

    /// Keeps the snapshot store under budget, oldest first, never evicting a unit's most recent
    /// snapshot.
    fn evict_snapshots(&self) -> Result<()> {
        let rows = self.store.snapshots()?;
        let mut sizes: BTreeMap<&str, u64> = BTreeMap::new();
        for r in &rows {
            sizes.insert(&r.content_hash, r.size);
        }
        let mut total: u64 = sizes.values().sum();
        if total <= self.opts.snapshot_budget_bytes {
            return Ok(());
        }
        let latest: HashSet<i64> = {
            let mut m: BTreeMap<&str, i64> = BTreeMap::new();
            for r in &rows {
                m.insert(&r.unit_id, r.id);
            }
            m.into_values().collect()
        };
        let mut remaining: Vec<&crate::store::SnapshotRow> = rows.iter().collect();
        for r in &rows {
            if total <= self.opts.snapshot_budget_bytes {
                break;
            }
            if latest.contains(&r.id) {
                continue;
            }
            self.store.delete_snapshot(r.id)?;
            remaining.retain(|x| x.id != r.id);
            if !remaining.iter().any(|x| x.content_hash == r.content_hash) {
                let _ = fs::remove_file(self.snapshot_path(&r.content_hash));
                total -= r.size;
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------------------------------
    // History, restore, conflicts
    // ------------------------------------------------------------------------------------------

    /// Accepts a card id (`ps2:Mcd001`) or card name (`Mcd001`) and returns the card id.
    pub fn resolve_card(&self, arg: &str) -> Result<String> {
        let cards = self.store.cards()?;
        if cards.iter().any(|(id, _)| id == arg) {
            return Ok(arg.to_string());
        }
        let matches: Vec<String> =
            cards.into_iter().map(|(id, _)| id).filter(|id| id.split_once(':').map(|(_, n)| n) == Some(arg)).collect();
        match matches.as_slice() {
            [one] => Ok(one.clone()),
            [] => Err(Error::NotFound(format!("no card named {arg:?}"))),
            many => Err(Error::InvalidArg(format!("{arg:?} is ambiguous: {}", many.join(", ")))),
        }
    }

    /// Accepts a unit id (`ps2:Mcd001:BASLUS-20062GTA3`, `ps2:Mcd001:@card`), the name of a
    /// save (`BASLUS-20062GTA3`), or, for a card in whole-file mode, the card (`Mcd001`,
    /// `ps2:Mcd001`). Returns the unit id.
    pub fn resolve_unit(&self, arg: &str) -> Result<String> {
        if split_unit(arg).is_some() {
            return Ok(arg.to_string());
        }
        if let Ok(card_id) = self.resolve_card(arg) {
            return match self.card_mode(&card_id)? {
                CardMode::WholeFile => Ok(whole_file_unit(&card_id)),
                CardMode::Units => {
                    let saves = self.units_of_card(&card_id, CardMode::Units, None)?;
                    let names: Vec<&str> = saves.iter().filter_map(|u| split_unit(u).map(|(_, n)| n)).take(5).collect();
                    Err(Error::InvalidArg(format!(
                        "card {card_id} syncs each save separately; name a save{}",
                        if names.is_empty() { String::new() } else { format!(", e.g. {}", names.join(", ")) }
                    )))
                }
            };
        }
        let mut matches = Vec::new();
        for (card_id, mode) in self.store.cards()? {
            if mode == CardMode::Units {
                let unit = unit_id(&card_id, arg);
                if !self.store.versions_of(&unit)?.is_empty() {
                    matches.push(unit);
                }
            }
        }
        match matches.as_slice() {
            [one] => Ok(one.clone()),
            [] => Err(Error::NotFound(format!("no card or save named {arg:?}"))),
            many => Err(Error::InvalidArg(format!("{arg:?} is on several cards: {}", many.join(", ")))),
        }
    }

    fn entry(
        &self,
        v: VersionRecord,
        tips: &HashSet<String>,
        local: &HashSet<String>,
        origin: &str,
    ) -> Result<HistoryEntry> {
        let device = self.store.device_name(origin)?.unwrap_or_else(|| origin.to_string());
        Ok(HistoryEntry { is_tip: tips.contains(&v.id), is_local: local.contains(&v.id), version: v, device })
    }

    /// Containers holding the card a unit belongs to.
    fn containers_of_unit(&self, unit: &str) -> Result<Vec<ContainerRow>> {
        let card = split_unit(unit).map(|(c, _)| c).unwrap_or_default();
        Ok(self.store.containers()?.into_iter().filter(|c| c.card_id == card).collect())
    }

    fn local_bases(&self, unit: &str) -> Result<HashSet<String>> {
        let mut out = HashSet::new();
        for c in self.containers_of_unit(unit)? {
            if let Some(LocalUnit { base: Some(b), .. }) = self.store.local_unit(unit, c.id)? {
                out.insert(b);
            }
        }
        Ok(out)
    }

    /// Every known version of a unit, newest first.
    pub fn history(&self, arg: &str) -> Result<Vec<HistoryEntry>> {
        let unit = self.resolve_unit(arg)?;
        let tips: HashSet<String> = self.tips(&unit)?.into_iter().map(|t| t.rec.id).collect();
        let local = self.local_bases(&unit)?;
        let mut all = self.store.versions_of(&unit)?;
        all.sort_by(|a, b| (b.rec.hlc, &b.rec.id).cmp(&(a.rec.hlc, &a.rec.id)));
        all.into_iter().map(|v| self.entry(v.rec, &tips, &local, &v.origin)).collect()
    }

    fn find_version(&self, unit: &str, prefix: &str) -> Result<VersionRecord> {
        let p = hex_of(prefix);
        if p.is_empty() {
            return Err(Error::InvalidArg("empty version id".into()));
        }
        let found: Vec<VersionRecord> =
            self.store.versions_of(unit)?.into_iter().map(|v| v.rec).filter(|v| hex_of(&v.id).starts_with(p)).collect();
        match found.len() {
            1 => Ok(found.into_iter().next().unwrap()),
            0 => Err(Error::NotFound(format!("no version {prefix} of {unit}"))),
            _ => Err(Error::InvalidArg(format!("version prefix {prefix} is ambiguous"))),
        }
    }

    fn scan_unit(&self, unit: &str) -> Result<()> {
        let mut report = SyncReport::default();
        for c in self.containers_of_unit(unit)? {
            self.scan(&c, &mut report)?;
        }
        Ok(())
    }

    /// Creates a new version whose content equals `version` (DESIGN `es_unit_restore`). Its
    /// parents are all current tips, so it also closes an open conflict. Unsaved local changes
    /// are captured as a version first. Run [`Engine::sync`] to apply and upload it.
    pub fn restore(&mut self, arg: &str, version: &str) -> Result<VersionRecord> {
        let unit = self.resolve_unit(arg)?;
        self.scan_unit(&unit)?;
        let v = self.find_version(&unit, version)?;
        let mut parents: Vec<String> = self.tips(&unit)?.into_iter().map(|t| t.rec.id).collect();
        parents.sort();
        let mut meta = v.meta.clone();
        meta.insert("restored_from".into(), v.id.clone());
        let rec = VersionRecord {
            unit: unit.clone(),
            id: String::new(),
            parents,
            content: v.content.clone(),
            chunks: v.chunks.clone(),
            size: v.size,
            meta,
            hlc: self.tick()?,
            playtime_ms: v.playtime_ms,
            titles: v.titles.clone(),
            tombstone: v.tombstone,
        }
        .seal();
        self.publish_version(&rec)?;
        self.store.close_conflicts(&unit)?;
        Ok(rec)
    }

    pub fn conflicts(&self) -> Result<Vec<ConflictInfo>> {
        let containers = self.store.containers()?;
        let mut out = Vec::new();
        for row in self.store.open_conflicts()? {
            let tips: HashSet<String> = row.tips.iter().cloned().collect();
            let local: HashSet<String> = [row.local_version.clone()].into();
            let mut entries = Vec::new();
            for id in &row.tips {
                if let Some(v) = self.store.version(id)? {
                    entries.push(self.entry(v.rec, &tips, &local, &v.origin)?);
                }
            }
            let local_entry = match self.store.version(&row.local_version)? {
                Some(v) => Some(self.entry(v.rec, &tips, &local, &v.origin)?),
                None => None,
            };
            out.push(ConflictInfo {
                id: row.id,
                unit: row.unit_id.clone(),
                location: containers.iter().find(|c| c.id == row.container_id).map(|c| c.location.clone()),
                local: local_entry,
                tips: entries,
            });
        }
        Ok(out)
    }

    /// Resolves an open conflict with a merge version whose parents are all current tips
    /// (DESIGN §6.3). Run [`Engine::sync`] to apply and upload it.
    pub fn resolve(&mut self, conflict: i64, resolution: Resolution) -> Result<VersionRecord> {
        let row = self
            .store
            .open_conflicts()?
            .into_iter()
            .find(|c| c.id == conflict)
            .ok_or_else(|| Error::NotFound(format!("no open conflict #{conflict}")))?;
        let unit = row.unit_id.clone();
        self.scan_unit(&unit)?;
        let tips = self.tips(&unit)?;
        let chosen = match &resolution {
            Resolution::KeepLocal => {
                let base = self
                    .store
                    .local_unit(&unit, row.container_id)?
                    .and_then(|l| l.base)
                    .ok_or_else(|| Error::NotFound("no local version".into()))?;
                self.store.version(&base)?.ok_or_else(|| Error::NotFound(base.clone()))?.rec
            }
            Resolution::Take(prefix) => {
                let p = hex_of(prefix);
                let found: Vec<&StoredVersion> = tips.iter().filter(|t| hex_of(&t.rec.id).starts_with(p)).collect();
                match found.as_slice() {
                    [one] => one.rec.clone(),
                    [] => return Err(Error::NotFound(format!("{prefix} is not one of the conflicting versions"))),
                    _ => return Err(Error::InvalidArg(format!("version prefix {prefix} is ambiguous"))),
                }
            }
        };
        let mut parents: Vec<String> = tips.iter().map(|t| t.rec.id.clone()).collect();
        parents.sort();
        let rec = VersionRecord {
            unit: unit.clone(),
            id: String::new(),
            parents,
            content: chosen.content.clone(),
            chunks: chosen.chunks.clone(),
            size: chosen.size,
            meta: chosen.meta.clone(),
            hlc: self.tick()?,
            playtime_ms: chosen.playtime_ms,
            titles: chosen.titles.clone(),
            tombstone: chosen.tombstone,
        }
        .seal();
        self.publish_version(&rec)?;
        self.store.close_conflicts(&unit)?;
        Ok(rec)
    }

    /// Ids of every version in the local DB (for convergence checks).
    pub fn all_version_ids(&self) -> Result<Vec<String>> {
        self.store.all_version_ids()
    }

    /// Records waiting in the outbox.
    pub fn pending_uploads(&self) -> Result<usize> {
        Ok(self.store.outbox()?.len())
    }

    /// Remote versions that couldn't be applied yet: `(unit, container, version, reason)`.
    pub fn inbox(&self) -> Result<Vec<(String, i64, String, String)>> {
        self.store.inbox()
    }
}

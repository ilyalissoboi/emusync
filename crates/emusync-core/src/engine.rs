//! The sync engine (DESIGN §6): local snapshots, push, pull, reconciliation and apply.
//!
//! M0 syncs logical cards in whole-file mode: each card is one unit (`ps2:Mcd001:@card`)
//! whose content is the card image, synced through the `raw` adapter.

use crate::chunk;
use crate::error::{Error, Result};
use crate::fsutil;
use crate::hlc::Hlc;
use crate::model::{content_id, parse_record, whole_file_unit, CardMode, CardRecord, LogRecord, VersionRecord};
use crate::store::{ContainerRow, LocalUnit, Store, StoredVersion, CF_READ_ONLY};
use emusync_formats::{self as formats, ApplyOp, Canonical, ContainerFormat, UnitRef, WHOLE_FILE_UNIT};
use emusync_remote::{create_only, probe_conditional_writes, PutOutcome, Remote, RemoteError};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            device_name: None,
            emulator_id: "emusync".into(),
            policy: ConflictPolicy::Ask,
            snapshot_budget_bytes: 100 * 1024 * 1024,
        }
    }
}

/// A container to register (mirrors `es_container_desc`).
#[derive(Debug, Clone, Default)]
pub struct ContainerDesc {
    pub location: PathBuf,
    /// Adapter id; `None` = `raw`.
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
    pub location: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocked {
    pub unit: String,
    pub reason: String,
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
    /// Cards whose mode another device changed.
    pub mode_changes: Vec<String>,
    /// `(old, new)` when this installation found its device id in use elsewhere and minted a new one.
    pub reminted: Option<(String, String)>,
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
    pub local_version: Option<String>,
    pub tips: Vec<String>,
    pub conflict: Option<i64>,
    pub inbox: Option<String>,
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
    busy: HashSet<i64>,
}

fn segment_key(device: &str, seq: u64) -> String {
    format!("devices/{device}/log/{seq:016x}.ndjson")
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
        Ok(Self { dir, store, remote: None, conditional_writes, opts, busy: HashSet::new() })
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
    pub fn add_remote(&mut self, remote: Arc<dyn Remote>) -> Result<bool> {
        remote.list_dirs("devices/")?;
        let cond = probe_conditional_writes(&*remote)?;
        match remote.get("repo.json")? {
            Some(body) => {
                let v: serde_json::Value = serde_json::from_slice(&body)?;
                if v.get("format").and_then(|f| f.as_u64()) != Some(1) {
                    return Err(Error::InvalidArg(format!("unsupported repository format in repo.json: {v}")));
                }
            }
            None => {
                let repo = serde_json::json!({
                    "format": 1,
                    "repo_id": uuid::Uuid::now_v7().to_string(),
                    "encryption": null,
                });
                create_only(&*remote, "repo.json", repo.to_string().as_bytes(), cond)?;
            }
        }
        self.store.set_meta("remote", &remote.describe())?;
        self.store.set_meta("conditional_writes", if cond { "1" } else { "0" })?;
        self.conditional_writes = cond;
        self.remote = Some(remote);
        Ok(cond)
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

    /// Registers a container for a logical card. Idempotent per location. Publishes the card
    /// (in whole-file mode, the only mode M0 adapters support) unless the repository already
    /// knows it.
    pub fn add_container(&mut self, desc: ContainerDesc) -> Result<i64> {
        let location = std::path::absolute(&desc.location)?;
        let loc = location.to_string_lossy().into_owned();
        let containers = self.store.containers()?;
        if let Some(c) = containers.iter().find(|c| c.location == loc) {
            return Ok(c.id);
        }
        let format = desc.format.as_deref().unwrap_or("raw");
        let fmt = formats::by_id(format).ok_or_else(|| {
            Error::InvalidArg(format!("format {format:?} is not available in this build (M0 ships only \"raw\")"))
        })?;
        let file_name = location
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| Error::InvalidArg(format!("{loc} has no file name")))?;
        let platform = desc.platform.clone().unwrap_or_else(|| formats::infer_platform(&file_name).to_string());
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
                    let hlc = self.tick()?;
                    self.store.set_card(&card_id, mode, hlc)?;
                    let rec = LogRecord::Card(CardRecord { card: card_id.clone(), mode, hlc });
                    self.store.outbox_push(&rec.to_line())?;
                }
            }
            self.store.insert_container(&loc, format, &card_id, flags)
        })
    }

    pub fn remove_container(&mut self, id: i64) -> Result<()> {
        self.store.tx(|| self.store.delete_container(id))
    }

    /// Marks a container as open by the emulator. Results for busy containers go to the inbox
    /// and are applied by the first sync after it is released.
    pub fn set_busy(&mut self, container: i64, busy: bool) {
        if busy {
            self.busy.insert(container);
        } else {
            self.busy.remove(&container);
        }
    }

    pub fn containers(&self) -> Result<Vec<ContainerRow>> {
        self.store.containers()
    }

    pub fn cards(&self) -> Result<Vec<CardStatus>> {
        let containers = self.store.containers()?;
        let conflicts = self.store.open_conflicts()?;
        let inbox = self.store.inbox()?;
        let mut out = Vec::new();
        for (card_id, mode) in self.store.cards()? {
            let unit = whole_file_unit(&card_id);
            let c = containers.iter().find(|c| c.card_id == card_id);
            let lu = match c {
                Some(c) => self.store.local_unit(&unit, c.id)?,
                None => None,
            };
            out.push(CardStatus {
                card_id,
                mode,
                container_id: c.map(|c| c.id),
                location: c.map(|c| c.location.clone()),
                local_version: lu.and_then(|l| l.base),
                tips: self.tips(&unit)?.into_iter().map(|t| t.rec.id).collect(),
                conflict: conflicts.iter().find(|x| x.unit_id == unit).map(|x| x.id),
                inbox: inbox.iter().find(|i| i.0 == unit).map(|i| i.3.clone()),
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
        for c in &containers {
            self.scan(c, &mut report)?;
        }
        for c in &containers {
            self.reconcile(c, online, &mut report)?;
        }
        if online {
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
        Ok(!matches!(self.store.card(&c.card_id)?, Some((CardMode::Units, _)) if !fmt.supports_units()))
    }

    /// Quiescent snapshot of one container: if its content changed since the base version,
    /// record a new local version (parent = base) and queue it in the outbox.
    fn scan(&self, c: &ContainerRow, report: &mut SyncReport) -> Result<()> {
        let unit = whole_file_unit(&c.card_id);
        if !self.card_syncable(c)? {
            report.blocked.push(Blocked {
                unit,
                reason: "card is in unit mode in the repository; this build supports whole-file mode only".into(),
            });
            return Ok(());
        }
        let path = Path::new(&c.location);
        let image = match fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                report.blocked.push(Blocked { unit, reason: format!("{} does not exist", c.location) });
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        let fmt = self.format_of(c)?;
        if let Err(e) = fmt.validate(&image) {
            // A container that fails validation is never synced, so corruption doesn't spread.
            report.blocked.push(Blocked { unit, reason: e.to_string() });
            return Ok(());
        }
        let canon = fmt.extract(&image, &UnitRef { name: WHOLE_FILE_UNIT.into() })?;
        let hash = content_id(&canon.bytes);
        let lu = self.store.local_unit(&unit, c.id)?;
        if let Some(LocalUnit { content_hash, base: Some(_) }) = &lu {
            if *content_hash == hash {
                return Ok(());
            }
        }
        // Same bytes reached independently: adopt the tip as base, no new version.
        if let Some(t) = self.tips(&unit)?.into_iter().find(|t| t.rec.content == hash) {
            self.store.set_local_unit(&unit, c.id, &hash, Some(&t.rec.id))?;
            return Ok(());
        }
        // First sync of a container holding an older version (a stale copy of the card): adopt
        // that version, so reconciliation fast-forwards it instead of raising a conflict.
        if lu.as_ref().and_then(|l| l.base.as_ref()).is_none() {
            let mut known = self.store.versions_of(&unit)?;
            known.sort_by(|a, b| (b.rec.hlc, &b.rec.id).cmp(&(a.rec.hlc, &a.rec.id)));
            if let Some(v) = known.into_iter().find(|v| v.rec.content == hash) {
                self.store.set_local_unit(&unit, c.id, &hash, Some(&v.rec.id))?;
                return Ok(());
            }
        }
        let parents: Vec<String> = lu.and_then(|l| l.base).into_iter().collect();
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
        let rec = self.new_version(&unit, parents, &canon.bytes, meta)?;
        self.store.set_local_unit(&unit, c.id, &hash, Some(&rec.id))?;
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
            titles: vec![],
            tombstone: false,
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

    /// Reconciliation table (DESIGN §6.3) for one container. Local changes were already turned
    /// into versions by [`Self::scan`], so divergence shows up as more than one tip.
    fn reconcile(&self, c: &ContainerRow, online: bool, report: &mut SyncReport) -> Result<()> {
        let unit = whole_file_unit(&c.card_id);
        if !self.card_syncable(c)? {
            return Ok(());
        }
        let Some(lu) = self.store.local_unit(&unit, c.id)? else { return Ok(()) };
        let Some(base) = lu.base.clone() else { return Ok(()) };
        let tips = self.tips(&unit)?;
        let target = match tips.len() {
            0 => return Ok(()),
            // Nothing to do, or a fast-forward: the single tip descends from every version.
            1 => tips[0].clone(),
            _ => {
                let contents: HashSet<&str> = tips.iter().map(|t| t.rec.content.as_str()).collect();
                if contents.len() == 1 {
                    // Divergent histories with identical bytes: collapse them, no decision needed.
                    self.auto_merge(&unit, &tips, tips.last().unwrap())?
                } else {
                    match self.opts.policy {
                        ConflictPolicy::Ask => {
                            let ids: Vec<String> = tips.iter().map(|t| t.rec.id.clone()).collect();
                            let id = self.store.upsert_conflict(&unit, c.id, &base, &ids)?;
                            if !report.conflicts.contains(&id) {
                                report.conflicts.push(id);
                            }
                            return Ok(());
                        }
                        ConflictPolicy::Newest => self.auto_merge(&unit, &tips, newest(&tips))?,
                    }
                }
            }
        };
        self.store.close_conflicts(&unit)?;
        if target.rec.id == base {
            self.store.inbox_remove(&unit, c.id)?;
            return Ok(());
        }
        self.apply(c, &unit, &lu, &target, online, report)
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

    fn block(&self, c: &ContainerRow, unit: &str, target: &str, reason: String, report: &mut SyncReport) -> Result<()> {
        self.store.inbox_put(unit, c.id, target, &reason)?;
        report.blocked.push(Blocked { unit: unit.to_string(), reason });
        Ok(())
    }

    /// Writes `target` into the container (DESIGN §5 apply steps).
    fn apply(
        &self,
        c: &ContainerRow,
        unit: &str,
        lu: &LocalUnit,
        target: &StoredVersion,
        online: bool,
        report: &mut SyncReport,
    ) -> Result<()> {
        let t = &target.rec;
        if lu.content_hash == t.content {
            self.store.set_local_unit(unit, c.id, &t.content, Some(&t.id))?;
            self.store.inbox_remove(unit, c.id)?;
            return Ok(());
        }
        if c.flags & CF_READ_ONLY != 0 {
            return self.block(c, unit, &t.id, "container is read-only".into(), report);
        }
        if self.busy.contains(&c.id) {
            return self.block(c, unit, &t.id, "container is in use".into(), report);
        }
        let content = match self.assemble(t, online) {
            Ok(b) => b,
            Err(e) => return self.block(c, unit, &t.id, format!("content not available: {e}"), report),
        };
        let path = Path::new(&c.location);
        // 1. Read and validate the current container. It must still be what we scanned.
        let current = fs::read(path)?;
        if content_id(&current) != lu.content_hash {
            report.blocked.push(Blocked {
                unit: unit.to_string(),
                reason: "container changed during sync; it will be snapshotted next cycle".into(),
            });
            return Ok(());
        }
        let fmt = self.format_of(c)?;
        // 2. Build the new image.
        let canon = Canonical { name: WHOLE_FILE_UNIT.into(), bytes: content, meta: BTreeMap::new() };
        let image = match fmt.apply(&current, &[ApplyOp::Put(canon)]) {
            Ok(i) => i,
            Err(e) => return self.block(c, unit, &t.id, e.to_string(), report),
        };
        // 3. Re-parse and verify.
        fmt.validate(&image)?;
        let check = fmt.extract(&image, &UnitRef { name: WHOLE_FILE_UNIT.into() })?;
        if content_id(&check.bytes) != t.content {
            return Err(Error::Integrity(format!("rebuilt {} does not match version {}", c.location, t.id)));
        }
        // 4. Keep what we are about to overwrite.
        self.snapshot(unit, c.id, &current, lu.base.as_deref())?;
        // 5. Temp file, fsync, atomic replace.
        if let Err(e) = fsutil::atomic_replace(path, &image) {
            return self.block(c, unit, &t.id, format!("cannot replace {}: {e}", c.location), report);
        }
        self.store.tx(|| {
            self.store.set_local_unit(unit, c.id, &t.content, Some(&t.id))?;
            self.store.inbox_remove(unit, c.id)
        })?;
        report.applied.push(Applied { unit: unit.to_string(), version: t.id.clone(), location: c.location.clone() });
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
                    let blob = remote
                        .get(&chunk::blob_key(id))?
                        .ok_or_else(|| Error::Integrity(format!("blob {id} is missing from the remote")))?;
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
        let mut versions = 0;
        for line in seg.body.lines() {
            if let Ok(Some(LogRecord::Version(v))) = parse_record(line.as_bytes()) {
                versions += 1;
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
            match create_only(remote, &key, seg.body.as_bytes(), self.conditional_writes)? {
                PutOutcome::Created => break seq,
                PutOutcome::AlreadyExists => {
                    if remote.get(&key)?.as_deref() == Some(seg.body.as_bytes()) {
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
        report.uploaded_versions += versions;
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
        create_only(remote, &chunk::blob_key(id), &chunk::encode_blob(&data), self.conditional_writes)?;
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
        create_only(
            remote,
            &format!("devices/{dev}/device.json"),
            body.to_string().as_bytes(),
            self.conditional_writes,
        )?;
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
        let me = self.device_id()?;
        for dev in remote.list_dirs("devices/")? {
            if dev == me {
                continue;
            }
            if self.store.device_name(&dev)?.is_none() {
                if let Some(body) = remote.get(&format!("devices/{dev}/device.json"))? {
                    let name = serde_json::from_slice::<serde_json::Value>(&body)
                        .ok()
                        .and_then(|v| v.get("name").and_then(|n| n.as_str()).map(str::to_string));
                    if let Some(name) = name {
                        self.store.set_device_name(&dev, &name)?;
                    }
                }
            }
            let cursor = self.store.cursor(&dev)?;
            for key in remote.list(&format!("devices/{dev}/log/"), cursor.as_deref())? {
                if !key.ends_with(".ndjson") {
                    continue;
                }
                let Some(body) = remote.get(&key)? else { break };
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

    /// Accepts a unit id (`ps2:Mcd001:@card`), a card id (`ps2:Mcd001`) or a card name
    /// (`Mcd001`) and returns the unit id.
    pub fn resolve_unit(&self, arg: &str) -> Result<String> {
        if arg.ends_with(&format!(":{WHOLE_FILE_UNIT}")) {
            return Ok(arg.to_string());
        }
        if arg.contains(':') {
            return Ok(whole_file_unit(arg));
        }
        let matches: Vec<String> = self
            .store
            .cards()?
            .into_iter()
            .map(|(id, _)| id)
            .filter(|id| id.split_once(':').map(|(_, n)| n) == Some(arg))
            .collect();
        match matches.as_slice() {
            [one] => Ok(whole_file_unit(one)),
            [] => Err(Error::NotFound(format!("no card named {arg:?}"))),
            many => Err(Error::InvalidArg(format!("{arg:?} is ambiguous: {}", many.join(", ")))),
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

    fn local_bases(&self, unit: &str) -> Result<HashSet<String>> {
        let mut out = HashSet::new();
        for c in self.store.containers()? {
            if whole_file_unit(&c.card_id) == unit {
                if let Some(LocalUnit { base: Some(b), .. }) = self.store.local_unit(unit, c.id)? {
                    out.insert(b);
                }
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
        for c in self.store.containers()? {
            if whole_file_unit(&c.card_id) == unit {
                self.scan(&c, &mut report)?;
            }
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

//! The sidecar's decision logic (DESIGN §6.4), kept free of real time and real processes so
//! tests can drive it: callers pass `now` and file events in, and the daemon decides when to
//! run a sync cycle.
//!
//! - Writes to a card arm a debounce; the cycle runs once they have settled.
//! - While an emulator runs, its cards are marked busy, so remote changes wait in the inbox.
//!   Local changes are still snapshotted and uploaded.
//! - When the emulator exits, a cycle applies whatever waited.
//! - The remote is polled on a back-off timer, and right away when the emulator starts.
//! - On shutdown, a final cycle runs unless an emulator is running.
//! - Folder cards (DESIGN §19) are hashed as tree images and watched recursively. emusync's own
//!   directory replace swaps the folder, so its watch is renewed afterwards.

use super::process::ProcessProbe;
use super::profile::Profile;
use emusync_core::{fsutil, short_id, split_unit, ContainerDesc, ContainerRow, Engine, SyncReport, LOCKED};
use emusync_formats::ContainerKind;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct Timing {
    /// Quiet time after the last write before a card is snapshotted.
    pub debounce: Duration,
    /// Shortest and longest interval between polls of the remote.
    pub poll_min: Duration,
    pub poll_max: Duration,
    /// How often the process list is checked.
    pub probe_every: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            debounce: Duration::from_secs(2),
            poll_min: Duration::from_secs(30),
            poll_max: Duration::from_secs(15 * 60),
            probe_every: Duration::from_secs(2),
        }
    }
}

/// Why a cycle ran, in increasing priority (a pending cycle takes the strongest reason).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Trigger {
    Poll,
    Startup,
    Changed,
    Started,
    Exited,
    /// The watcher is stopping (never pending; see [`Daemon::shutdown`]).
    Shutdown,
}

impl fmt::Display for Trigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Trigger::Poll => "poll",
            Trigger::Startup => "startup",
            Trigger::Changed => "card changed",
            Trigger::Started => "emulator started",
            Trigger::Exited => "emulator exited",
            Trigger::Shutdown => "final sync",
        })
    }
}

/// What one cycle did.
#[derive(Debug, Clone)]
pub struct Cycle {
    pub trigger: Trigger,
    /// The sync failed (the error was logged).
    pub failed: bool,
    pub applied: usize,
    /// Remote changes held back because the emulator is running.
    pub held_back: usize,
}

/// Hash of a card's content, to tell a real write from our own reads, our own applies or a
/// spurious event. Size and mtime are not enough: cards have a fixed size, and a coarse mtime
/// can miss a second write within the same tick. A folder card is hashed as its tree image,
/// modification times included, as the engine reads it.
type Fingerprint = Option<blake3::Hash>;

fn read_fingerprint(path: &Path, folder: bool) -> std::io::Result<blake3::Hash> {
    let image = if folder { fsutil::read_tree(path).map(|t| t.encode()) } else { std::fs::read(path) };
    image.map(|b| blake3::hash(&b))
}

fn fingerprint(path: &Path, folder: bool) -> Fingerprint {
    read_fingerprint(path, folder).ok()
}

/// Whether a registered card is a directory (a folder card) rather than a file.
fn is_folder(c: &ContainerRow) -> bool {
    emusync_formats::by_id(&c.format).is_some_and(|f| f.kind() == ContainerKind::Directory)
}

/// Paths compare without regard to case on Windows and macOS, whose file systems usually
/// ignore it.
fn path_key(path: &Path) -> String {
    let s = path.to_string_lossy();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    if cfg!(any(windows, target_os = "macos")) {
        s.replace('\\', "/").to_lowercase()
    } else {
        s.to_string()
    }
}

/// A folder to watch for changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchDir {
    pub path: PathBuf,
    /// Watch everything under it: folder cards keep saves in subdirectories.
    pub recursive: bool,
}

/// How long a new folder in a card folder is checked for a folder card marker.
const CANDIDATE_TTL: Duration = Duration::from_secs(60);

/// The engine reports this reason for units it routed to the inbox because of `set_busy`.
const IN_USE: &str = "container is in use";

pub struct Daemon<P> {
    engine: Engine,
    profiles: Vec<Profile>,
    probe: P,
    timing: Timing,
    discover: bool,
    running: Vec<bool>,
    probed: bool,
    pending: Option<Trigger>,
    last_change: Option<Instant>,
    next_probe: Instant,
    next_poll: Instant,
    poll_interval: Duration,
    /// Card fingerprints as of the last cycle, by path key.
    fingerprints: HashMap<String, Fingerprint>,
    /// Registered folder cards, by path key.
    folders: HashMap<String, PathBuf>,
    /// Folder cards that were replaced, so their recursive watch follows the old directory.
    rewatch: Vec<PathBuf>,
    /// New folders in card folders that may still become folder cards, by path key, with when
    /// they were seen.
    candidates: HashMap<String, (PathBuf, Instant)>,
    /// Card files that discovery could not register (warned about once).
    skipped: HashSet<String>,
    /// Units held back by the last cycle (logged once, not on every poll).
    held: HashSet<String>,
    /// Cards another program had locked in the last cycle (logged once, not on every poll).
    locked: HashSet<String>,
    offline: bool,
}

impl<P: ProcessProbe> Daemon<P> {
    pub fn new(engine: Engine, profiles: Vec<Profile>, probe: P, timing: Timing, discover: bool, now: Instant) -> Self {
        let n = profiles.len();
        Self {
            engine,
            profiles,
            probe,
            discover,
            running: vec![false; n],
            probed: false,
            pending: Some(Trigger::Startup),
            last_change: None,
            next_probe: now,
            next_poll: now + timing.poll_min,
            poll_interval: timing.poll_min,
            timing,
            fingerprints: HashMap::new(),
            folders: HashMap::new(),
            rewatch: Vec::new(),
            candidates: HashMap::new(),
            skipped: HashSet::new(),
            held: HashSet::new(),
            locked: HashSet::new(),
            offline: false,
        }
    }

    #[cfg(test)]
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    #[cfg(test)]
    pub fn poll_interval(&self) -> Duration {
        self.poll_interval
    }

    /// When [`Daemon::tick`] next has something to do.
    pub fn next_wakeup(&self) -> Instant {
        let mut t = self.next_probe.min(self.next_poll);
        if let Some(c) = self.last_change {
            t = t.min(c + self.timing.debounce);
        }
        t
    }

    /// Folders to watch: those holding registered cards, registered folder cards themselves
    /// (recursively), plus the profiles' card folders when discovery is on.
    pub fn watch_dirs(&self) -> Vec<WatchDir> {
        let flat = |path: &Path| WatchDir { path: path.to_path_buf(), recursive: false };
        let mut dirs = Vec::new();
        for c in self.engine.containers().unwrap_or_default() {
            let p = Path::new(&c.location);
            dirs.extend(p.parent().map(flat));
            if is_folder(&c) {
                dirs.push(WatchDir { path: p.to_path_buf(), recursive: true });
            }
        }
        if self.discover {
            dirs.extend(self.profiles.iter().flat_map(|p| p.card_dirs.iter().map(|d| flat(d))));
        }
        dirs.retain(|d| d.path.is_dir());
        dirs.sort_by(|a, b| path_key(&a.path).cmp(&path_key(&b.path)).then(b.recursive.cmp(&a.recursive)));
        dirs.dedup_by(|later, first| path_key(&later.path) == path_key(&first.path));
        dirs
    }

    /// Folder cards whose watch must be dropped and set up again: the directory was replaced
    /// (by our own apply, or from outside), and the old watch follows the old directory.
    pub fn take_rewatch(&mut self) -> Vec<PathBuf> {
        let mut cards = std::mem::take(&mut self.rewatch);
        cards.sort_by_key(|p| path_key(p));
        cards.dedup_by_key(|p| path_key(p));
        cards
    }

    /// A path in a watched folder was created, removed or renamed. A folder card replaced
    /// that way (by another program; our own applies are handled in the cycle) needs a new
    /// watch. Writes inside a folder card also report the folder as modified, but those keep it.
    pub fn on_moved(&mut self, path: &Path) {
        if let Some(card) = self.folders.get(&path_key(path)) {
            self.rewatch.push(card.clone());
        }
    }

    /// A path in a watched folder changed. Only changes to cards arm the debounce: a card
    /// file, anything inside a folder card, or a new folder that may become one.
    pub fn on_event(&mut self, path: &Path, now: Instant) {
        // emusync's own replace works in sibling scratch paths (DESIGN §19.3).
        if path.ancestors().any(fsutil::is_scratch) {
            return;
        }
        let candidate = self.may_become_folder_card(path);
        if candidate {
            self.candidates.insert(path_key(path), (path.to_path_buf(), now));
        }
        let is_card = candidate
            || path.ancestors().any(|a| self.fingerprints.contains_key(&path_key(a)))
            || self.profiles.iter().any(|p| p.claims(path));
        if is_card {
            log::debug!("change: {}", path.display());
            self.last_change = Some(now);
        }
    }

    /// A new folder in a card folder: PCSX2 creates a folder card's directory before its
    /// superblock file, and the card folder's flat watch doesn't see the superblock arrive.
    /// Such folders are checked for the marker for a while ([`Daemon::check_candidates`]).
    fn may_become_folder_card(&self, path: &Path) -> bool {
        let Some(parent) = path.parent().map(path_key) else { return false };
        self.discover
            && path.is_dir()
            && self
                .profiles
                .iter()
                .any(|p| p.folder_card_marker.is_some() && p.card_dirs.iter().any(|d| path_key(d) == parent))
    }

    /// Arms the debounce once a candidate folder's marker has appeared, which can be after
    /// the debounce its creation armed has expired. Gives up on folders that are gone or that
    /// stayed plain folders for [`CANDIDATE_TTL`].
    fn check_candidates(&mut self, now: Instant) {
        let (profiles, fingerprints) = (&self.profiles, &self.fingerprints);
        let mut arrived = false;
        self.candidates.retain(|key, (path, seen)| {
            if fingerprints.contains_key(key) {
                return false;
            }
            if profiles.iter().any(|p| p.claims(path)) {
                log::debug!("new folder card: {}", path.display());
                arrived = true;
                return false;
            }
            path.is_dir() && now.duration_since(*seen) < CANDIDATE_TTL
        });
        // A debounce already armed finds the new card when it expires.
        if arrived && self.last_change.is_none() {
            self.last_change = Some(now);
        }
    }

    fn want(&mut self, t: Trigger) {
        self.pending = Some(self.pending.map_or(t, |p| p.max(t)));
    }

    /// Checks processes, timers and the debounce, and runs a sync cycle if one is due.
    pub fn tick(&mut self, now: Instant) -> Option<Cycle> {
        if now >= self.next_probe {
            self.next_probe = now + self.timing.probe_every;
            self.probe_processes();
            self.check_candidates(now);
        }
        if let Some(t) = self.last_change {
            if now.duration_since(t) >= self.timing.debounce {
                self.last_change = None;
                let (changed, locked) = self.local_changed();
                if changed {
                    self.want(Trigger::Changed);
                } else if let Some(card) = locked {
                    // Windows: the emulator keeps the card locked while a game runs. The cycle
                    // that runs once it releases the card (game closed, emulator exited) syncs it.
                    log::debug!("change events on {card}, which is locked; it syncs once released");
                } else {
                    log::debug!("change events, but no card file changed");
                }
            }
        }
        if now >= self.next_poll {
            self.want(Trigger::Poll);
        }
        let trigger = self.pending?;
        // Never snapshot a card halfway through being written. Once the emulator has exited,
        // its writes are complete.
        if self.last_change.is_some() && trigger != Trigger::Exited {
            return None;
        }
        self.pending = None;
        self.last_change = None;
        Some(self.cycle(trigger, now))
    }

    /// The watcher is stopping: runs a final cycle, so writes still settling and remote
    /// changes waiting for the emulator to exit aren't left for the next start. Skipped while
    /// an emulator runs, since its card may be halfway through a write; its saves were
    /// uploaded as they settled, and the next start picks up the rest.
    pub fn shutdown(&mut self, now: Instant) -> Option<Cycle> {
        self.probe_processes();
        if self.running.iter().any(|&r| r) {
            log::info!("{} is running: no final sync", self.running_names());
            return None;
        }
        self.pending = None;
        self.last_change = None;
        Some(self.cycle(Trigger::Shutdown, now))
    }

    fn probe_processes(&mut self) {
        let names = self.probe.running();
        for (i, p) in self.profiles.iter().enumerate() {
            let up = names.iter().any(|n| p.matches_process(n)) || p.lock_files.iter().any(|f| f.exists());
            if up == self.running[i] {
                continue;
            }
            self.running[i] = up;
            if !self.probed {
                log::info!("{} is running: its cards are not written to until it exits", p.name);
            } else if up {
                log::info!("{} started: its cards are not written to until it exits", p.name);
                self.pending = Some(self.pending.map_or(Trigger::Started, |t| t.max(Trigger::Started)));
            } else {
                log::info!("{} exited", p.name);
                self.pending = Some(Trigger::Exited);
            }
        }
        self.probed = true;
    }

    /// Whether the card at `location` may be open in a running emulator. Cards that no
    /// profile claims are treated as open whenever any watched emulator runs.
    fn in_use(&self, location: &Path) -> bool {
        let owners: Vec<usize> = (0..self.profiles.len()).filter(|&i| self.profiles[i].claims(location)).collect();
        if owners.is_empty() {
            self.running.iter().any(|&r| r)
        } else {
            owners.iter().any(|&i| self.running[i])
        }
    }

    fn running_names(&self) -> String {
        let names: Vec<&str> =
            self.profiles.iter().zip(&self.running).filter(|(_, &r)| r).map(|(p, _)| p.name.as_str()).collect();
        if names.is_empty() {
            "the emulator".into()
        } else {
            names.join(", ")
        }
    }

    /// Card files in the profiles' folders that aren't registered yet.
    fn undiscovered(&self) -> Vec<PathBuf> {
        if !self.discover {
            return Vec::new();
        }
        let known: HashSet<String> =
            self.engine.containers().unwrap_or_default().iter().map(|c| path_key(Path::new(&c.location))).collect();
        self.profiles
            .iter()
            .flat_map(|p| p.discover())
            .filter(|f| {
                let k = path_key(f);
                !known.contains(&k) && !self.skipped.contains(&k)
            })
            .collect()
    }

    /// Whether a card's content changed since the last cycle, and the first card that couldn't
    /// be read because another program has it locked. A locked card doesn't count as changed:
    /// it can't be snapshotted until it is released.
    fn local_changed(&self) -> (bool, Option<String>) {
        let containers = self.engine.containers().unwrap_or_default();
        let mut locked = None;
        let mut changed = false;
        for c in &containers {
            let p = Path::new(&c.location);
            let now = match read_fingerprint(p, is_folder(c)) {
                Ok(h) => Some(h),
                Err(e) if fsutil::is_locked(&e) => {
                    locked.get_or_insert_with(|| c.location.clone());
                    continue;
                }
                Err(_) => None,
            };
            if self.fingerprints.get(&path_key(p)) != Some(&now) {
                changed = true;
            }
        }
        (changed || !self.undiscovered().is_empty(), locked)
    }

    fn register_new_cards(&mut self) {
        for path in self.undiscovered() {
            match self.engine.add_container(ContainerDesc { location: path.clone(), ..Default::default() }) {
                Ok(_) => log::info!("found {}: now syncing it", path.display()),
                Err(e) => {
                    log::warn!("not syncing {}: {e}", path.display());
                    self.skipped.insert(path_key(&path));
                }
            }
        }
    }

    fn cycle(&mut self, trigger: Trigger, now: Instant) -> Cycle {
        let result = self.sync_once();
        let mut cycle = Cycle { trigger, failed: false, applied: 0, held_back: 0 };
        let active = match &result {
            Ok((report, busy)) => {
                cycle.applied = report.applied.len();
                cycle.held_back = self.log_report(report, busy, trigger);
                !report.offline
                    && (report.pulled_records > 0
                        || report.new_local_versions > 0
                        || report.uploaded_versions > 0
                        || !report.applied.is_empty())
            }
            Err(e) => {
                log::error!("sync failed ({trigger}): {e}");
                cycle.failed = true;
                false
            }
        };
        let offline = result.as_ref().map_or(true, |(r, _)| r.offline);
        // Back off while nothing happens or the remote is unreachable; anything else (a local
        // change, the emulator starting or exiting, news from the remote) resets the timer.
        self.poll_interval = if !offline && (active || trigger != Trigger::Poll) {
            self.timing.poll_min
        } else {
            (self.poll_interval * 2).min(self.timing.poll_max)
        };
        self.next_poll = now + self.poll_interval;
        log::debug!("next poll in {}s", self.poll_interval.as_secs());
        cycle
    }

    /// Marks cards busy as needed and runs one engine cycle. Returns the report and the cards
    /// that were busy.
    fn sync_once(&mut self) -> emusync_core::Result<(SyncReport, HashSet<String>)> {
        self.register_new_cards();
        let containers = self.engine.containers()?;
        let mut busy = HashSet::new();
        let mut before = HashMap::new();
        self.folders.clear();
        for c in &containers {
            let p = Path::new(&c.location);
            let in_use = self.in_use(p);
            self.engine.set_busy(c.id, in_use);
            if in_use {
                busy.insert(c.card_id.clone());
            }
            let folder = is_folder(c);
            if folder {
                self.folders.insert(path_key(p), p.to_path_buf());
            }
            before.insert(path_key(p), fingerprint(p, folder));
        }
        let report = self.engine.sync()?;
        // Fingerprints from before the cycle, so a write that lands during it still counts as
        // a change; cards this cycle wrote are re-read.
        for a in &report.applied {
            let p = Path::new(&a.location);
            let key = path_key(p);
            let folder = self.folders.contains_key(&key);
            if folder {
                self.rewatch.push(p.to_path_buf());
            }
            before.insert(key, fingerprint(p, folder));
        }
        self.fingerprints = before;
        Ok((report, busy))
    }

    /// Logs what a cycle did; returns how many remote changes were held back.
    fn log_report(&mut self, r: &SyncReport, busy: &HashSet<String>, trigger: Trigger) -> usize {
        if let Some((old, new)) = &r.reminted {
            log::warn!("device id {old} was in use by another installation; now {new}");
        }
        for a in &r.applied {
            log::info!("applied   {} ← {} ({})", crate::target_of(&a.unit), short_id(&a.version), a.location);
        }
        let mut held = HashSet::new();
        let mut locked = HashSet::new();
        for b in &r.blocked {
            let card = split_unit(&b.unit).map_or(b.unit.as_str(), |(c, _)| c);
            if b.reason == LOCKED {
                // Logged once while it lasts: every poll would otherwise repeat it.
                let first = !self.locked.contains(card);
                if busy.contains(card) {
                    let level = if first { log::Level::Info } else { log::Level::Debug };
                    log::log!(
                        level,
                        "waiting   {card}: {} has it open; it syncs when {} closes the game",
                        self.running_names(),
                        self.running_names()
                    );
                } else if first {
                    log::warn!("blocked   {card}: locked by another program; it syncs once released");
                } else {
                    log::debug!("blocked   {card}: still locked");
                }
                locked.insert(card.to_string());
            } else if b.reason == IN_USE && busy.contains(card) {
                let level = if self.held.contains(&b.unit) { log::Level::Debug } else { log::Level::Info };
                log::log!(
                    level,
                    "held back {}: newer version waits until {} exits",
                    crate::target_of(&b.unit),
                    self.running_names()
                );
                held.insert(b.unit.clone());
            } else {
                log::warn!("blocked   {}: {}", crate::target_of(&b.unit), b.reason);
            }
        }
        for u in &r.resolved {
            let t = crate::target_of(u);
            log::info!("diverged  {t}: kept the newest version; the other is in `emusync history {t}`");
        }
        for m in &r.mode_changes {
            log::info!("mode      {m} changed on another device");
        }
        for w in &r.warnings {
            log::warn!("{w}");
        }
        if !r.conflicts.is_empty() {
            for c in self.engine.conflicts().unwrap_or_default() {
                if r.conflicts.contains(&c.id) {
                    log::warn!("CONFLICT  {}: devices diverged; see `emusync conflicts`", crate::target_of(&c.unit));
                }
            }
        }
        if r.offline && !self.offline {
            log::warn!("remote unreachable; working offline ({} record(s) waiting to upload)", r.pending_uploads);
        } else if !r.offline && self.offline {
            log::info!("remote reachable again");
        }
        self.offline = r.offline;
        let held_back = held.len();
        self.held = held;
        for card in self.locked.difference(&locked) {
            log::info!("released  {card}");
        }
        self.locked = locked;
        let summary = format!(
            "{trigger}: pulled {} version(s), {} new local, uploaded {} version(s){}",
            r.pulled_records,
            r.new_local_versions,
            r.uploaded_versions,
            if r.pending_uploads > 0 { format!(", {} waiting to upload", r.pending_uploads) } else { String::new() }
        );
        if r.pulled_records + r.new_local_versions + r.uploaded_versions > 0 || trigger != Trigger::Poll {
            log::info!("{summary}");
        } else {
            log::debug!("{summary}");
        }
        held_back
    }
}

/// Registered cards that no profile claims, for a startup warning.
pub fn unclaimed(containers: &[ContainerRow], profiles: &[Profile]) -> Vec<String> {
    containers
        .iter()
        .filter(|c| !profiles.iter().any(|p| p.claims(Path::new(&c.location))))
        .map(|c| c.location.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::process::FakeProbe;
    use super::*;
    use emusync_core::{ConflictPolicy, EngineOptions};
    use emusync_remote::{MemoryRemote, MemoryRemoteOptions};
    use std::sync::Arc;

    const EMU: &str = "fakeemu.exe";

    struct World {
        dir: tempfile::TempDir,
        remote: Arc<MemoryRemote>,
    }

    impl World {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().unwrap(),
                remote: Arc::new(MemoryRemote::new(MemoryRemoteOptions::default())),
            }
        }

        fn engine(&self, device: &str) -> Engine {
            let opts = EngineOptions {
                device_name: Some(device.into()),
                emulator_id: "test".into(),
                policy: ConflictPolicy::Newest,
                ..Default::default()
            };
            let mut e = Engine::open(self.dir.path().join(device).join("data"), opts).unwrap();
            e.add_remote(self.remote.clone()).unwrap();
            e
        }

        /// The card folder of a device, holding `card.bin`.
        fn card(&self, device: &str, content: &[u8]) -> PathBuf {
            let dir = self.dir.path().join(device).join("cards");
            std::fs::create_dir_all(&dir).unwrap();
            let p = dir.join("card.bin");
            std::fs::write(&p, content).unwrap();
            p
        }

        /// Device A, run by the daemon, which discovers its card.
        fn daemon(&self, content: &[u8], t0: Instant) -> (Daemon<FakeProbe>, FakeProbe, PathBuf) {
            let card = self.card("a", content);
            let profile = Profile {
                id: "fake".into(),
                name: "FakeEmu".into(),
                card_extensions: vec!["bin".into()],
                folder_card_marker: None,
                processes: vec![EMU.into()],
                card_dirs: vec![card.parent().unwrap().to_path_buf()],
                lock_files: vec![],
            };
            let probe = FakeProbe::default();
            let mut d = Daemon::new(self.engine("a"), vec![profile], probe.clone(), Timing::default(), true, t0);
            let c = d.tick(t0).expect("startup cycle");
            assert_eq!(c.trigger, Trigger::Startup);
            assert!(!c.failed);
            assert_eq!(d.engine().containers().unwrap().len(), 1, "card discovered");
            (d, probe, card)
        }

        /// Device B, synced directly, with the same logical card.
        fn other(&self, content: &[u8]) -> (Engine, PathBuf) {
            let mut e = self.engine("b");
            let card = self.card("b", content);
            e.add_container(ContainerDesc { location: card.clone(), ..Default::default() }).unwrap();
            e.sync().unwrap();
            (e, card)
        }
    }

    fn s(secs: f64) -> Duration {
        Duration::from_secs_f64(secs)
    }

    #[test]
    fn busy_while_running_then_apply_after_exit() {
        let w = World::new();
        let t0 = Instant::now();
        let (mut d, probe, card_a) = w.daemon(b"v1", t0);
        let (mut b, card_b) = w.other(b"v1");

        // The emulator starts: the remote is polled at once, and the card is now busy.
        probe.set(&[EMU]);
        let c = d.tick(t0 + s(2.0)).expect("cycle on start");
        assert_eq!(c.trigger, Trigger::Started);

        // Another device saves; the next poll fetches it but must not touch the open card.
        std::fs::write(&card_b, b"v2 from b").unwrap();
        assert_eq!(b.sync().unwrap().uploaded_versions, 1);
        assert!(d.tick(t0 + s(10.0)).is_none(), "no poll before the interval");
        let c = d.tick(t0 + s(33.0)).expect("poll");
        assert_eq!(c.trigger, Trigger::Poll);
        assert_eq!((c.applied, c.held_back), (0, 1));
        assert_eq!(std::fs::read(&card_a).unwrap(), b"v1");
        assert_eq!(d.engine().inbox().unwrap().len(), 1);

        // Still running at the next poll: still held back.
        let c = d.tick(t0 + s(64.0)).expect("poll");
        assert_eq!(c.held_back, 1);
        assert_eq!(std::fs::read(&card_a).unwrap(), b"v1");

        // The emulator exits: the waiting version is applied right away.
        probe.set(&[]);
        let c = d.tick(t0 + s(66.0)).expect("cycle on exit");
        assert_eq!(c.trigger, Trigger::Exited);
        assert_eq!((c.applied, c.held_back), (1, 0));
        assert_eq!(std::fs::read(&card_a).unwrap(), b"v2 from b");
        assert!(d.engine().inbox().unwrap().is_empty());

        // Our own write to the card is not mistaken for a new save.
        d.on_event(&card_a, t0 + s(66.1));
        assert!(d.tick(t0 + s(69.0)).is_none());
    }

    #[test]
    fn local_saves_upload_while_running_and_divergence_keeps_both() {
        let w = World::new();
        let t0 = Instant::now();
        let (mut d, probe, card_a) = w.daemon(b"v1", t0);
        let (mut b, card_b) = w.other(b"v1");
        probe.set(&[EMU]);
        d.tick(t0 + s(2.0)).expect("cycle on start");

        // B saves first; A's poll holds it back.
        std::fs::write(&card_b, b"v2 from b").unwrap();
        b.sync().unwrap();
        assert_eq!(d.tick(t0 + s(33.0)).expect("poll").held_back, 1);

        // Then the emulator on A saves: it is uploaded even though the card is busy, and the
        // newest version wins.
        std::fs::write(&card_a, b"v3 from a").unwrap();
        d.on_event(&card_a, t0 + s(40.0));
        let c = d.tick(t0 + s(42.5)).expect("debounced cycle");
        assert_eq!(c.trigger, Trigger::Changed);
        assert_eq!(std::fs::read(&card_a).unwrap(), b"v3 from a");
        b.sync().unwrap();
        assert_eq!(std::fs::read(&card_b).unwrap(), b"v3 from a");

        // The losing version stays in history.
        let history = d.engine().history("card").unwrap();
        assert!(history.iter().any(|h| d.engine().read_version(&h.version.id).unwrap() == b"v2 from b"));

        // Nothing is left to apply after exit: the newest version is already on the card.
        probe.set(&[]);
        let c = d.tick(t0 + s(45.0)).expect("cycle on exit");
        assert_eq!(c.applied, 0);
        assert_eq!(std::fs::read(&card_a).unwrap(), b"v3 from a");
    }

    /// On Windows, PCSX2 keeps a card opened without sharing while a game runs, so it can't be
    /// read. The watcher waits for the card to be released instead of failing cycles or
    /// warning on every poll, then syncs it.
    #[cfg(windows)]
    #[test]
    fn a_locked_card_syncs_once_released() {
        use std::os::windows::fs::OpenOptionsExt;
        let w = World::new();
        let t0 = Instant::now();
        let (mut d, probe, card_a) = w.daemon(b"v1", t0);
        let (mut b, card_b) = w.other(b"v1");
        probe.set(&[EMU]);
        d.tick(t0 + s(2.0)).expect("cycle on start");

        // The emulator saves and keeps the card locked.
        std::fs::write(&card_a, b"v2 from a").unwrap();
        let lock = std::fs::OpenOptions::new().read(true).write(true).share_mode(0).open(&card_a).unwrap();
        d.on_event(&card_a, t0 + s(5.0));
        assert!(d.tick(t0 + s(8.0)).is_none(), "a locked card can't be snapshotted yet");

        // Polls still run and don't fail; the card is reported as locked, not as an error.
        let c = d.tick(t0 + s(33.0)).expect("poll");
        assert!(!c.failed);
        assert_eq!(d.locked.len(), 1);
        // A quiet poll doubles the interval: the next one is a minute later.
        let c = d.tick(t0 + s(94.0)).expect("poll");
        assert!(!c.failed);

        // The game is closed and the emulator exits: the card is read and the save uploaded.
        drop(lock);
        probe.set(&[]);
        let c = d.tick(t0 + s(96.0)).expect("cycle on exit");
        assert_eq!(c.trigger, Trigger::Exited);
        assert!(d.locked.is_empty());
        b.sync().unwrap();
        assert_eq!(std::fs::read(&card_b).unwrap(), b"v2 from a");
    }

    #[test]
    fn writes_are_debounced() {
        let w = World::new();
        let t0 = Instant::now();
        let (mut d, _probe, card_a) = w.daemon(b"v1", t0);
        let (mut b, card_b) = w.other(b"v1");

        // Two writes 1.5 s apart: one cycle, 2 s after the second.
        std::fs::write(&card_a, b"half").unwrap();
        d.on_event(&card_a, t0 + s(5.0));
        assert!(d.tick(t0 + s(6.0)).is_none());
        std::fs::write(&card_a, b"v2 complete").unwrap();
        d.on_event(&card_a, t0 + s(6.5));
        assert!(d.tick(t0 + s(8.0)).is_none(), "2 s since the first write, not the last");
        let c = d.tick(t0 + s(8.6)).expect("settled");
        assert_eq!(c.trigger, Trigger::Changed);
        assert!(d.tick(t0 + s(9.0)).is_none());
        b.sync().unwrap();
        assert_eq!(std::fs::read(&card_b).unwrap(), b"v2 complete");

        // A poll that falls due while a write is settling waits for it.
        let next_poll = t0 + s(8.6) + d.poll_interval();
        std::fs::write(&card_a, b"v3").unwrap();
        d.on_event(&card_a, next_poll - s(0.5));
        assert!(d.tick(next_poll + s(0.1)).is_none());
        let c = d.tick(next_poll + s(1.6)).expect("settled");
        assert_eq!(c.trigger, Trigger::Changed);

        // A same-size write that keeps the old mtime is still a change.
        let mtime = std::fs::metadata(&card_a).unwrap().modified().unwrap();
        std::fs::write(&card_a, b"v4").unwrap();
        std::fs::File::options().write(true).open(&card_a).unwrap().set_modified(mtime).unwrap();
        d.on_event(&card_a, next_poll + s(1.7));
        let c = d.tick(next_poll + s(3.8)).expect("same size, same mtime, new content");
        assert_eq!(c.trigger, Trigger::Changed);
        b.sync().unwrap();
        assert_eq!(std::fs::read(&card_b).unwrap(), b"v4");

        // Events for other files don't arm the debounce.
        d.on_event(&card_a.with_extension("bin.emusync-tmp"), next_poll + s(4.0));
        assert!(d.tick(next_poll + s(6.5)).is_none());
    }

    #[test]
    fn polling_backs_off_and_resets_on_news() {
        let w = World::new();
        let t0 = Instant::now();
        let (mut d, _probe, card_a) = w.daemon(b"v1", t0);
        let (mut b, card_b) = w.other(b"v1");
        let mut t = t0;
        let mut seen = Vec::new();
        for _ in 0..7 {
            t += d.poll_interval();
            assert_eq!(d.tick(t).expect("poll").trigger, Trigger::Poll);
            seen.push(d.poll_interval().as_secs());
        }
        // B adopted A's identical card without publishing anything, so no poll has news.
        assert_eq!(seen, [60, 120, 240, 480, 900, 900, 900]);

        std::fs::write(&card_b, b"v2").unwrap();
        b.sync().unwrap();
        t += d.poll_interval();
        let c = d.tick(t).expect("poll");
        assert_eq!(c.applied, 1);
        assert_eq!(d.poll_interval().as_secs(), 30);
        assert_eq!(std::fs::read(&card_a).unwrap(), b"v2");

        // Offline: back off too.
        w.remote.set_offline(true);
        t += d.poll_interval();
        d.tick(t).expect("poll");
        assert_eq!(d.poll_interval().as_secs(), 60);
    }

    #[test]
    fn shutdown_syncs_unless_the_emulator_runs() {
        let w = World::new();
        let t0 = Instant::now();
        let (mut d, probe, card_a) = w.daemon(b"v1", t0);
        let (mut b, card_b) = w.other(b"v1");

        // A write that hasn't settled yet is uploaded by the final sync.
        std::fs::write(&card_a, b"v2 from a").unwrap();
        d.on_event(&card_a, t0 + s(5.0));
        let c = d.shutdown(t0 + s(5.5)).expect("final sync");
        assert_eq!(c.trigger, Trigger::Shutdown);
        assert!(!c.failed);
        b.sync().unwrap();
        assert_eq!(std::fs::read(&card_b).unwrap(), b"v2 from a");

        // A remote change waiting for the emulator is applied once it has exited.
        probe.set(&[EMU]);
        d.tick(t0 + s(10.0)).expect("cycle on start");
        std::fs::write(&card_b, b"v3 from b").unwrap();
        b.sync().unwrap();
        assert_eq!(d.tick(t0 + s(41.0)).expect("poll").held_back, 1);
        assert!(d.shutdown(t0 + s(42.0)).is_none(), "no final sync while the emulator runs");
        assert_eq!(std::fs::read(&card_a).unwrap(), b"v2 from a");
        probe.set(&[]);
        let c = d.shutdown(t0 + s(43.0)).expect("final sync");
        assert_eq!(c.applied, 1);
        assert_eq!(std::fs::read(&card_a).unwrap(), b"v3 from b");
    }

    #[test]
    fn already_running_at_startup_holds_back() {
        let w = World::new();
        let (_b, _) = w.other(b"v1 from b");
        let t0 = Instant::now();
        let card_a = w.card("a", b"v1 from a");
        let profile = Profile {
            id: "fake".into(),
            name: "FakeEmu".into(),
            card_extensions: vec!["bin".into()],
            folder_card_marker: None,
            processes: vec![EMU.into()],
            card_dirs: vec![card_a.parent().unwrap().to_path_buf()],
            lock_files: vec![],
        };
        let probe = FakeProbe::default();
        probe.set(&["other.exe", "FAKEEMU.EXE"]);
        let mut d = Daemon::new(w.engine("a"), vec![profile], probe.clone(), Timing::default(), true, t0);
        // Both devices had the card before their first sync; between first versions the older
        // (B's) wins, but A's card is open and must not be written yet.
        let c = d.tick(t0).expect("startup");
        assert_eq!(c.trigger, Trigger::Startup);
        assert_eq!(c.held_back, 1);
        assert_eq!(std::fs::read(&card_a).unwrap(), b"v1 from a");

        probe.set(&[]);
        let c = d.tick(t0 + s(2.0)).expect("exit");
        assert_eq!((c.trigger, c.applied), (Trigger::Exited, 1));
        assert_eq!(std::fs::read(&card_a).unwrap(), b"v1 from b");
        let history = d.engine().history("card").unwrap();
        assert!(history.iter().any(|h| d.engine().read_version(&h.version.id).unwrap() == b"v1 from a"));
    }

    mod folder {
        //! A PCSX2 folder card on device A, run by the daemon, against a `.ps2` card file on B.

        use super::*;
        use emusync_formats::ps2::mcfs::{Node, NodeKind, MODE_DIR_DEFAULT, MODE_FILE_DEFAULT};
        use emusync_formats::ps2::{blank_card, decode, encode, layout, LAYOUT_META};
        use emusync_formats::tree::EntryKind;
        use emusync_formats::{ApplyOp, Canonical, ContainerFormat, Ps2Card, Ps2Folder};
        use std::collections::BTreeMap;

        const PCSX2: &str = "pcsx2-qt.exe";
        const OTHER: &str = "otheremu.exe";
        const GTA: &str = "BASLUS-20062GTA3";
        const RAC: &str = "BASCUS-97199RAC";
        const T: [u8; 8] = [0, 0, 30, 12, 18, 10, 0xEA, 0x07];

        fn save(name: &str, data: &[u8]) -> ApplyOp {
            let file = |n: &[u8], d: &[u8]| Node {
                name: n.to_vec(),
                mode: MODE_FILE_DEFAULT,
                attr: 0,
                created: T,
                modified: T,
                kind: NodeKind::File(d.to_vec()),
            };
            let node = Node {
                name: name.as_bytes().to_vec(),
                mode: MODE_DIR_DEFAULT,
                attr: 0,
                created: T,
                modified: T,
                kind: NodeKind::Dir(vec![file(b"icon.sys", &[0x50; 964]), file(b"data", data)]),
            };
            ApplyOp::Put(Canonical {
                name: name.into(),
                bytes: encode(&node),
                meta: [(LAYOUT_META.to_string(), layout(&node))].into(),
            })
        }

        /// Save name → its `data` file.
        fn saves(fmt: &dyn ContainerFormat, image: &[u8]) -> BTreeMap<String, Vec<u8>> {
            fmt.extract_all(image)
                .unwrap()
                .into_iter()
                .map(|c| {
                    let NodeKind::Dir(files) = decode(&c, T).unwrap().kind else { panic!("{} is a file", c.name) };
                    let data = files.into_iter().find(|f| f.name == b"data").map(|f| match f.kind {
                        NodeKind::File(d) => d,
                        NodeKind::Dir(_) => vec![],
                    });
                    (c.name, data.unwrap_or_default())
                })
                .collect()
        }

        fn folder_saves(card: &Path) -> BTreeMap<String, Vec<u8>> {
            saves(&Ps2Folder, &fsutil::read_tree(card).unwrap().encode())
        }

        fn file_saves(card: &Path) -> BTreeMap<String, Vec<u8>> {
            saves(&Ps2Card, &std::fs::read(card).unwrap())
        }

        /// PCSX2 flushes a save into the open folder card: files written in place, no rename.
        fn pcsx2_writes(card: &Path, op: ApplyOp) {
            let image = Ps2Folder.apply(&fsutil::read_tree(card).unwrap().encode(), &[op]).unwrap();
            for (rel, entry) in emusync_formats::tree::Tree::decode(&image).unwrap().entries {
                let p = rel.split('/').fold(card.to_path_buf(), |p, s| p.join(s));
                match entry.kind {
                    EntryKind::Dir => std::fs::create_dir_all(&p).unwrap(),
                    EntryKind::File(data) => std::fs::write(&p, data).unwrap(),
                }
            }
        }

        fn other_saves(b: &mut Engine, card: &Path, op: ApplyOp) {
            let image = Ps2Card.apply(&std::fs::read(card).unwrap(), &[op]).unwrap();
            std::fs::write(card, image).unwrap();
            assert_eq!(b.sync().unwrap().uploaded_versions, 1);
        }

        fn profile(id: &str, ext: &str, marker: Option<&str>, process: &str, card_dirs: Vec<PathBuf>) -> Profile {
            Profile {
                id: id.into(),
                name: id.into(),
                card_extensions: vec![ext.into()],
                folder_card_marker: marker.map(Into::into),
                processes: vec![process.into()],
                card_dirs,
                lock_files: vec![],
            }
        }

        #[test]
        fn folder_card_changes_sync_and_wait_while_pcsx2_runs() {
            let w = World::new();
            let t0 = Instant::now();

            // A: a new folder card, as PCSX2 creates it (a directory with an empty superblock),
            // named without an extension.
            let memcards = w.dir.path().join("a").join("memcards");
            let card_a = memcards.join("Mcd001");
            std::fs::create_dir_all(&card_a).unwrap();
            std::fs::write(card_a.join("_pcsx2_superblock"), b"").unwrap();
            let profiles = vec![
                profile("pcsx2", "ps2", Some("_pcsx2_superblock"), PCSX2, vec![memcards.clone()]),
                profile("other", "srm", None, OTHER, vec![]),
            ];
            let probe = FakeProbe::default();
            let mut d = Daemon::new(w.engine("a"), profiles, probe.clone(), Timing::default(), true, t0);
            let c = d.tick(t0).expect("startup cycle");
            assert!(!c.failed);
            let containers = d.engine().containers().unwrap();
            assert_eq!(containers.len(), 1, "folder card discovered");
            assert_eq!(containers[0].format, "ps2.card.folder");
            assert_eq!(
                d.watch_dirs(),
                [
                    WatchDir { path: memcards.clone(), recursive: false },
                    WatchDir { path: std::path::absolute(&card_a).unwrap(), recursive: true },
                ]
            );

            // B: the same logical card as a card file.
            let mut b = w.engine("b");
            let dir_b = w.dir.path().join("b").join("memcards");
            std::fs::create_dir_all(&dir_b).unwrap();
            let card_b = dir_b.join("Mcd001.ps2");
            std::fs::write(&card_b, blank_card(8192, true, T)).unwrap();
            b.add_container(ContainerDesc { location: card_b.clone(), ..Default::default() }).unwrap();
            b.sync().unwrap();

            // PCSX2 saves on A, deep inside the folder: the debounced cycle uploads it.
            pcsx2_writes(&card_a, save(GTA, b"gta v1"));
            d.on_event(&card_a.join(GTA).join("data"), t0 + s(5.0));
            d.on_event(&card_a.join("_pcsx2_superblock"), t0 + s(5.1));
            let c = d.tick(t0 + s(7.2)).expect("debounced cycle");
            assert_eq!(c.trigger, Trigger::Changed);
            b.sync().unwrap();
            assert_eq!(file_saves(&card_b), [(GTA.to_string(), b"gta v1".to_vec())].into());

            // emusync's own scratch paths never count as changes, even ones holding a superblock.
            let tmp = fsutil::temp_path(&card_a);
            std::fs::create_dir(&tmp).unwrap();
            std::fs::write(tmp.join("_pcsx2_superblock"), b"").unwrap();
            for p in [tmp.join(GTA).join("data"), tmp.clone(), fsutil::journal_path(&card_a), fsutil::old_path(&card_a)]
            {
                d.on_event(&p, t0 + s(8.0));
            }
            assert!(d.tick(t0 + s(10.5)).is_none());
            std::fs::remove_dir_all(&tmp).unwrap();

            // Another emulator runs: PCSX2's profile claims the folder card, so it isn't busy and
            // B's save is applied at the next poll.
            probe.set(&[OTHER]);
            assert_eq!(d.tick(t0 + s(13.0)).expect("cycle on start").trigger, Trigger::Started);
            other_saves(&mut b, &card_b, save(RAC, b"rac v1"));
            let mut t = t0 + s(13.0) + d.poll_interval();
            let c = d.tick(t).expect("poll");
            assert_eq!((c.applied, c.held_back), (1, 0));
            assert_eq!(folder_saves(&card_a)[RAC], b"rac v1");
            // The apply swapped the folder, so its recursive watch is renewed.
            let rewatch = d.take_rewatch();
            assert_eq!(rewatch.len(), 1);
            assert_eq!(path_key(&rewatch[0]), path_key(&card_a));
            // Neither the swap's events nor the old folder's deletion (which Windows reports
            // under the card's path) are new saves.
            d.on_event(&card_a, t + s(0.1));
            d.on_event(&card_a.join(GTA).join("data"), t + s(0.2));
            assert!(d.tick(t + s(2.5)).is_none());
            // Another program replacing the folder renews the watch too; writes inside don't.
            d.on_moved(&card_a.join(GTA));
            assert!(d.take_rewatch().is_empty());
            d.on_moved(&card_a);
            assert_eq!(d.take_rewatch().len(), 1);

            // PCSX2 starts: the folder card is busy, so B's next save waits in the inbox.
            probe.set(&[OTHER, PCSX2]);
            t += s(5.0);
            assert_eq!(d.tick(t).expect("cycle on start").trigger, Trigger::Started);
            other_saves(&mut b, &card_b, save(RAC, b"rac v2"));
            t += d.poll_interval();
            let c = d.tick(t).expect("poll");
            assert_eq!((c.applied, c.held_back), (0, 1));
            assert_eq!(folder_saves(&card_a)[RAC], b"rac v1");
            assert_eq!(d.engine().inbox().unwrap().len(), 1);
            assert!(d.take_rewatch().is_empty());

            // PCSX2 exits: the waiting save is applied right away.
            probe.set(&[OTHER]);
            let c = d.tick(t + s(2.0)).expect("cycle on exit");
            assert_eq!((c.trigger, c.applied, c.held_back), (Trigger::Exited, 1, 0));
            assert_eq!(
                folder_saves(&card_a),
                [(GTA.to_string(), b"gta v1".to_vec()), (RAC.to_string(), b"rac v2".to_vec())].into()
            );
            assert!(d.engine().inbox().unwrap().is_empty());
        }

        #[test]
        fn new_folder_cards_are_discovered() {
            let w = World::new();
            let t0 = Instant::now();
            let memcards = w.dir.path().join("a").join("memcards");
            std::fs::create_dir_all(&memcards).unwrap();
            std::fs::write(memcards.join("Mcd001.ps2"), blank_card(8192, true, T)).unwrap();
            let profiles = vec![profile("pcsx2", "ps2", Some("_pcsx2_superblock"), PCSX2, vec![memcards.clone()])];
            let mut d = Daemon::new(w.engine("a"), profiles, FakeProbe::default(), Timing::default(), true, t0);
            d.tick(t0).expect("startup cycle");
            assert_eq!(d.engine().containers().unwrap().len(), 1);

            // PCSX2 creates a folder card: the directory first (the only event the card folder's
            // flat watch reports), then its superblock.
            let card = memcards.join("Mcd002");
            std::fs::create_dir(&card).unwrap();
            d.on_event(&card, t0 + s(1.0));
            std::fs::write(card.join("_pcsx2_superblock"), b"").unwrap();
            let c = d.tick(t0 + s(3.1)).expect("new card");
            assert_eq!(c.trigger, Trigger::Changed);
            let formats: Vec<String> = d.engine().containers().unwrap().into_iter().map(|c| c.format).collect();
            assert_eq!(formats.len(), 2);
            assert!(formats.contains(&"ps2.card.folder".to_string()), "{formats:?}");

            // The superblock can arrive after the debounce the directory armed has expired; the
            // folder is still checked at each process probe.
            let late = memcards.join("Mcd003");
            std::fs::create_dir(&late).unwrap();
            d.on_event(&late, t0 + s(4.0));
            assert!(d.tick(t0 + s(6.1)).is_none(), "no marker yet");
            std::fs::write(late.join("_pcsx2_superblock"), b"").unwrap();
            assert!(d.tick(t0 + s(8.2)).is_none(), "marker seen: debounce armed");
            let c = d.tick(t0 + s(10.3)).expect("late card");
            assert_eq!(c.trigger, Trigger::Changed);
            assert_eq!(d.engine().containers().unwrap().len(), 3);
            assert!(d.candidates.is_empty());

            // A folder that never becomes a card is checked for a minute, then left alone.
            let other = memcards.join("screenshots");
            std::fs::create_dir(&other).unwrap();
            d.on_event(&other, t0 + s(11.0));
            assert!(d.tick(t0 + s(13.1)).is_none());
            assert_eq!(d.candidates.len(), 1);
            let mut t = t0 + s(13.1);
            while t < t0 + s(11.0) + CANDIDATE_TTL + s(2.0) {
                t += s(2.0);
                d.tick(t);
            }
            assert!(d.candidates.is_empty());
            assert_eq!(d.engine().containers().unwrap().len(), 3);
        }
    }
}

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

use super::process::ProcessProbe;
use super::profile::Profile;
use emusync_core::{short_id, split_unit, ContainerDesc, ContainerRow, Engine, SyncReport};
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

/// Hash of a card file's content, to tell a real write from our own reads, our own applies or
/// a spurious event. Size and mtime are not enough: cards have a fixed size, and a coarse mtime
/// can miss a second write within the same tick.
type Fingerprint = Option<blake3::Hash>;

fn fingerprint(path: &Path) -> Fingerprint {
    std::fs::read(path).ok().map(|b| blake3::hash(&b))
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
    /// Card files that discovery could not register (warned about once).
    skipped: HashSet<String>,
    /// Units held back by the last cycle (logged once, not on every poll).
    held: HashSet<String>,
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
            skipped: HashSet::new(),
            held: HashSet::new(),
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

    /// Folders to watch: those holding registered cards, plus the profiles' card folders
    /// when discovery is on.
    pub fn watch_dirs(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = self
            .engine
            .containers()
            .unwrap_or_default()
            .iter()
            .filter_map(|c| Path::new(&c.location).parent().map(Path::to_path_buf))
            .collect();
        if self.discover {
            dirs.extend(self.profiles.iter().flat_map(|p| p.card_dirs.iter().cloned()));
        }
        dirs.retain(|d| d.is_dir());
        dirs.sort_by_key(|d| path_key(d));
        dirs.dedup_by_key(|d| path_key(d));
        dirs
    }

    /// A file in a watched folder changed. Only card files arm the debounce.
    pub fn on_event(&mut self, path: &Path, now: Instant) {
        let key = path_key(path);
        let is_card = self.profiles.iter().any(|p| p.claims(path)) || self.fingerprints.contains_key(&key);
        if is_card {
            log::debug!("change: {}", path.display());
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
        }
        if let Some(t) = self.last_change {
            if now.duration_since(t) >= self.timing.debounce {
                self.last_change = None;
                if self.local_changed() {
                    self.want(Trigger::Changed);
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

    fn local_changed(&self) -> bool {
        let containers = self.engine.containers().unwrap_or_default();
        containers.iter().any(|c| {
            let p = Path::new(&c.location);
            self.fingerprints.get(&path_key(p)) != Some(&fingerprint(p))
        }) || !self.undiscovered().is_empty()
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
        for c in &containers {
            let p = Path::new(&c.location);
            let in_use = self.in_use(p);
            self.engine.set_busy(c.id, in_use);
            if in_use {
                busy.insert(c.card_id.clone());
            }
            before.insert(path_key(p), fingerprint(p));
        }
        let report = self.engine.sync()?;
        // Fingerprints from before the cycle, so a write that lands during it still counts as
        // a change; cards this cycle wrote are re-read.
        for a in &report.applied {
            let p = Path::new(&a.location);
            before.insert(path_key(p), fingerprint(p));
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
        for b in &r.blocked {
            let card = split_unit(&b.unit).map_or(b.unit.as_str(), |(c, _)| c);
            if b.reason == IN_USE && busy.contains(card) {
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
}

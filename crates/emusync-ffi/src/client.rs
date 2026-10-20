//! The client behind `es_client*` (DESIGN §3.2): one worker thread owns the synchronous
//! [`Engine`]; API calls queue commands for it, and only the documented blocking calls wait for
//! an answer, each with a deadline.
//!
//! The worker never writes to a container that a session has open. `es_session_begin` marks
//! its containers busy on the [`BusyGate`] before it returns, whether or not the worker got to
//! the pull in time, and the engine holds the gate across every container write, so a pull
//! that finishes after the deadline sends its changes to the inbox instead.

use crate::abi::{self, *};
use crate::callbacks::{route_logs, Callbacks};
use crate::events::{Event, EventQueue};
use crate::status_of;
use emusync_core::{
    BlockKind, BusyGate, CardMode, ConflictInfo, ContainerDesc, ContainerRow, Engine, EngineOptions, HistoryEntry,
    KdfParams, RemoteSetup, Resolution, SyncReport,
};
use emusync_remote::Remote;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, MutexGuard, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// `es_container_touched` arms this debounce before a quiescent snapshot (DESIGN §6.4).
pub(crate) const DEBOUNCE: Duration = Duration::from_secs(2);
/// How long calls that need an answer from the worker (adding a container, history, ...)
/// wait for it to become free.
pub(crate) const WORKER_WAIT: Duration = Duration::from_secs(10);

/// A passphrase, wiped from memory when dropped.
pub(crate) type Passphrase = zeroize::Zeroizing<String>;

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ----------------------------------------------------------------------------------------------
// Replies
// ----------------------------------------------------------------------------------------------

enum Slot<T> {
    Queued,
    Running,
    Done(T),
    /// The caller stopped waiting before the work started; the worker skips it.
    Cancelled,
    /// The worker dropped the command without answering (it panicked).
    Lost,
    Taken,
}

/// The answer to one command, which the caller may stop waiting for.
pub(crate) struct Pending<T> {
    slot: Mutex<Slot<T>>,
    cv: Condvar,
}

pub(crate) enum Waited<T> {
    Done(T),
    TimedOut,
    Lost,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaitMode {
    /// At the deadline, cancel the command if it hasn't started; if it has, wait for it (for
    /// local work, which is quick once it runs). ES_TIMEOUT then means "had no effect".
    CancelIfQueued,
    /// At the deadline, stop waiting; the command still runs.
    Deadline,
}

impl<T> Pending<T> {
    pub fn new() -> (Arc<Self>, Reply<T>) {
        let p = Arc::new(Self { slot: Mutex::new(Slot::Queued), cv: Condvar::new() });
        (p.clone(), Reply(p))
    }

    pub fn wait(&self, timeout: Duration, mode: WaitMode) -> Waited<T> {
        let deadline = Instant::now() + timeout;
        let mut slot = lock(&self.slot);
        loop {
            match &*slot {
                Slot::Done(_) => match std::mem::replace(&mut *slot, Slot::Taken) {
                    Slot::Done(v) => return Waited::Done(v),
                    _ => unreachable!(),
                },
                Slot::Lost => return Waited::Lost,
                Slot::Cancelled | Slot::Taken => return Waited::TimedOut,
                Slot::Running if mode == WaitMode::CancelIfQueued => {
                    slot = self.cv.wait(slot).unwrap_or_else(|e| e.into_inner());
                }
                Slot::Queued | Slot::Running => {
                    let now = Instant::now();
                    if now >= deadline {
                        if mode == WaitMode::CancelIfQueued {
                            *slot = Slot::Cancelled;
                        }
                        return Waited::TimedOut;
                    }
                    slot = self.cv.wait_timeout(slot, deadline - now).unwrap_or_else(|e| e.into_inner()).0;
                }
            }
        }
    }
}

/// The worker's side of a [`Pending`]. Dropping it unanswered reports `Lost`.
pub(crate) struct Reply<T>(Arc<Pending<T>>);

impl<T> Reply<T> {
    /// False if the caller cancelled the command.
    fn start(&self) -> bool {
        let mut slot = lock(&self.0.slot);
        match *slot {
            Slot::Queued => {
                *slot = Slot::Running;
                true
            }
            _ => false,
        }
    }

    fn finish(self, v: T) {
        let mut slot = lock(&self.0.slot);
        if matches!(*slot, Slot::Queued | Slot::Running) {
            *slot = Slot::Done(v);
        }
        self.0.cv.notify_all();
    }
}

impl<T> Drop for Reply<T> {
    fn drop(&mut self) {
        let mut slot = lock(&self.0.slot);
        if matches!(*slot, Slot::Queued | Slot::Running) {
            *slot = Slot::Lost;
        }
        self.0.cv.notify_all();
    }
}

// ----------------------------------------------------------------------------------------------
// Commands
// ----------------------------------------------------------------------------------------------

/// What `es_session_begin` learns from the worker.
pub(crate) struct BeginOutcome {
    pub applied: u32,
    pub blocked: u32,
    pub remote_reached: bool,
}

pub(crate) enum Cmd {
    AddContainer(ContainerDesc, Reply<Result<i64, i32>>),
    RemoveContainer(i64, Reply<Result<(), i32>>),
    Touched(i64),
    Flushed(i64, Reply<i32>),
    SetMode(String, CardMode),
    /// A remote that `es_remote_add` set up (`Some`), or could not reach (`None`: attached, set
    /// up on the next network cycle, with the `es_repo_create_encrypted` passphrase if any).
    BindRemote(Arc<dyn Remote>, Option<RemoteSetup>, Option<Passphrase>),
    /// `es_repo_unlock`; replies with a status.
    Unlock(Passphrase, Reply<i32>),
    SessionBegin {
        mounted: Vec<i64>,
        reply: Reply<BeginOutcome>,
    },
    SessionEnd {
        mounted: Vec<i64>,
    },
    Suspend(Reply<i32>),
    Sync,
    Resolve(i64, Resolution),
    History(String, Reply<Result<Vec<HistoryEntry>, i32>>),
    Restore(String, String, Reply<Result<(), i32>>),
    Shutdown,
}

#[derive(Default)]
pub(crate) struct Queue {
    q: Mutex<VecDeque<Cmd>>,
    cv: Condvar,
}

impl Queue {
    pub fn push(&self, cmd: Cmd) {
        lock(&self.q).push_back(cmd);
        self.cv.notify_one();
    }

    /// Ahead of everything else queued (but after the command that is running).
    pub fn push_urgent(&self, cmd: Cmd) {
        lock(&self.q).push_front(cmd);
        self.cv.notify_one();
    }

    /// The next command, or `None` once `deadline` passes.
    fn pop(&self, deadline: Option<Instant>) -> Option<Cmd> {
        let mut q = lock(&self.q);
        loop {
            if let Some(cmd) = q.pop_front() {
                return Some(cmd);
            }
            match deadline {
                None => q = self.cv.wait(q).unwrap_or_else(|e| e.into_inner()),
                Some(d) => {
                    let now = Instant::now();
                    if now >= d {
                        return None;
                    }
                    q = self.cv.wait_timeout(q, d - now).unwrap_or_else(|e| e.into_inner()).0;
                }
            }
        }
    }
}

// ----------------------------------------------------------------------------------------------
// State shared between API threads and the worker
// ----------------------------------------------------------------------------------------------

/// What API calls can answer without waiting for the worker. Refreshed after every command.
#[derive(Default, Clone)]
pub(crate) struct Cache {
    pub containers: Vec<ContainerRow>,
    pub modes: Vec<(String, CardMode)>,
    pub conflicts: Vec<ConflictInfo>,
    pub remote: Option<String>,
}

pub(crate) struct Session {
    pub mounted: Vec<i64>,
}

#[derive(Default)]
pub(crate) struct Sessions {
    /// Open sessions, from the moment `es_session_begin` registers them.
    pub open: HashMap<u64, Session>,
    /// Containers an `es_container_remove` is removing; no session may mount them meanwhile.
    pub removing: HashSet<i64>,
}

pub(crate) struct Shared {
    pub queue: Queue,
    pub events: EventQueue,
    pub cb: Arc<Callbacks>,
    gate: BusyGate,
    /// Sessions holding each container; it is busy while the count is above zero.
    holds: Mutex<HashMap<i64, usize>>,
    pub sessions: Mutex<Sessions>,
    /// The remote `es_remote_add` accepted but couldn't set up yet (unreachable): the data
    /// directory is bound to it only once it is, but no other remote may be added meanwhile.
    pub pending_remote: Mutex<Option<String>>,
    /// From `es_repo_create_encrypted`, for the next `es_remote_add`.
    pub create_passphrase: Mutex<Option<Passphrase>>,
    /// The data directory (canonical), where an encrypted repository's key is kept.
    pub data_dir: PathBuf,
    pub next_session: AtomicU64,
    pub cache: RwLock<Cache>,
    /// An `es_sync_now` is queued and not started yet (later ones are merged into it).
    pub sync_queued: AtomicBool,
}

impl Shared {
    pub fn emit(&self, ev: Event) {
        self.events.push(ev, &self.cb);
    }

    pub fn cache(&self) -> std::sync::RwLockReadGuard<'_, Cache> {
        self.cache.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Marks containers busy for a session. Returns once no write to them is in progress.
    pub fn hold(&self, ids: &[i64]) {
        let mut holds = lock(&self.holds);
        for &id in ids {
            *holds.entry(id).or_default() += 1;
            self.gate.set(id, true);
        }
    }

    pub fn release(&self, ids: &[i64]) {
        let mut holds = lock(&self.holds);
        for id in ids {
            if let Some(n) = holds.get_mut(id) {
                *n -= 1;
                if *n == 0 {
                    holds.remove(id);
                    self.gate.set(*id, false);
                }
            }
        }
    }

    pub fn is_held(&self, id: i64) -> bool {
        lock(&self.holds).contains_key(&id)
    }
}

// ----------------------------------------------------------------------------------------------
// Client
// ----------------------------------------------------------------------------------------------

/// Data directories with a live client in this process: two engines on one directory would
/// fight over its state.
static OPEN_DIRS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

pub struct Client {
    pub(crate) shared: Arc<Shared>,
    worker: Mutex<Option<(JoinHandle<()>, mpsc::Receiver<()>)>>,
}

impl Client {
    pub(crate) fn create(data_dir: PathBuf, opts: EngineOptions, cb: Arc<Callbacks>) -> Result<Box<Client>, i32> {
        std::fs::create_dir_all(&data_dir).map_err(|e| {
            log::error!("cannot create {}: {e}", data_dir.display());
            ES_E_IO
        })?;
        let dir = std::fs::canonicalize(&data_dir).unwrap_or(data_dir);
        {
            let mut open = lock(&OPEN_DIRS);
            if open.contains(&dir) {
                log::error!("{} is already open in another emusync client", dir.display());
                return Err(ES_E_BUSY);
            }
            open.push(dir.clone());
        }
        let unregister = |dir: &PathBuf| lock(&OPEN_DIRS).retain(|d| d != dir);
        let engine = match Engine::open(&dir, opts) {
            Ok(e) => e,
            Err(e) => {
                log::error!("cannot open {}: {e}", dir.display());
                unregister(&dir);
                return Err(status_of(&e));
            }
        };
        let shared = Arc::new(Shared {
            queue: Queue::default(),
            events: EventQueue::default(),
            cb,
            gate: engine.busy_gate(),
            holds: Mutex::new(HashMap::new()),
            sessions: Mutex::new(Sessions::default()),
            pending_remote: Mutex::new(None),
            create_passphrase: Mutex::new(None),
            data_dir: dir.clone(),
            next_session: AtomicU64::new(1),
            cache: RwLock::new(Cache::default()),
            sync_queued: AtomicBool::new(false),
        });
        let mut worker = Worker {
            engine,
            shared: shared.clone(),
            dirty: HashMap::new(),
            push_due: None,
            unsetup_remote: None,
            announced: HashSet::new(),
            blocked: HashMap::new(),
        };
        worker.refresh();
        let (done_tx, done_rx) = mpsc::channel();
        let worker_dir = dir.clone();
        let handle = std::thread::Builder::new()
            .name("emusync-worker".into())
            .spawn(move || {
                worker.run();
                drop(worker);
                unregister(&worker_dir);
                let _ = done_tx.send(());
            })
            .map_err(|e| {
                log::error!("cannot start the worker thread: {e}");
                unregister(&dir);
                ES_E_INTERNAL
            })?;
        Ok(Box::new(Client { shared, worker: Mutex::new(Some((handle, done_rx))) }))
    }

    /// Final snapshot and best-effort push, waiting up to `timeout`; after that the worker
    /// finishes on its own, but no host callback runs once this returns.
    pub(crate) fn destroy(self, timeout: Duration) {
        self.shared.queue.push(Cmd::Shutdown);
        if let Some((handle, done)) = lock(&self.worker).take() {
            match done.recv_timeout(timeout) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = handle.join();
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    log::warn!("the worker did not finish within {timeout:?}; it finishes in the background");
                }
            }
        }
        self.shared.cb.shut_down();
    }
}

// ----------------------------------------------------------------------------------------------
// Worker
// ----------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Cycle {
    Full,
    PullApply,
    Push,
}

struct Worker {
    engine: Engine,
    shared: Arc<Shared>,
    /// Touched containers and when their debounce expires.
    dirty: HashMap<i64, Instant>,
    /// When to push the outbox (after a snapshot found new versions).
    push_due: Option<Instant>,
    /// Attached while unreachable at `es_remote_add`; set up on the next network cycle (with
    /// the passphrase for a new encrypted repository, if one was given).
    unsetup_remote: Option<(Arc<dyn Remote>, Option<Passphrase>)>,
    /// Conflicts already reported with ES_EV_CONFLICT.
    announced: HashSet<i64>,
    /// Blocked units already reported, per container, so a unit that stays blocked (a card
    /// that stays full) is reported once rather than every cycle.
    blocked: HashMap<i64, HashSet<(String, BlockKind)>>,
}

impl Worker {
    fn run(&mut self) {
        let _route = route_logs(&self.shared.cb);
        loop {
            let deadline = self.dirty.values().chain(self.push_due.iter()).min().copied();
            let cmd = self.shared.queue.pop(deadline);
            let shutdown = matches!(cmd, Some(Cmd::Shutdown));
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                match cmd {
                    Some(cmd) => self.handle(cmd),
                    None => self.timers(),
                }
                self.refresh();
            }));
            if result.is_err() {
                log::error!("internal error in the emusync worker (see stderr)");
                self.shared
                    .emit(Event::new(ES_EV_ERROR, ES_E_INTERNAL).message("internal error in the emusync worker"));
            }
            if shutdown {
                return;
            }
        }
    }

    /// Updates the cache API calls read.
    fn refresh(&mut self) {
        let e = &self.engine;
        let cache = (|| -> emusync_core::Result<Cache> {
            Ok(Cache {
                containers: e.containers()?,
                modes: e.card_modes()?,
                conflicts: e.conflicts()?,
                remote: e.remote_uri()?,
            })
        })();
        match cache {
            Ok(c) => *self.shared.cache.write().unwrap_or_else(|e| e.into_inner()) = c,
            Err(e) => log::warn!("cannot read the local state: {e}"),
        }
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::AddContainer(desc, reply) => {
                if reply.start() {
                    let r = self.engine.add_container(desc).map_err(|e| self.local_error("adding a container", &e));
                    self.refresh();
                    reply.finish(r);
                }
            }
            Cmd::RemoveContainer(id, reply) => {
                if reply.start() {
                    self.dirty.remove(&id);
                    self.blocked.remove(&id);
                    let r = self.engine.remove_container(id).map_err(|e| self.local_error("removing a container", &e));
                    self.refresh();
                    reply.finish(r);
                }
            }
            Cmd::Touched(id) => {
                self.dirty.insert(id, Instant::now() + DEBOUNCE);
            }
            Cmd::Flushed(id, reply) => {
                reply.start();
                self.dirty.remove(&id);
                let status = self.snapshot(Some(&[id]));
                reply.finish(status);
            }
            Cmd::SetMode(card, mode) => match self.engine.set_card_mode(&card, mode) {
                Ok(()) => self.push_due = Some(Instant::now()),
                Err(e) => {
                    let status = self.local_error("changing the card mode", &e);
                    self.shared.emit(Event::new(ES_EV_ERROR, status).message(format!("{card}: {e}")));
                }
            },
            Cmd::BindRemote(remote, setup, passphrase) => match setup {
                Some(setup) => {
                    if let Err(e) = self.engine.bind_remote(remote, setup) {
                        let status = self.local_error("adding the remote", &e);
                        self.shared.emit(Event::new(ES_EV_ERROR, status).message(e.to_string()));
                    }
                }
                None => {
                    self.engine.attach_remote(remote.clone());
                    self.unsetup_remote = Some((remote, passphrase));
                }
            },
            Cmd::Unlock(passphrase, reply) => {
                if reply.start() {
                    let status = self.unlock(&passphrase);
                    self.refresh();
                    reply.finish(status);
                }
            }
            Cmd::SessionBegin { mounted, reply } => {
                reply.start();
                let (report, _) = self.cycle(Cycle::PullApply);
                let outcome = match report {
                    Some(r) => {
                        let applied = r.applied.iter().filter(|a| mounted.contains(&a.container)).count();
                        let blocked: HashSet<&str> = r
                            .blocked
                            .iter()
                            .filter(|b| mounted.contains(&b.container))
                            .map(|b| b.unit.as_str())
                            .collect();
                        BeginOutcome {
                            applied: applied as u32,
                            blocked: blocked.len() as u32,
                            remote_reached: !r.offline,
                        }
                    }
                    None => BeginOutcome { applied: 0, blocked: 0, remote_reached: false },
                };
                self.refresh();
                reply.finish(outcome);
            }
            Cmd::SessionEnd { mounted } => {
                for id in &mounted {
                    self.dirty.remove(id);
                }
                self.snapshot(Some(&mounted));
                self.shared.release(&mounted);
                self.cycle(Cycle::Full);
            }
            Cmd::Suspend(reply) => {
                reply.start();
                let status = self.snapshot(None);
                let status = if status < 0 { status } else { self.cycle(Cycle::Push).1 };
                reply.finish(status);
            }
            Cmd::Sync => {
                self.shared.sync_queued.store(false, Ordering::SeqCst);
                self.cycle(Cycle::Full);
            }
            Cmd::Resolve(id, resolution) => match self.engine.resolve(id, resolution) {
                Ok(_) => {
                    self.cycle(Cycle::Full);
                }
                Err(e) => {
                    let status = self.local_error("resolving a conflict", &e);
                    self.shared.emit(
                        Event { conflict_id: id as u64, ..Event::new(ES_EV_ERROR, status) }.message(e.to_string()),
                    );
                }
            },
            Cmd::History(unit, reply) => {
                if reply.start() {
                    let r = self.engine.history(&unit).map_err(|e| self.local_error("reading history", &e));
                    reply.finish(r);
                }
            }
            Cmd::Restore(unit, version, reply) => {
                if reply.start() {
                    let r = self.engine.restore(&unit, &version);
                    let ok = r.is_ok();
                    let r = r.map(|_| ()).map_err(|e| self.local_error("restoring a version", &e));
                    self.refresh();
                    reply.finish(r);
                    if ok {
                        self.cycle(Cycle::Full);
                    }
                }
            }
            Cmd::Shutdown => {
                self.snapshot(None);
                if self.engine.pending_uploads().unwrap_or(0) > 0 {
                    self.cycle(Cycle::Push);
                }
            }
        }
    }

    fn timers(&mut self) {
        let now = Instant::now();
        let due: Vec<i64> = self.dirty.iter().filter(|(_, t)| **t <= now).map(|(id, _)| *id).collect();
        if !due.is_empty() {
            for id in &due {
                self.dirty.remove(id);
            }
            self.snapshot(Some(&due));
        }
        if self.push_due.is_some_and(|t| t <= now) {
            self.push_due = None;
            self.cycle(Cycle::Push);
        }
    }

    fn local_error(&self, what: &str, e: &emusync_core::Error) -> i32 {
        log::warn!("{what}: {e}");
        status_of(e)
    }

    /// Quiescent snapshot of some (or all) containers; schedules a push if it found changes.
    fn snapshot(&mut self, ids: Option<&[i64]>) -> i32 {
        match self.engine.snapshot_containers(ids) {
            Ok(r) => {
                if r.new_local_versions > 0 {
                    self.push_due = Some(Instant::now());
                }
                self.report_blocked(&r, ids);
                ES_OK
            }
            Err(e) => {
                let status = self.local_error("snapshot", &e);
                self.shared.emit(Event::new(ES_EV_ERROR, status).message(e.to_string()));
                status
            }
        }
    }

    /// Sets up a remote `es_remote_add` couldn't reach. `Err` with the status if it still can't
    /// be (`ES_OFFLINE`) or was rejected (reported, and dropped).
    fn setup_pending_remote(&mut self) -> Result<(), i32> {
        let Some((remote, passphrase)) = self.unsetup_remote.take() else { return Ok(()) };
        let r = match &passphrase {
            Some(p) => self.engine.add_remote_encrypted(remote.clone(), p, KdfParams::default()),
            None => self.engine.add_remote(remote.clone()),
        };
        match r {
            Ok(_) => Ok(()),
            Err(e) if e.is_offline() => {
                self.unsetup_remote = Some((remote, passphrase));
                Err(ES_OFFLINE)
            }
            Err(e) => Err(self.report_error(&e)),
        }
    }

    /// `es_repo_unlock`.
    fn unlock(&mut self, passphrase: &str) -> i32 {
        if let Err(status) = self.setup_pending_remote() {
            return status;
        }
        match self.engine.unlock(passphrase) {
            Ok(()) => {
                log::info!("the repository is unlocked; its key is kept in the data directory");
                ES_OK
            }
            Err(e) => self.local_error("unlocking the repository", &e),
        }
    }

    /// A cycle that may use the network, framed by SYNC_STARTED and SYNC_FINISHED.
    fn cycle(&mut self, kind: Cycle) -> (Option<SyncReport>, i32) {
        self.shared.emit(Event::new(ES_EV_SYNC_STARTED, ES_OK));
        let _ = self.setup_pending_remote();
        let r = match kind {
            Cycle::Full => self.engine.sync(),
            Cycle::PullApply => self.engine.pull_and_apply(),
            Cycle::Push => self.engine.push_pending(),
        };
        match r {
            Ok(rep) => {
                if kind != Cycle::PullApply && !rep.offline {
                    self.push_due = None;
                }
                self.refresh();
                self.report(&rep, kind != Cycle::Push);
                let status = if rep.offline { ES_OFFLINE } else { ES_OK };
                self.shared.emit(Event::new(ES_EV_SYNC_FINISHED, status));
                (Some(rep), status)
            }
            Err(e) => {
                let status = self.report_error(&e);
                self.shared.emit(Event::new(ES_EV_SYNC_FINISHED, status).message(e.to_string()));
                (None, status)
            }
        }
    }

    fn report_error(&self, e: &emusync_core::Error) -> i32 {
        let status = status_of(e);
        log::warn!("sync: {e}");
        if status == ES_E_AUTH {
            let uri = self.engine.remote_uri().ok().flatten().unwrap_or_default();
            self.shared.emit(Event::new(ES_EV_AUTH_REQUIRED, status).message(uri));
        } else {
            self.shared.emit(Event::new(ES_EV_ERROR, status).message(e.to_string()));
        }
        status
    }

    fn title_of(&self, version: &str) -> Option<String> {
        self.engine.version(version).ok().flatten().and_then(|v| v.titles.into_iter().next())
    }

    /// Events for one cycle's report. `all` = the cycle covered every container.
    fn report(&mut self, r: &SyncReport, all: bool) {
        for (unit, version) in &r.uploaded {
            let ev = Event { title_key: self.title_of(version), ..Event::new(ES_EV_UNIT_UPLOADED, ES_OK) };
            self.shared.emit(ev.unit(unit));
        }
        for a in &r.applied {
            let ev = Event {
                container: a.container as u64,
                title_key: self.title_of(&a.version),
                ..Event::new(ES_EV_UNIT_APPLIED, ES_OK)
            };
            self.shared.emit(ev.unit(&a.unit));
        }
        if all {
            self.report_blocked(r, None);
        }
        for card in &r.mode_changes {
            self.shared.emit(Event::new(ES_EV_CARD_MODE_CHANGED, ES_OK).message(card.clone()));
        }
        let open: Vec<ConflictInfo> = self.shared.cache().conflicts.clone();
        self.announced.retain(|id| open.iter().any(|c| c.id == *id));
        for c in open {
            if self.announced.insert(c.id) {
                let title = c.tips.iter().find_map(|t| t.version.titles.first().cloned());
                let container = self
                    .shared
                    .cache()
                    .containers
                    .iter()
                    .find(|x| Some(&x.location) == c.location.as_ref())
                    .map(|x| x.id);
                let ev = Event {
                    conflict_id: c.id as u64,
                    container: container.unwrap_or_default() as u64,
                    title_key: title,
                    ..Event::new(ES_EV_CONFLICT, ES_CONFLICT)
                };
                self.shared.emit(ev.unit(&c.unit));
            }
        }
        for w in &r.warnings {
            log::warn!("{w}");
        }
        if let Some((old, new)) = &r.reminted {
            log::warn!("device id {old} is in use by another installation; this device is now {new}");
        }
    }

    /// APPLY_BLOCKED for units newly blocked in the containers this cycle covered.
    fn report_blocked(&mut self, r: &SyncReport, ids: Option<&[i64]>) {
        let covered: Vec<i64> = match ids {
            Some(ids) => ids.to_vec(),
            None => self.shared.cache().containers.iter().map(|c| c.id).collect(),
        };
        let mut now: HashMap<i64, HashSet<(String, BlockKind)>> = HashMap::new();
        for b in &r.blocked {
            now.entry(b.container).or_default().insert((b.unit.clone(), b.kind));
        }
        for id in covered {
            let current = now.remove(&id).unwrap_or_default();
            let before = self.blocked.remove(&id).unwrap_or_default();
            for b in r.blocked.iter().filter(|b| b.container == id) {
                if !before.contains(&(b.unit.clone(), b.kind)) {
                    let ev = Event { container: id as u64, ..Event::new(ES_EV_APPLY_BLOCKED, block_status(b.kind)) };
                    self.shared.emit(ev.unit(&b.unit).message(b.reason.clone()));
                }
            }
            if !current.is_empty() {
                self.blocked.insert(id, current);
            }
        }
    }
}

fn block_status(k: BlockKind) -> i32 {
    match k {
        BlockKind::NoSpace => ES_E_NO_SPACE,
        BlockKind::Busy | BlockKind::Changed => ES_E_BUSY,
        BlockKind::Missing => ES_E_NOT_FOUND,
        BlockKind::Invalid => ES_E_FORMAT,
        BlockKind::Unavailable => ES_OFFLINE,
        BlockKind::Io => ES_E_IO,
        BlockKind::Unsupported => abi::ES_E_INCOMPATIBLE,
    }
}

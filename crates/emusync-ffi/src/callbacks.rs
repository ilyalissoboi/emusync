//! Calls into the host: the log callback and the event `wake` notifier (the credential
//! provider is in `creds.rs`), and the guard that stops them once `es_client_destroy` returns.
//!
//! Engine code logs through the `log` crate, which has one global logger per process. The
//! logger installed here routes each record to the client whose thread produced it: library
//! threads, and host threads inside an API call, register their client in a thread-local.

use crate::abi::{es_log_fn, es_wake_fn};
use std::cell::RefCell;
use std::ffi::{c_void, CString};
use std::sync::{Arc, Mutex, Once, RwLock};

#[derive(Clone, Copy)]
pub(crate) struct HostPtr(pub *mut c_void);
// SAFETY: the pointer is the host's `user` value, only passed back to the host's callbacks,
// which the header requires to be callable from any library thread.
unsafe impl Send for HostPtr {}
unsafe impl Sync for HostPtr {}

pub(crate) struct Callbacks {
    /// False once the client is destroyed. Callers hold the read lock while calling into the
    /// host, so `shut_down` returns only when no callback is running.
    alive: RwLock<bool>,
    log: es_log_fn,
    log_user: HostPtr,
    log_level: log::LevelFilter,
    wake: Mutex<(es_wake_fn, HostPtr)>,
}

impl Callbacks {
    pub fn new(log: es_log_fn, log_user: *mut c_void, log_level: log::LevelFilter) -> Arc<Self> {
        let level = if log.is_some() { log_level } else { log::LevelFilter::Off };
        install_logger(level);
        Arc::new(Self {
            alive: RwLock::new(true),
            log,
            log_user: HostPtr(log_user),
            log_level: level,
            wake: Mutex::new((None, HostPtr(std::ptr::null_mut()))),
        })
    }

    /// Runs `f` unless the client was destroyed. `f` must not log: the logger takes the same
    /// lock, and a recursive read can deadlock behind a waiting `shut_down`.
    pub fn guarded<T>(&self, f: impl FnOnce() -> T) -> Option<T> {
        let alive = self.alive.read().unwrap_or_else(|e| e.into_inner());
        alive.then(f)
    }

    /// Stops all further calls into the host. Waits for one in progress.
    pub fn shut_down(&self) {
        *self.alive.write().unwrap_or_else(|e| e.into_inner()) = false;
    }

    pub fn set_wake(&self, wake: es_wake_fn, user: *mut c_void) {
        *self.wake.lock().unwrap_or_else(|e| e.into_inner()) = (wake, HostPtr(user));
    }

    pub fn wake(&self) {
        let (wake, user) = *self.wake.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(wake) = wake {
            // SAFETY: a host callback, called with the host's own user pointer.
            self.guarded(|| unsafe { wake(user.0) });
        }
    }

    fn log(&self, record: &log::Record) {
        let Some(f) = self.log else { return };
        if record.level() > self.log_level {
            return;
        }
        let level = match record.level() {
            log::Level::Error => 0,
            log::Level::Warn => 1,
            log::Level::Info => 2,
            log::Level::Debug => 3,
            log::Level::Trace => 4,
        };
        let target = cstring_lossy(record.target());
        let msg = cstring_lossy(&record.args().to_string());
        // SAFETY: a host callback; both strings live until it returns.
        self.guarded(|| unsafe { f(self.log_user.0, level, target.as_ptr(), msg.as_ptr()) });
    }
}

/// A C string with interior NULs replaced, so it never fails.
pub(crate) fn cstring_lossy(s: &str) -> CString {
    CString::new(s.replace('\0', "\u{FFFD}")).unwrap_or_default()
}

thread_local! {
    static CURRENT: RefCell<Option<Arc<Callbacks>>> = const { RefCell::new(None) };
}

/// Routes this thread's log records to `cb` until the guard is dropped.
pub(crate) fn route_logs(cb: &Arc<Callbacks>) -> RouteGuard {
    let prev = CURRENT.with(|c| c.replace(Some(cb.clone())));
    RouteGuard(prev)
}

pub(crate) struct RouteGuard(Option<Arc<Callbacks>>);

impl Drop for RouteGuard {
    fn drop(&mut self) {
        let prev = self.0.take();
        CURRENT.with(|c| *c.borrow_mut() = prev);
    }
}

struct Router;

impl log::Log for Router {
    fn enabled(&self, m: &log::Metadata) -> bool {
        CURRENT.with(|c| c.borrow().as_ref().is_some_and(|cb| m.level() <= cb.log_level))
    }

    fn log(&self, record: &log::Record) {
        // Clone out of the RefCell first: the host callback may run for a while.
        if let Some(cb) = CURRENT.with(|c| c.borrow().clone()) {
            cb.log(record);
        }
    }

    fn flush(&self) {}
}

static ROUTER: Router = Router;

/// Installs the routing logger once per process (unless the host, being a Rust program,
/// installed its own) and raises the global level to `level` if it is lower.
fn install_logger(level: log::LevelFilter) {
    static INSTALL: Once = Once::new();
    static OURS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    INSTALL.call_once(|| {
        let ok = log::set_logger(&ROUTER).is_ok();
        OURS.store(ok, std::sync::atomic::Ordering::SeqCst);
    });
    if OURS.load(std::sync::atomic::Ordering::SeqCst) && level > log::max_level() {
        log::set_max_level(level);
    }
}

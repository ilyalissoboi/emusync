//! The exported functions. Their contracts are documented in `include/emusync.h`; the safety
//! requirements are the usual C ones (valid pointers, NUL-terminated strings, a live client).

#![allow(clippy::missing_safety_doc)]

use crate::abi::*;
use crate::callbacks::{route_logs, Callbacks};
use crate::client::{lock, Client, Cmd, Passphrase, Pending, Session, Shared, WaitMode, Waited, WORKER_WAIT};
use crate::status_of;
use emusync_core::{
    CardMode, ConflictPolicy, ContainerDesc, Engine, EngineOptions, HistoryEntry, KdfParams, Resolution,
};
use emusync_remote::{Remote, S3Config, S3Remote};
use std::ffi::{c_char, c_int, c_void, CStr};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc};
use std::time::Duration;

/// `es_remote_add` waits this long for the remote to answer.
const REMOTE_ADD_WAIT: Duration = Duration::from_secs(10);
/// `es_container_flushed` waits this long for the snapshot.
const FLUSH_WAIT: Duration = Duration::from_secs(2);
/// `es_session_begin` with `timeout_ms` = 0.
const DEFAULT_SESSION_TIMEOUT_MS: u32 = 3000;

type Res = Result<i32, i32>;

/// Runs an API call: no panic escapes, and log records go to the client's callback.
unsafe fn with_client(c: *mut Client, f: impl FnOnce(&Client) -> Res) -> i32 {
    if c.is_null() {
        return ES_E_INVALID_ARG;
    }
    let client = &*c;
    let _route = route_logs(&client.shared.cb);
    match catch_unwind(AssertUnwindSafe(|| f(client))) {
        Ok(Ok(s) | Err(s)) => s,
        Err(_) => {
            log::error!("internal error (see stderr)");
            ES_E_INTERNAL
        }
    }
}

fn invalid(msg: impl std::fmt::Display) -> i32 {
    log::warn!("invalid argument: {msg}");
    ES_E_INVALID_ARG
}

unsafe fn opt_str(p: *const c_char, what: &str) -> Result<Option<String>, i32> {
    if p.is_null() {
        return Ok(None);
    }
    match CStr::from_ptr(p).to_str() {
        Ok(s) => Ok(Some(s.to_string())),
        Err(_) => Err(invalid(format!("{what} is not valid UTF-8"))),
    }
}

unsafe fn req_str(p: *const c_char, what: &str) -> Result<String, i32> {
    opt_str(p, what)?.ok_or_else(|| invalid(format!("{what} is NULL")))
}

/// Checks a struct pointer and its `struct_size`.
unsafe fn check_struct<'a, T>(p: *const T, size: impl FnOnce(&T) -> u32, what: &str) -> Result<&'a T, i32> {
    if p.is_null() {
        return Err(invalid(format!("{what} is NULL")));
    }
    let r = &*p;
    let size = size(r);
    if !size_ok::<T>(size) {
        log::error!("{what}: struct_size {size} is smaller than this library's {}", std::mem::size_of::<T>());
        return Err(ES_E_ABI);
    }
    Ok(r)
}

/// Zeroes an output struct, keeping the host's `struct_size`.
unsafe fn clear_out<T>(p: *mut T) {
    let size = *(p as *const u32);
    std::ptr::write_bytes(p as *mut u8, 0, std::mem::size_of::<T>());
    *(p as *mut u32) = size;
}

fn waited<T>(w: Waited<Result<T, i32>>) -> Result<T, i32> {
    match w {
        Waited::Done(r) => r,
        Waited::TimedOut => Err(ES_TIMEOUT),
        Waited::Lost => Err(ES_E_INTERNAL),
    }
}

/// Looks a card up by id (`ps2:Mcd001`) or, if unambiguous, by name (`Mcd001`).
fn find_card(modes: &[(String, CardMode)], arg: &str) -> Option<(String, CardMode)> {
    if let Some(m) = modes.iter().find(|(id, _)| id == arg) {
        return Some(m.clone());
    }
    let mut by_name = modes.iter().filter(|(id, _)| id.split_once(':').map(|(_, n)| n) == Some(arg));
    match (by_name.next(), by_name.next()) {
        (Some(m), None) => Some(m.clone()),
        _ => None,
    }
}

// ----------------------------------------------------------------------------------------------
// Client
// ----------------------------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn es_client_create(cfg: *const es_config, out: *mut *mut Client) -> c_int {
    if out.is_null() {
        return ES_E_INVALID_ARG;
    }
    *out = std::ptr::null_mut();
    let r = catch_unwind(AssertUnwindSafe(|| -> Res {
        let cfg = check_struct(cfg, |c| c.struct_size, "es_config")?;
        let level = match cfg.log_level {
            0 => log::LevelFilter::Error,
            1 => log::LevelFilter::Warn,
            2 => log::LevelFilter::Info,
            3 => log::LevelFilter::Debug,
            4 => log::LevelFilter::Trace,
            _ => return Err(ES_E_INVALID_ARG),
        };
        let cb = Callbacks::new(cfg.log, cfg.log_user, level);
        let _route = route_logs(&cb);
        let data_dir = req_str(cfg.data_dir, "data_dir")?;
        let device_name = opt_str(cfg.device_name, "device_name")?;
        let emulator_id = opt_str(cfg.emulator_id, "emulator_id")?;
        opt_str(cfg.emulator_version, "emulator_version")?;
        // Copied: the host keeps `user` and the functions valid until es_client_destroy.
        let vfs = match unsafe { cfg.vfs.as_ref() } {
            None => None,
            Some(v) => Some(Arc::new(crate::vfs::HostVfs::new(v).map_err(invalid)?)),
        };
        let policy = match cfg.conflict_policy {
            ES_POLICY_ASK => ConflictPolicy::Ask,
            ES_POLICY_NEWEST => ConflictPolicy::Newest,
            p => return Err(invalid(format!("conflict policy {p} is not supported (use ES_POLICY_ASK or NEWEST)"))),
        };
        if cfg.flags & !ES_CFG_KNOWN_FLAGS != 0 {
            return Err(invalid(format!("unknown es_config flags {:#x}", cfg.flags)));
        }
        // Containers in a session are snapshotted only when the host says they are quiet.
        let mut opts = EngineOptions {
            device_name,
            policy,
            scan_busy: false,
            vfs: vfs.map(|v| v as Arc<dyn emusync_core::vfs::Vfs>),
            ..Default::default()
        };
        if let Some(id) = emulator_id {
            opts.emulator_id = id;
        }
        if cfg.snapshot_budget_bytes != 0 {
            opts.snapshot_budget_bytes = cfg.snapshot_budget_bytes;
        }
        let client = Client::create(PathBuf::from(data_dir), opts, cb)?;
        *out = Box::into_raw(client);
        Ok(ES_OK)
    }));
    match r {
        Ok(Ok(s) | Err(s)) => s,
        Err(_) => ES_E_INTERNAL,
    }
}

#[no_mangle]
pub unsafe extern "C" fn es_client_destroy(c: *mut Client, timeout_ms: u32) {
    if c.is_null() {
        return;
    }
    let client = Box::from_raw(c);
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let _route = route_logs(&client.shared.cb.clone());
        (*client).destroy(Duration::from_millis(timeout_ms as u64));
    }));
}

// ----------------------------------------------------------------------------------------------
// Remotes
// ----------------------------------------------------------------------------------------------

/// Sets a remote up on a helper thread, so the worker isn't held up by the network; the
/// result goes to the worker as a `BindRemote` command, and the outcome of binding it (or of
/// the setup, if that failed) to the returned channel. If the remote is rejected and
/// `reserved` (this call reserved `pending_remote`), the reservation is dropped.
///
/// With `passphrase` (from `es_repo_create_encrypted`), a new repository is created encrypted
/// and an existing encrypted one unlocked. Without, an encrypted repository whose key this data
/// directory doesn't have yet is added locked: `ES_E_LOCKED`.
pub(crate) fn setup_remote(
    shared: &Arc<Shared>,
    remote: Arc<dyn Remote>,
    reserved: bool,
    passphrase: Option<Passphrase>,
) -> Result<mpsc::Receiver<i32>, i32> {
    let (tx, rx) = mpsc::channel();
    let shared = shared.clone();
    std::thread::Builder::new()
        .name("emusync-remote-setup".into())
        .spawn(move || {
            let _route = route_logs(&shared.cb);
            let uri = remote.describe();
            let prepared = match &passphrase {
                Some(p) => Engine::prepare_remote_encrypted(&*remote, p, KdfParams::default()),
                None => Engine::prepare_remote(&*remote),
            };
            let status = match prepared {
                Ok(setup) => {
                    // Ahead of queued work, and answered once the data directory is bound (or
                    // binding failed, e.g. the key couldn't be stored), so the caller learns the
                    // real outcome. A dropped reply means the client is shutting down.
                    let (bound_tx, bound_rx) = mpsc::channel();
                    shared.queue.push_urgent(Cmd::BindRemote(remote, Some(setup), None, Some(bound_tx)));
                    bound_rx.recv().unwrap_or(ES_E_INTERNAL)
                }
                Err(e) if e.is_offline() => {
                    log::info!("{uri} is unreachable; it is set up once it can be reached: {e}");
                    shared.queue.push(Cmd::BindRemote(remote, None, passphrase, None));
                    ES_OFFLINE
                }
                Err(e) => {
                    log::warn!("cannot use {uri}: {e}");
                    status_of(&e)
                }
            };
            if status < 0 && status != ES_E_LOCKED && reserved {
                // Rejected (credentials, repository format, binding failed): not added, so free
                // the slot.
                let mut pending = lock(&shared.pending_remote);
                if pending.as_deref() == Some(uri.as_str()) {
                    *pending = None;
                }
            }
            if tx.send(status).is_err() && status < 0 {
                // The caller stopped waiting; tell the host another way.
                let kind = if status == ES_E_AUTH { ES_EV_AUTH_REQUIRED } else { ES_EV_ERROR };
                shared.emit(crate::events::Event::new(kind, status).message(uri));
            }
        })
        .map_err(|_| ES_E_INTERNAL)?;
    Ok(rx)
}

#[no_mangle]
pub unsafe extern "C" fn es_remote_add(
    c: *mut Client,
    remote_uri: *const c_char,
    creds: *const es_credential_provider,
) -> c_int {
    with_client(c, |client| {
        let uri = req_str(remote_uri, "remote_uri")?;
        if creds.is_null() || (*creds).get.is_none() {
            return Err(invalid("a credential provider with a get function is required"));
        }
        if !uri.starts_with("s3://") {
            return Err(invalid(format!("unsupported remote {uri:?} (this version supports s3:// only)")));
        }
        let cfg = S3Config::parse(&uri).map_err(invalid)?;
        let provider = crate::creds::provider(*creds, &uri, client.shared.cb.clone());
        let remote: Arc<dyn Remote> = Arc::new(S3Remote::new(cfg, Box::new(provider)).map_err(invalid)?);
        let uri = remote.describe();
        let reserved = {
            // Reserved first, so a second es_remote_add can't slip in while this one is offline.
            let mut pending = lock(&client.shared.pending_remote);
            let bound = client.shared.cache().remote.clone();
            if let Some(other) = bound.as_ref().or(pending.as_ref()).filter(|b| **b != uri) {
                return Err(invalid(format!(
                    "this data directory is bound to {other}; use a new data directory for {uri}"
                )));
            }
            let reserved = pending.is_none();
            *pending = Some(uri);
            reserved
        };
        let passphrase = lock(&client.shared.create_passphrase).take();
        let rx = setup_remote(&client.shared, remote, reserved, passphrase)?;
        Ok(rx.recv_timeout(REMOTE_ADD_WAIT).unwrap_or(ES_OFFLINE))
    })
}

/// A passphrase argument. Copied straight into memory that is wiped after use; never logged.
unsafe fn passphrase_arg(p: *const c_char) -> Result<Passphrase, i32> {
    if p.is_null() {
        return Err(invalid("passphrase is NULL"));
    }
    let bytes = CStr::from_ptr(p).to_bytes();
    let s = Passphrase::new(
        std::str::from_utf8(bytes).map_err(|_| invalid("the passphrase is not valid UTF-8"))?.to_string(),
    );
    if s.is_empty() {
        return Err(invalid("the passphrase is empty"));
    }
    Ok(s)
}

#[no_mangle]
pub unsafe extern "C" fn es_repo_create_encrypted(c: *mut Client, passphrase: *const c_char) -> c_int {
    with_client(c, |client| {
        let p = passphrase_arg(passphrase)?;
        *lock(&client.shared.create_passphrase) = Some(p);
        Ok(ES_OK)
    })
}

#[no_mangle]
pub unsafe extern "C" fn es_repo_unlock(c: *mut Client, passphrase: *const c_char) -> c_int {
    with_client(c, |client| {
        let p = passphrase_arg(passphrase)?;
        let (pending, reply) = Pending::new();
        client.shared.queue.push(Cmd::Unlock(p, reply));
        match pending.wait(WORKER_WAIT, WaitMode::CancelIfQueued) {
            Waited::Done(status) => Ok(status),
            Waited::TimedOut => Err(ES_TIMEOUT),
            Waited::Lost => Err(ES_E_INTERNAL),
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn es_oauth_begin(
    c: *mut Client,
    _provider: *const c_char,
    _redirect_uri: *const c_char,
    out_url: *mut c_char,
    cap: usize,
) -> c_int {
    if !out_url.is_null() && cap > 0 {
        *out_url = 0;
    }
    with_client(c, |_| Err(invalid("no backend in this version uses OAuth")))
}

#[no_mangle]
pub unsafe extern "C" fn es_oauth_complete(
    c: *mut Client,
    _provider: *const c_char,
    _redirect_with_code: *const c_char,
) -> c_int {
    with_client(c, |_| Err(invalid("no backend in this version uses OAuth")))
}

// ----------------------------------------------------------------------------------------------
// Containers and cards
// ----------------------------------------------------------------------------------------------

/// Runs a call that needs no client: no panic escapes. (Its log records have no callback to go
/// to.)
fn guarded(f: impl FnOnce() -> Res) -> i32 {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(s) | Err(s)) => s,
        Err(_) => ES_E_INTERNAL,
    }
}

unsafe fn secret_args(
    service: *const c_char,
    name: *const c_char,
    fallback_dir: *const c_char,
) -> Result<(String, String, Option<PathBuf>), i32> {
    Ok((
        req_str(service, "service")?,
        req_str(name, "name")?,
        opt_str(fallback_dir, "fallback_dir")?.filter(|d| !d.is_empty()).map(PathBuf::from),
    ))
}

#[no_mangle]
pub unsafe extern "C" fn es_secret_set(
    service: *const c_char,
    name: *const c_char,
    value: *const c_char,
    fallback_dir: *const c_char,
) -> c_int {
    guarded(|| {
        let (service, name, dir) = secret_args(service, name, fallback_dir)?;
        if value.is_null() {
            return Err(invalid("value is NULL"));
        }
        let Ok(value) = CStr::from_ptr(value).to_str() else { return Err(invalid("value is not valid UTF-8")) };
        let value = zeroize::Zeroizing::new(value.to_owned());
        emusync_core::secrets::set(&service, &name, &value, dir.as_deref()).map(|_| ES_OK).map_err(|e| status_of(&e))
    })
}

#[no_mangle]
pub unsafe extern "C" fn es_secret_get(
    service: *const c_char,
    name: *const c_char,
    fallback_dir: *const c_char,
    out: *mut c_char,
    cap: usize,
    out_len: *mut usize,
) -> c_int {
    if !out.is_null() && cap > 0 {
        *out = 0;
    }
    if !out_len.is_null() {
        *out_len = 0;
    }
    guarded(|| {
        let (service, name, dir) = secret_args(service, name, fallback_dir)?;
        if out.is_null() && cap > 0 {
            return Err(invalid("out is NULL"));
        }
        let Some(value) = emusync_core::secrets::get(&service, &name, dir.as_deref()).map_err(|e| status_of(&e))?
        else {
            return Err(ES_E_NOT_FOUND);
        };
        let needed = value.len() + 1;
        if !out_len.is_null() {
            *out_len = needed;
        }
        if cap < needed {
            return Err(ES_E_NO_SPACE);
        }
        std::ptr::copy_nonoverlapping(value.as_ptr(), out.cast::<u8>(), value.len());
        *out.add(value.len()) = 0;
        Ok(ES_OK)
    })
}

#[no_mangle]
pub unsafe extern "C" fn es_secret_delete(
    service: *const c_char,
    name: *const c_char,
    fallback_dir: *const c_char,
) -> c_int {
    guarded(|| {
        let (service, name, dir) = secret_args(service, name, fallback_dir)?;
        emusync_core::secrets::delete(&service, &name, dir.as_deref()).map(|_| ES_OK).map_err(|e| status_of(&e))
    })
}

#[no_mangle]
pub unsafe extern "C" fn es_container_add(
    c: *mut Client,
    d: *const es_container_desc,
    out: *mut es_container_id,
) -> c_int {
    with_client(c, |client| {
        let d = check_struct(d, |d| d.struct_size, "es_container_desc")?;
        if out.is_null() {
            return Err(invalid("out is NULL"));
        }
        let location = req_str(d.location, "location")?;
        let format = opt_str(d.format, "format")?;
        let platform = opt_str(d.platform, "platform")?;
        let card = opt_str(d.card, "card")?;
        if opt_str(d.title_scope, "title_scope")?.is_some() {
            return Err(invalid(if card.is_none() {
                "title_scope needs card"
            } else {
                "per-game containers (title_scope) are not supported yet"
            }));
        }
        if d.flags & !(ES_CF_READ_ONLY | ES_CF_NO_UPLOAD | ES_CF_REPLACE) != 0 {
            return Err(invalid(format!("unknown container flags {:#x}", d.flags)));
        }
        if d.flags & ES_CF_NO_UPLOAD != 0 {
            return Err(invalid("ES_CF_NO_UPLOAD is not supported yet"));
        }
        // The container it would replace may belong to a session, one that is still starting
        // included, which no container may be taken from.
        if d.flags & ES_CF_REPLACE != 0 && !lock(&client.shared.sessions).open.is_empty() {
            return Err(ES_E_BUSY);
        }
        let desc = ContainerDesc {
            location: PathBuf::from(location),
            format,
            platform,
            card,
            read_only: d.flags & ES_CF_READ_ONLY != 0,
            replace: d.flags & ES_CF_REPLACE != 0,
        };
        let (p, reply) = Pending::new();
        client.shared.queue.push(Cmd::AddContainer(desc, reply));
        let id = waited(p.wait(WORKER_WAIT, WaitMode::CancelIfQueued))?;
        *out = id as u64;
        Ok(ES_OK)
    })
}

#[no_mangle]
pub unsafe extern "C" fn es_container_remove(c: *mut Client, id: es_container_id) -> c_int {
    with_client(c, |client| {
        let id = id as i64;
        if !client.shared.cache().containers.iter().any(|x| x.id == id) {
            return Err(ES_E_NOT_FOUND);
        }
        {
            // A session that is still starting isn't holding its containers yet, so check the
            // registry, and keep new sessions off the container until it is gone.
            let mut sessions = lock(&client.shared.sessions);
            if client.shared.is_held(id) || sessions.open.values().any(|s| s.mounted.contains(&id)) {
                return Err(ES_E_BUSY);
            }
            sessions.removing.insert(id);
        }
        let (p, reply) = Pending::new();
        client.shared.queue.push(Cmd::RemoveContainer(id, reply));
        let r = waited(p.wait(WORKER_WAIT, WaitMode::CancelIfQueued));
        lock(&client.shared.sessions).removing.remove(&id);
        r?;
        Ok(ES_OK)
    })
}

#[no_mangle]
pub unsafe extern "C" fn es_card_get_mode(c: *mut Client, card_id: *const c_char, out: *mut c_int) -> c_int {
    with_client(c, |client| {
        let card = req_str(card_id, "card_id")?;
        if out.is_null() {
            return Err(invalid("out is NULL"));
        }
        let (_, mode) = find_card(&client.shared.cache().modes, &card).ok_or(ES_E_NOT_FOUND)?;
        *out = match mode {
            CardMode::Units => ES_CARD_UNITS,
            CardMode::WholeFile => ES_CARD_WHOLE_FILE,
        };
        Ok(ES_OK)
    })
}

#[no_mangle]
pub unsafe extern "C" fn es_card_set_mode(c: *mut Client, card_id: *const c_char, mode: c_int) -> c_int {
    with_client(c, |client| {
        let card = req_str(card_id, "card_id")?;
        let mode = match mode {
            ES_CARD_UNITS => CardMode::Units,
            ES_CARD_WHOLE_FILE => CardMode::WholeFile,
            m => return Err(invalid(format!("unknown card mode {m}"))),
        };
        let cache = client.shared.cache();
        let (card_id, current) = find_card(&cache.modes, &card).ok_or(ES_E_NOT_FOUND)?;
        if current == mode {
            return Ok(ES_OK);
        }
        let containers: Vec<_> = cache.containers.iter().filter(|x| x.card_id == card_id).collect();
        if containers.iter().any(|x| client.shared.is_held(x.id)) {
            return Err(ES_E_BUSY);
        }
        if mode == CardMode::Units {
            if let Some(x) =
                containers.iter().find(|x| !emusync_formats::by_id(&x.format).is_some_and(|f| f.supports_units()))
            {
                return Err(invalid(format!("{} ({}) can't be split into saves", x.location, x.format)));
            }
        }
        drop(cache);
        client.shared.queue.push(Cmd::SetMode(card_id, mode));
        Ok(ES_PENDING)
    })
}

#[no_mangle]
pub unsafe extern "C" fn es_container_touched(c: *mut Client, id: es_container_id) {
    with_client(c, |client| {
        client.shared.queue.push(Cmd::Touched(id as i64));
        Ok(ES_OK)
    });
}

#[no_mangle]
pub unsafe extern "C" fn es_container_flushed(c: *mut Client, id: es_container_id) -> c_int {
    with_client(c, |client| {
        let id = id as i64;
        if !client.shared.cache().containers.iter().any(|x| x.id == id) {
            return Err(ES_E_NOT_FOUND);
        }
        let (p, reply) = Pending::new();
        client.shared.queue.push_urgent(Cmd::Flushed(id, reply));
        match p.wait(FLUSH_WAIT, WaitMode::Deadline) {
            Waited::Done(s) => Ok(s),
            Waited::TimedOut => Ok(ES_PENDING),
            Waited::Lost => Err(ES_E_INTERNAL),
        }
    })
}

// ----------------------------------------------------------------------------------------------
// Sessions
// ----------------------------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn es_session_begin(
    c: *mut Client,
    args: *const es_session_begin_args,
    out: *mut es_session_info,
) -> c_int {
    with_client(c, |client| {
        let args = check_struct(args, |a| a.struct_size, "es_session_begin_args")?;
        check_struct(out, |o| o.struct_size, "es_session_info")?;
        let title = check_struct(args.title, |t| t.struct_size, "title")?;
        let platform = req_str(title.platform, "title.platform")?;
        let serial = opt_str(title.serial, "title.serial")?;
        let hash = opt_str(title.content_hash, "title.content_hash")?;
        opt_str(title.display_name, "title.display_name")?;
        let title_key = match serial.or(hash) {
            Some(s) => format!("{platform}:{s}"),
            None => return Err(invalid("title needs a serial or a content_hash")),
        };
        if args.flags & !ES_SB_QUERY_RESUME_STATE != 0 {
            return Err(invalid(format!("unknown session flags {:#x}", args.flags)));
        }
        if args.flags & ES_SB_QUERY_RESUME_STATE != 0 {
            req_str(args.state_compat_key, "state_compat_key")?;
        }
        let mounted: Vec<i64> = match args.mounted_count {
            0 => Vec::new(),
            _ if args.mounted.is_null() => return Err(invalid("mounted is NULL")),
            n => std::slice::from_raw_parts(args.mounted, n).iter().map(|id| *id as i64).collect(),
        };
        {
            let cache = client.shared.cache();
            if let Some(id) = mounted.iter().find(|id| !cache.containers.iter().any(|x| x.id == **id)) {
                log::warn!("es_session_begin: no container {id}");
                return Err(ES_E_NOT_FOUND);
            }
        }
        let session = {
            let mut sessions = lock(&client.shared.sessions);
            if sessions.open.values().any(|s| s.mounted.iter().any(|id| mounted.contains(id)))
                || mounted.iter().any(|id| sessions.removing.contains(id))
            {
                return Err(ES_E_BUSY);
            }
            let id = client.shared.next_session.fetch_add(1, Ordering::SeqCst);
            sessions.open.insert(id, Session { mounted: mounted.clone() });
            id
        };
        let timeout = match args.timeout_ms {
            0 => DEFAULT_SESSION_TIMEOUT_MS,
            t => t,
        };
        let (p, reply) = Pending::new();
        client.shared.queue.push(Cmd::SessionBegin { mounted: mounted.clone(), reply });
        let w = p.wait(Duration::from_millis(timeout as u64), WaitMode::Deadline);
        // From here on the worker no longer writes to these containers.
        client.shared.hold(&mounted);

        clear_out(out);
        let info = &mut *out;
        info.id = session;
        let cache = client.shared.cache();
        let card_of = |id: &i64| cache.containers.iter().find(|x| x.id == *id).map(|x| x.card_id.clone());
        let cards: Vec<String> = mounted.iter().filter_map(card_of).collect();
        let conflicts = cache
            .conflicts
            .iter()
            .filter(|x| {
                x.tips.iter().any(|t| t.version.titles.contains(&title_key))
                    || (x.unit.ends_with(":@card") && cards.iter().any(|card| x.unit.starts_with(&format!("{card}:"))))
            })
            .count() as u32;
        info.conflicts = conflicts;
        let status = match w {
            Waited::Done(o) => {
                info.units_applied = o.applied;
                info.units_blocked = o.blocked;
                info.remote_reached = o.remote_reached as u8;
                if conflicts > 0 {
                    ES_CONFLICT
                } else if !o.remote_reached {
                    ES_OFFLINE
                } else {
                    ES_OK
                }
            }
            Waited::TimedOut => ES_TIMEOUT,
            Waited::Lost => ES_OFFLINE,
        };
        Ok(status)
    })
}

#[no_mangle]
pub unsafe extern "C" fn es_session_end(c: *mut Client, id: es_session_id, _played_ms: u64) -> c_int {
    with_client(c, |client| {
        let session = lock(&client.shared.sessions).open.remove(&id).ok_or(ES_E_NOT_FOUND)?;
        client.shared.queue.push(Cmd::SessionEnd { mounted: session.mounted });
        Ok(ES_PENDING)
    })
}

#[no_mangle]
pub unsafe extern "C" fn es_app_suspend(c: *mut Client, budget_ms: u32) -> c_int {
    with_client(c, |client| {
        let (p, reply) = Pending::new();
        client.shared.queue.push_urgent(Cmd::Suspend(reply));
        match p.wait(Duration::from_millis(budget_ms as u64), WaitMode::Deadline) {
            Waited::Done(s) => Ok(s),
            Waited::TimedOut => Ok(ES_TIMEOUT),
            Waited::Lost => Err(ES_E_INTERNAL),
        }
    })
}

fn queue_sync(shared: &Shared) {
    if !shared.sync_queued.swap(true, Ordering::SeqCst) {
        shared.queue.push(Cmd::Sync);
    }
}

#[no_mangle]
pub unsafe extern "C" fn es_app_resume(c: *mut Client) -> c_int {
    with_client(c, |client| {
        queue_sync(&client.shared);
        Ok(ES_PENDING)
    })
}

#[no_mangle]
pub unsafe extern "C" fn es_sync_now(c: *mut Client) -> c_int {
    with_client(c, |client| {
        queue_sync(&client.shared);
        Ok(ES_PENDING)
    })
}

// ----------------------------------------------------------------------------------------------
// Save states (M3)
// ----------------------------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn es_state_publish(c: *mut Client, _d: *const es_state_desc) -> c_int {
    with_client(c, |_| Err(invalid("save states are not supported yet")))
}

#[no_mangle]
pub unsafe extern "C" fn es_state_query_latest(
    c: *mut Client,
    _t: *const es_title,
    _compat_key: *const c_char,
    _slot: *const c_char,
    _out: *mut es_state_info,
) -> c_int {
    with_client(c, |_| Err(invalid("save states are not supported yet")))
}

#[no_mangle]
pub unsafe extern "C" fn es_state_fetch(
    c: *mut Client,
    _state_id: *const c_char,
    _dest_path: *const c_char,
    _timeout_ms: u32,
) -> c_int {
    with_client(c, |_| Err(invalid("save states are not supported yet")))
}

// ----------------------------------------------------------------------------------------------
// Conflicts and history
// ----------------------------------------------------------------------------------------------

fn version_info(e: &HistoryEntry) -> es_version_info {
    // SAFETY: plain old data; all zeroes is a valid value.
    let mut v: es_version_info = unsafe { std::mem::zeroed() };
    copy_str(&mut v.version_id, &e.version.id);
    copy_str(&mut v.device_name, &e.device);
    v.modified_unix_ms = e.version.hlc.ms;
    v.playtime_ms = e.version.playtime_ms;
    v.size = e.version.size;
    v.is_tombstone = e.version.tombstone as u8;
    v
}

/// The tips of a conflict other than this device's version.
fn remote_tips(x: &emusync_core::ConflictInfo) -> Vec<&HistoryEntry> {
    let local = x.local.as_ref().map(|l| l.version.id.as_str());
    x.tips.iter().filter(|t| Some(t.version.id.as_str()) != local).take(4).collect()
}

#[no_mangle]
pub unsafe extern "C" fn es_conflict_get(c: *mut Client, id: es_conflict_id, out: *mut es_conflict_info) -> c_int {
    with_client(c, |client| {
        check_struct(out, |o| o.struct_size, "es_conflict_info")?;
        let cache = client.shared.cache();
        let x = cache.conflicts.iter().find(|x| x.id as u64 == id).ok_or(ES_E_NOT_FOUND)?;
        clear_out(out);
        let info = &mut *out;
        info.container = x
            .location
            .as_ref()
            .and_then(|l| cache.containers.iter().find(|c| &c.location == l))
            .map_or(0, |c| c.id as u64);
        copy_str(&mut info.unit_id, &x.unit);
        copy_str(&mut info.unit_name, emusync_core::split_unit(&x.unit).map_or("", |(_, n)| n));
        let newest = x.tips.last().or(x.local.as_ref());
        if let Some(t) = newest {
            copy_str(&mut info.title_key, t.version.titles.first().map_or("", |s| s.as_str()));
            copy_str(&mut info.unit_title, t.version.meta.get("title").map_or("", |s| s.as_str()));
        }
        if let Some(l) = &x.local {
            info.local = version_info(l);
        }
        let remote = remote_tips(x);
        for (slot, t) in info.remote.iter_mut().zip(&remote) {
            *slot = version_info(t);
        }
        info.remote_count = remote.len() as u32;
        Ok(ES_OK)
    })
}

#[no_mangle]
pub unsafe extern "C" fn es_conflict_resolve(c: *mut Client, id: es_conflict_id, r: c_int, remote_index: u32) -> c_int {
    with_client(c, |client| {
        let cache = client.shared.cache();
        let x = cache.conflicts.iter().find(|x| x.id as u64 == id).ok_or(ES_E_NOT_FOUND)?;
        let resolution = match r {
            ES_RESOLVE_KEEP_LOCAL if x.local.is_some() => Resolution::KeepLocal,
            ES_RESOLVE_KEEP_LOCAL => return Err(invalid("this device has no version of the unit to keep")),
            ES_RESOLVE_TAKE_REMOTE => match remote_tips(x).get(remote_index as usize) {
                Some(t) => Resolution::Take(t.version.id.clone()),
                None => return Err(invalid(format!("remote_index {remote_index} is out of range"))),
            },
            r => return Err(invalid(format!("unknown resolution {r}"))),
        };
        let conflict = x.id;
        drop(cache);
        client.shared.queue.push(Cmd::Resolve(conflict, resolution));
        Ok(ES_PENDING)
    })
}

#[no_mangle]
pub unsafe extern "C" fn es_unit_history(
    c: *mut Client,
    unit_id: *const c_char,
    cb: es_version_cb,
    user: *mut c_void,
) -> c_int {
    with_client(c, |client| {
        let unit = req_str(unit_id, "unit_id")?;
        let Some(cb) = cb else { return Err(invalid("cb is NULL")) };
        let (p, reply) = Pending::new();
        client.shared.queue.push(Cmd::History(unit, reply));
        let entries = waited(p.wait(WORKER_WAIT, WaitMode::CancelIfQueued))?;
        for e in &entries {
            let v = version_info(e);
            cb(user, &v);
        }
        Ok(ES_OK)
    })
}

#[no_mangle]
pub unsafe extern "C" fn es_unit_restore(c: *mut Client, unit_id: *const c_char, version_id: *const c_char) -> c_int {
    with_client(c, |client| {
        let unit = req_str(unit_id, "unit_id")?;
        let version = req_str(version_id, "version_id")?;
        let (p, reply) = Pending::new();
        client.shared.queue.push(Cmd::Restore(unit, version, reply));
        waited(p.wait(WORKER_WAIT, WaitMode::CancelIfQueued))?;
        Ok(ES_PENDING)
    })
}

// ----------------------------------------------------------------------------------------------
// Events
// ----------------------------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn es_event_poll(c: *mut Client, out: *mut es_event) -> c_int {
    with_client(c, |client| {
        check_struct(out, |o| o.struct_size, "es_event")?;
        Ok(client.shared.events.poll(&mut *out) as c_int)
    })
}

#[no_mangle]
pub unsafe extern "C" fn es_event_release(c: *mut Client, ev: *mut es_event) {
    if ev.is_null() {
        return;
    }
    with_client(c, |client| {
        client.shared.events.release(&mut *ev);
        Ok(ES_OK)
    });
}

#[no_mangle]
pub unsafe extern "C" fn es_event_set_notifier(c: *mut Client, wake: es_wake_fn, user: *mut c_void) {
    with_client(c, |client| {
        client.shared.cb.set_wake(wake, user);
        Ok(ES_OK)
    });
}

//! The exported functions, called the way a host calls them, against an in-memory remote
//! (attached through a test-only hook; the C API has no memory remote).

use crate::abi::*;
use crate::api::*;
use crate::Client;
use emusync_formats::ps2::mcfs::{Node, NodeKind, MODE_DIR_DEFAULT, MODE_FILE_DEFAULT};
use emusync_formats::ps2::{blank_card, encode, layout, LAYOUT_META};
use emusync_formats::{ApplyOp, Canonical, ContainerFormat, Ps2Card};
use emusync_remote::{MemoryRemote, PutOutcome, Remote};
use std::collections::BTreeMap;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const GTA: &str = "BASLUS-20062GTA3";
const RAC: &str = "BASCUS-97199RAC";
const T: [u8; 8] = [0, 0, 30, 12, 18, 10, 0xEA, 0x07];

fn save(name: &str, data: &[u8]) -> Canonical {
    let file = |n: &str, d: Vec<u8>| Node {
        name: n.as_bytes().to_vec(),
        mode: MODE_FILE_DEFAULT,
        attr: 0,
        created: T,
        modified: T,
        kind: NodeKind::File(d),
    };
    let node = Node {
        name: name.as_bytes().to_vec(),
        mode: MODE_DIR_DEFAULT,
        attr: 0,
        created: T,
        modified: T,
        kind: NodeKind::Dir(vec![file("icon.sys", vec![0x50; 964]), file("data", data.to_vec())]),
    };
    Canonical { name: name.into(), bytes: encode(&node), meta: [(LAYOUT_META.to_string(), layout(&node))].into() }
}

fn cstr(s: &str) -> CString {
    CString::new(s).unwrap()
}

#[derive(Debug, Clone)]
struct Ev {
    kind: i32,
    status: i32,
    container: u64,
    conflict_id: u64,
    unit_name: Option<String>,
    title_key: Option<String>,
    message: Option<String>,
}

unsafe fn opt(p: *const c_char) -> Option<String> {
    (!p.is_null()).then(|| CStr::from_ptr(p).to_string_lossy().into_owned())
}

fn config_init() -> es_config {
    es_config {
        struct_size: std::mem::size_of::<es_config>() as u32,
        data_dir: std::ptr::null(),
        device_name: std::ptr::null(),
        emulator_id: std::ptr::null(),
        emulator_version: std::ptr::null(),
        vfs: std::ptr::null(),
        conflict_policy: ES_POLICY_ASK,
        flags: 0,
        snapshot_budget_bytes: 0,
        log: None,
        log_user: std::ptr::null_mut(),
        log_level: 2,
    }
}

fn event_init() -> es_event {
    let mut ev: es_event = unsafe { std::mem::zeroed() };
    ev.struct_size = std::mem::size_of::<es_event>() as u32;
    ev
}

struct Dev {
    c: *mut Client,
    card: PathBuf,
    id: u64,
    _dir: tempfile::TempDir,
}

impl Dev {
    /// A device with a PS2 card in slot 1, on `remote` if given.
    fn new(name: &str, remote: Option<Arc<dyn Remote>>, policy: i32) -> Dev {
        let dir = tempfile::tempdir().unwrap();
        let card = dir.path().join("memcards").join("Mcd001.ps2");
        std::fs::create_dir_all(card.parent().unwrap()).unwrap();
        std::fs::write(&card, blank_card(8192, true, T)).unwrap();
        let data = cstr(dir.path().join("data").to_str().unwrap());
        let device = cstr(name);
        let mut cfg = config_init();
        cfg.data_dir = data.as_ptr();
        cfg.device_name = device.as_ptr();
        cfg.conflict_policy = policy;
        let mut c = std::ptr::null_mut();
        assert_eq!(unsafe { es_client_create(&cfg, &mut c) }, ES_OK);
        if let Some(r) = remote {
            let client = unsafe { &*c };
            let status = crate::api::setup_remote(&client.shared, r, false, None).unwrap().recv().unwrap();
            assert_eq!(status, ES_OK);
        }
        let id = add(c, &card, None).unwrap();
        Dev { c, card, id, _dir: dir }
    }

    fn write_saves(&self, ops: &[ApplyOp]) {
        let img = std::fs::read(&self.card).unwrap();
        std::fs::write(&self.card, Ps2Card.apply(&img, ops).unwrap()).unwrap();
    }

    fn put(&self, name: &str, data: &[u8]) {
        self.write_saves(&[ApplyOp::Put(save(name, data))]);
    }

    /// Save name → its data file, as the card holds them now.
    fn saves(&self) -> BTreeMap<String, Vec<u8>> {
        let img = std::fs::read(&self.card).unwrap();
        Ps2Card
            .extract_all(&img)
            .unwrap()
            .into_iter()
            .map(|c| {
                let node = emusync_formats::ps2::decode(&c, T).unwrap();
                let NodeKind::Dir(files) = node.kind else { panic!() };
                let data = files.into_iter().find(|f| f.name == b"data").map(|f| match f.kind {
                    NodeKind::File(d) => d,
                    NodeKind::Dir(_) => vec![],
                });
                (c.name, data.unwrap_or_default())
            })
            .collect()
    }

    fn events(&self) -> Vec<Ev> {
        let mut out = Vec::new();
        let mut ev = event_init();
        while unsafe { es_event_poll(self.c, &mut ev) } == 1 {
            out.push(unsafe {
                Ev {
                    kind: ev.type_,
                    status: ev.status,
                    container: ev.container,
                    conflict_id: ev.conflict_id,
                    unit_name: opt(ev.unit_name),
                    title_key: opt(ev.title_key),
                    message: opt(ev.message),
                }
            });
            unsafe { es_event_release(self.c, &mut ev) };
        }
        out
    }

    /// Polls until an event matches, returning every event seen so far.
    fn wait_for(&self, what: &str, pred: impl Fn(&Ev) -> bool) -> Vec<Ev> {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut seen = Vec::new();
        loop {
            seen.extend(self.events());
            if seen.iter().any(&pred) {
                return seen;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}; saw {seen:#?}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn sync(&self) -> Vec<Ev> {
        self.events();
        assert_eq!(unsafe { es_sync_now(self.c) }, ES_PENDING);
        self.wait_for("sync", |e| e.kind == ES_EV_SYNC_FINISHED)
    }

    /// Snapshot and push, as `onPause` would.
    fn suspend(&self) -> i32 {
        unsafe { es_app_suspend(self.c, 10_000) }
    }

    fn begin(&self, timeout_ms: u32) -> (i32, es_session_info) {
        let platform = cstr("ps2");
        let serial = cstr("SLUS-20062");
        let title = es_title {
            struct_size: std::mem::size_of::<es_title>() as u32,
            platform: platform.as_ptr(),
            serial: serial.as_ptr(),
            content_hash: std::ptr::null(),
            display_name: std::ptr::null(),
        };
        let args = es_session_begin_args {
            struct_size: std::mem::size_of::<es_session_begin_args>() as u32,
            title: &title,
            mounted: &self.id,
            mounted_count: 1,
            timeout_ms,
            flags: 0,
            state_compat_key: std::ptr::null(),
        };
        let mut info: es_session_info = unsafe { std::mem::zeroed() };
        info.struct_size = std::mem::size_of::<es_session_info>() as u32;
        let status = unsafe { es_session_begin(self.c, &args, &mut info) };
        (status, info)
    }
}

impl Dev {
    /// Destroys the client and creates a new one on the same data directory, as a restarted
    /// app would.
    fn reopen(&mut self) {
        unsafe { es_client_destroy(self.c, 10_000) };
        let data = cstr(self._dir.path().join("data").to_str().unwrap());
        let mut cfg = config_init();
        cfg.data_dir = data.as_ptr();
        self.c = std::ptr::null_mut();
        assert_eq!(unsafe { es_client_create(&cfg, &mut self.c) }, ES_OK);
    }
}

/// `es_remote_add` for the in-memory remote: sets it up with the passphrase from
/// `es_repo_create_encrypted`, if one was given, and returns the status.
fn attach(c: *mut Client, r: Arc<dyn Remote>) -> i32 {
    let client = unsafe { &*c };
    let passphrase = crate::client::lock(&client.shared.create_passphrase).take();
    crate::api::setup_remote(&client.shared, r, false, passphrase).unwrap().recv().unwrap()
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

impl Drop for Dev {
    fn drop(&mut self) {
        unsafe { es_client_destroy(self.c, 10_000) };
    }
}

fn add(c: *mut Client, path: &Path, format: Option<&str>) -> Result<u64, i32> {
    let loc = cstr(path.to_str().unwrap());
    let fmt = format.map(cstr);
    let d = es_container_desc {
        struct_size: std::mem::size_of::<es_container_desc>() as u32,
        format: fmt.as_ref().map_or(std::ptr::null(), |f| f.as_ptr()),
        location: loc.as_ptr(),
        platform: std::ptr::null(),
        card: std::ptr::null(),
        title_scope: std::ptr::null(),
        flags: 0,
    };
    let mut id = 0;
    match unsafe { es_container_add(c, &d, &mut id) } {
        ES_OK => Ok(id),
        e => Err(e),
    }
}

fn memory() -> Arc<MemoryRemote> {
    Arc::new(MemoryRemote::default())
}

// ----------------------------------------------------------------------------------------------

#[test]
fn create_validates_its_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let data = cstr(dir.path().to_str().unwrap());
    let mut c = 1 as *mut Client;
    unsafe {
        assert_eq!(es_client_create(std::ptr::null(), &mut c), ES_E_INVALID_ARG);
        assert!(c.is_null());
        let mut cfg = config_init();
        assert_eq!(es_client_create(&cfg, &mut c), ES_E_INVALID_ARG, "data_dir is required");
        cfg.data_dir = data.as_ptr();
        cfg.struct_size = 8;
        assert_eq!(es_client_create(&cfg, &mut c), ES_E_ABI);
        cfg.struct_size = std::mem::size_of::<es_config>() as u32;
        cfg.conflict_policy = 2;
        assert_eq!(es_client_create(&cfg, &mut c), ES_E_INVALID_ARG, "prefer-local is reserved");
        cfg.conflict_policy = ES_POLICY_ASK;
        let mut vfs = host_vfs(std::ptr::null());
        vfs.read = None;
        cfg.vfs = &vfs;
        assert_eq!(es_client_create(&cfg, &mut c), ES_E_INVALID_ARG, "es_vfs.read is required");
        let mut vfs = host_vfs(std::ptr::null());
        vfs.struct_size = 8;
        cfg.vfs = &vfs;
        assert_eq!(es_client_create(&cfg, &mut c), ES_E_INVALID_ARG, "too small");
        cfg.vfs = std::ptr::null();
        let bad = [0xC3u8, 0x28, 0];
        cfg.device_name = bad.as_ptr().cast();
        assert_eq!(es_client_create(&cfg, &mut c), ES_E_INVALID_ARG, "not UTF-8");
        cfg.device_name = std::ptr::null();
        cfg.log_level = 9;
        assert_eq!(es_client_create(&cfg, &mut c), ES_E_INVALID_ARG);
        cfg.log_level = 2;
        assert_eq!(es_client_create(&cfg, std::ptr::null_mut()), ES_E_INVALID_ARG);

        // A larger struct (from a newer header) is fine.
        #[repr(C)]
        struct Newer {
            v1: es_config,
            extra: u64,
        }
        let newer = Newer { v1: es_config { struct_size: std::mem::size_of::<Newer>() as u32, ..cfg }, extra: 7 };
        assert_eq!(es_client_create(&newer.v1, &mut c), ES_OK);
        let mut second = std::ptr::null_mut();
        assert_eq!(es_client_create(&cfg, &mut second), ES_E_BUSY, "one client per data_dir");
        es_client_destroy(c, 5000);
        assert_eq!(es_client_create(&cfg, &mut second), ES_OK, "free again after destroy");
        es_client_destroy(second, 5000);
        es_client_destroy(std::ptr::null_mut(), 0);

        // Every call on a NULL client is refused, not a crash.
        assert_eq!(es_sync_now(std::ptr::null_mut()), ES_E_INVALID_ARG);
        let mut ev = event_init();
        assert_eq!(es_event_poll(std::ptr::null_mut(), &mut ev), ES_E_INVALID_ARG);
        es_container_touched(std::ptr::null_mut(), 1);
        es_event_release(std::ptr::null_mut(), &mut ev);
    }
}

#[test]
fn containers_and_card_modes() {
    let dev = Dev::new("pc", None, ES_POLICY_ASK);
    let c = dev.c;
    unsafe {
        assert_eq!(add(c, &dev.card, None), Ok(dev.id), "re-adding is idempotent");
        let mut mode = -1;
        assert_eq!(es_card_get_mode(c, cstr("ps2:Mcd001").as_ptr(), &mut mode), ES_OK);
        assert_eq!(mode, ES_CARD_UNITS);
        assert_eq!(es_card_get_mode(c, cstr("Mcd001").as_ptr(), &mut mode), ES_OK, "by name");
        assert_eq!(es_card_get_mode(c, cstr("ps2:Nope").as_ptr(), &mut mode), ES_E_NOT_FOUND);

        // An SRAM file is whole-file only.
        let sram = dev._dir.path().join("game.srm");
        std::fs::write(&sram, b"sram").unwrap();
        let sid = add(c, &sram, None).unwrap();
        assert_eq!(es_card_get_mode(c, cstr("sram:game").as_ptr(), &mut mode), ES_OK);
        assert_eq!(mode, ES_CARD_WHOLE_FILE);
        assert_eq!(es_card_set_mode(c, cstr("sram:game").as_ptr(), ES_CARD_UNITS), ES_E_INVALID_ARG);
        assert_eq!(es_card_set_mode(c, cstr("sram:game").as_ptr(), ES_CARD_WHOLE_FILE), ES_OK);
        assert_eq!(es_card_set_mode(c, cstr("sram:game").as_ptr(), 7), ES_E_INVALID_ARG);

        // The PS2 card can switch to whole-file mode.
        assert_eq!(es_card_set_mode(c, cstr("ps2:Mcd001").as_ptr(), ES_CARD_WHOLE_FILE), ES_PENDING);
        dev.sync();
        assert_eq!(es_card_get_mode(c, cstr("ps2:Mcd001").as_ptr(), &mut mode), ES_OK);
        assert_eq!(mode, ES_CARD_WHOLE_FILE);

        // Per-game containers and receive-only containers are not in v1.
        let loc = cstr(sram.to_str().unwrap());
        let card = cstr("Mcd001");
        let scope = cstr("ps2:SLUS-20062");
        let mut d = es_container_desc {
            struct_size: std::mem::size_of::<es_container_desc>() as u32,
            format: std::ptr::null(),
            location: loc.as_ptr(),
            platform: std::ptr::null(),
            card: std::ptr::null(),
            title_scope: scope.as_ptr(),
            flags: 0,
        };
        let mut id = 0;
        assert_eq!(es_container_add(c, &d, &mut id), ES_E_INVALID_ARG, "title_scope without card");
        d.card = card.as_ptr();
        assert_eq!(es_container_add(c, &d, &mut id), ES_E_INVALID_ARG, "per-game containers");
        d.title_scope = std::ptr::null();
        d.flags = ES_CF_NO_UPLOAD;
        assert_eq!(es_container_add(c, &d, &mut id), ES_E_INVALID_ARG);
        d.flags = 0;
        d.struct_size = 4;
        assert_eq!(es_container_add(c, &d, &mut id), ES_E_ABI);
        assert_eq!(add(c, &sram, Some("sram")), Err(ES_E_INVALID_ARG), "format not in this build");

        assert_eq!(es_container_remove(c, sid), ES_OK);
        assert_eq!(es_container_remove(c, sid), ES_E_NOT_FOUND);
        assert_eq!(es_container_flushed(c, sid), ES_E_NOT_FOUND);
    }
}

/// ES_CF_REPLACE moves a card to another container on this device (a file card replaced by a
/// folder card here; for Dolphin, a slot switched between a raw card and a GCI folder).
#[test]
fn replace_moves_a_card_to_another_container() {
    let dev = Dev::new("windows", None, ES_POLICY_ASK);
    let folder = dev.card.with_file_name("Mcd001");
    std::fs::create_dir(&folder).unwrap();
    std::fs::write(folder.join("_pcsx2_superblock"), b"").unwrap();
    let loc = cstr(folder.to_str().unwrap());
    let card = cstr("Mcd001");
    let desc = |flags: u32| es_container_desc {
        struct_size: std::mem::size_of::<es_container_desc>() as u32,
        format: std::ptr::null(),
        location: loc.as_ptr(),
        platform: std::ptr::null(),
        card: card.as_ptr(),
        title_scope: std::ptr::null(),
        flags,
    };
    let mut id = 0;
    assert_eq!(unsafe { es_container_add(dev.c, &desc(0), &mut id) }, ES_E_INVALID_ARG, "the card has a container");

    // Not while a session is open: the container it would replace may be the session's.
    let (status, info) = dev.begin(0);
    assert!(status == ES_OK || status == ES_OFFLINE, "{status}");
    assert_eq!(unsafe { es_container_add(dev.c, &desc(ES_CF_REPLACE), &mut id) }, ES_E_BUSY);
    unsafe { es_session_end(dev.c, info.id, 0) };

    assert_eq!(unsafe { es_container_add(dev.c, &desc(ES_CF_REPLACE), &mut id) }, ES_OK);
    assert!(dev.card.exists(), "the old container's file is left alone");
    // The card is the folder's now: the file can't have it back without the flag.
    assert_eq!(add(dev.c, &dev.card, None), Err(ES_E_INVALID_ARG));
    // Adding the new location again is idempotent, with or without the flag.
    let mut again = 0;
    assert_eq!(unsafe { es_container_add(dev.c, &desc(ES_CF_REPLACE), &mut again) }, ES_OK);
    assert_eq!(again, id);
}

#[test]
fn session_begin_applies_before_the_emulator_opens_the_card() {
    let remote = memory();
    let win = Dev::new("windows", Some(remote.clone()), ES_POLICY_ASK);
    let mac = Dev::new("mac", Some(remote.clone()), ES_POLICY_ASK);

    // Windows saves GTA III; the emulator flushes the card and the app goes to the background.
    win.put(GTA, b"gta v1");
    assert_eq!(unsafe { es_container_flushed(win.c, win.id) }, ES_OK);
    assert_eq!(win.suspend(), ES_OK);
    let evs = win.events();
    let up = evs.iter().find(|e| e.kind == ES_EV_UNIT_UPLOADED).expect("uploaded");
    assert_eq!(up.unit_name.as_deref(), Some(GTA));
    assert_eq!(up.title_key.as_deref(), Some("ps2:SLUS-20062"));

    // The Mac starts the game: the save is on its card before the emulator opens it.
    let (status, info) = mac.begin(5000);
    assert_eq!(status, ES_OK);
    assert_eq!(info.units_applied, 1);
    assert_eq!(info.remote_reached, 1);
    assert_eq!(info.conflicts, 0);
    assert_eq!(info.other_device_active[0], 0);
    assert_eq!(mac.saves().get(GTA).map(Vec::as_slice), Some(&b"gta v1"[..]));
    let evs = mac.events();
    let applied = evs.iter().find(|e| e.kind == ES_EV_UNIT_APPLIED).expect("applied");
    assert_eq!(applied.container, mac.id);
    assert_eq!(applied.unit_name.as_deref(), Some(GTA));

    // A second session on the same card is refused while this one is open.
    assert_eq!(mac.begin(1000).0, ES_E_BUSY);
    assert_eq!(unsafe { es_container_remove(mac.c, mac.id) }, ES_E_BUSY);
    assert_eq!(unsafe { es_card_set_mode(mac.c, cstr("Mcd001").as_ptr(), ES_CARD_WHOLE_FILE) }, ES_E_BUSY);

    // While the Mac plays, Windows saves Ratchet & Clank. The Mac's card is not touched...
    win.put(RAC, b"rac v1");
    unsafe { es_container_flushed(win.c, win.id) };
    assert_eq!(win.suspend(), ES_OK);
    let evs = mac.sync();
    let blocked = evs.iter().find(|e| e.kind == ES_EV_APPLY_BLOCKED).expect("blocked");
    assert_eq!(blocked.status, ES_E_BUSY);
    assert_eq!(blocked.unit_name.as_deref(), Some(RAC));
    assert!(!mac.saves().contains_key(RAC));
    // ...and a blocked unit is reported once, not every cycle.
    assert!(!mac.sync().iter().any(|e| e.kind == ES_EV_APPLY_BLOCKED));

    // The Mac saves GTA and quits: the save uploads, and R&C lands on the card.
    mac.put(GTA, b"gta v2 (mac)");
    assert_eq!(unsafe { es_session_end(mac.c, info.id, 60_000) }, ES_PENDING);
    assert_eq!(unsafe { es_session_end(mac.c, info.id, 0) }, ES_E_NOT_FOUND);
    let evs = mac.wait_for("session end", |e| e.kind == ES_EV_SYNC_FINISHED);
    assert!(evs.iter().any(|e| e.kind == ES_EV_UNIT_UPLOADED && e.unit_name.as_deref() == Some(GTA)));
    assert!(evs.iter().any(|e| e.kind == ES_EV_UNIT_APPLIED && e.unit_name.as_deref() == Some(RAC)));
    assert_eq!(mac.saves().get(RAC).map(Vec::as_slice), Some(&b"rac v1"[..]));
    win.sync();
    assert_eq!(win.saves().get(GTA).map(Vec::as_slice), Some(&b"gta v2 (mac)"[..]));
}

/// A remote whose LIST waits until the test lets it go: a pull slower than the session's
/// deadline.
struct SlowRemote {
    inner: Arc<MemoryRemote>,
    gate: Mutex<bool>,
    cv: Condvar,
    stalled: AtomicUsize,
}

impl SlowRemote {
    fn new(inner: Arc<MemoryRemote>) -> Arc<Self> {
        Arc::new(Self { inner, gate: Mutex::new(true), cv: Condvar::new(), stalled: AtomicUsize::new(0) })
    }

    fn set_open(&self, open: bool) {
        *self.gate.lock().unwrap() = open;
        self.cv.notify_all();
    }

    fn pass(&self) {
        let mut open = self.gate.lock().unwrap();
        if !*open {
            self.stalled.fetch_add(1, Ordering::SeqCst);
        }
        while !*open {
            open = self.cv.wait(open).unwrap();
        }
    }
}

impl Remote for SlowRemote {
    fn describe(&self) -> String {
        self.inner.describe()
    }
    fn put(&self, key: &str, data: &[u8], if_none_match: bool) -> emusync_remote::Result<PutOutcome> {
        self.inner.put(key, data, if_none_match)
    }
    fn get(&self, key: &str) -> emusync_remote::Result<Option<Vec<u8>>> {
        self.inner.get(key)
    }
    fn head(&self, key: &str) -> emusync_remote::Result<bool> {
        self.inner.head(key)
    }
    fn list(&self, prefix: &str, start_after: Option<&str>) -> emusync_remote::Result<Vec<String>> {
        self.inner.list(prefix, start_after)
    }
    fn list_dirs(&self, prefix: &str) -> emusync_remote::Result<Vec<String>> {
        self.pass();
        self.inner.list_dirs(prefix)
    }
}

#[test]
fn a_pull_that_misses_the_deadline_never_writes_to_the_open_card() {
    let remote = memory();
    let win = Dev::new("windows", Some(remote.clone()), ES_POLICY_ASK);
    let slow = SlowRemote::new(remote.clone());
    let mac = Dev::new("mac", Some(slow.clone()), ES_POLICY_ASK);
    win.put(GTA, b"gta v1");
    unsafe { es_container_flushed(win.c, win.id) };
    assert_eq!(win.suspend(), ES_OK);

    slow.set_open(false);
    let started = Instant::now();
    let (status, info) = mac.begin(200);
    assert_eq!(status, ES_TIMEOUT);
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    assert_eq!(info.remote_reached, 0);
    assert_ne!(info.id, 0);
    // The emulator now has the card open, and writes to it. The pull goes through after all...
    let deadline = Instant::now() + Duration::from_secs(10);
    while slow.stalled.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    mac.put(RAC, b"rac, being written");
    slow.set_open(true);
    let evs = mac.wait_for("pull", |e| e.kind == ES_EV_SYNC_FINISHED);
    // ...but its result waits in the inbox,
    let blocked = evs.iter().find(|e| e.kind == ES_EV_APPLY_BLOCKED).expect("blocked");
    assert_eq!(blocked.status, ES_E_BUSY);
    assert!(!mac.saves().contains_key(GTA));
    // and it didn't read the card the emulator is writing: no version of the new save yet.
    let rac = cstr(RAC);
    let rc = unsafe { es_unit_history(mac.c, rac.as_ptr(), Some(collect_version), std::ptr::null_mut()) };
    assert_eq!(rc, ES_E_NOT_FOUND);
    assert_eq!(unsafe { es_session_end(mac.c, info.id, 0) }, ES_PENDING);
    mac.wait_for("session end", |e| e.kind == ES_EV_UNIT_APPLIED);
    assert_eq!(mac.saves().get(GTA).map(Vec::as_slice), Some(&b"gta v1"[..]));
    // The final snapshot picked the new save up.
    let mut versions: Vec<(String, String)> = Vec::new();
    let user = &mut versions as *mut _ as *mut c_void;
    assert_eq!(unsafe { es_unit_history(mac.c, rac.as_ptr(), Some(collect_version), user) }, ES_OK);
    assert_eq!(versions.len(), 1);
}

#[test]
fn offline_session_still_starts() {
    let dev = Dev::new("train", None, ES_POLICY_ASK);
    let (status, info) = dev.begin(0);
    assert_eq!(status, ES_OFFLINE);
    assert_eq!(info.remote_reached, 0);
    let evs = dev.events();
    let fin = evs.iter().find(|e| e.kind == ES_EV_SYNC_FINISHED).unwrap();
    assert_eq!(fin.status, ES_OFFLINE);
    assert_eq!(fin.message, None);
    assert_eq!(unsafe { es_session_end(dev.c, info.id, 0) }, ES_PENDING);
    // Suspend with nothing to push: snapshot only.
    assert_eq!(dev.suspend(), ES_OFFLINE);
}

#[test]
fn conflicts_are_reported_and_resolved() {
    let remote = memory();
    let win = Dev::new("windows", Some(remote.clone()), ES_POLICY_ASK);
    let mac = Dev::new("mac", Some(remote.clone()), ES_POLICY_ASK);
    win.put(GTA, b"gta v1");
    win.sync();
    mac.sync();
    assert_eq!(mac.saves().get(GTA).map(Vec::as_slice), Some(&b"gta v1"[..]));

    // Both play offline, then come back.
    win.put(GTA, b"gta windows");
    win.sync();
    mac.put(GTA, b"gta mac");
    let evs = mac.sync();
    let ev = evs.iter().find(|e| e.kind == ES_EV_CONFLICT).expect("conflict");
    assert_eq!(ev.unit_name.as_deref(), Some(GTA));
    assert_eq!(ev.container, mac.id);
    let id = ev.conflict_id;

    // A session for the game reports it.
    let (status, info) = mac.begin(5000);
    assert_eq!(status, ES_CONFLICT);
    assert_eq!(info.conflicts, 1);
    mac.events();
    assert_eq!(unsafe { es_session_end(mac.c, info.id, 0) }, ES_PENDING);
    mac.wait_for("session end", |e| e.kind == ES_EV_SYNC_FINISHED);

    let mut ci: es_conflict_info = unsafe { std::mem::zeroed() };
    ci.struct_size = std::mem::size_of::<es_conflict_info>() as u32;
    unsafe {
        assert_eq!(es_conflict_get(mac.c, id, &mut ci), ES_OK);
        assert_eq!(read_buf(&ci.unit_name).unwrap(), GTA);
        assert_eq!(read_buf(&ci.unit_id).unwrap(), format!("ps2:Mcd001:{GTA}"));
        assert_eq!(read_buf(&ci.title_key).unwrap(), "ps2:SLUS-20062");
        assert_eq!(ci.container, mac.id);
        assert_eq!(ci.remote_count, 1);
        assert_eq!(read_buf(&ci.local.device_name).unwrap(), "mac");
        assert_eq!(read_buf(&ci.remote[0].device_name).unwrap(), "windows");
        assert!(read_buf(&ci.remote[0].version_id).unwrap().starts_with("b3:"));
        assert_eq!(es_conflict_resolve(mac.c, id, ES_RESOLVE_TAKE_REMOTE, 1), ES_E_INVALID_ARG);
        assert_eq!(es_conflict_resolve(mac.c, id, 9, 0), ES_E_INVALID_ARG);
        assert_eq!(es_conflict_resolve(mac.c, id, ES_RESOLVE_TAKE_REMOTE, 0), ES_PENDING);
    }
    let evs = mac.wait_for("resolve", |e| e.kind == ES_EV_SYNC_FINISHED);
    assert!(evs.iter().any(|e| e.kind == ES_EV_UNIT_APPLIED), "{evs:#?}");
    assert_eq!(mac.saves().get(GTA).map(Vec::as_slice), Some(&b"gta windows"[..]));
    assert_eq!(unsafe { es_conflict_get(mac.c, id, &mut ci) }, ES_E_NOT_FOUND);
    ci.struct_size = 16;
    assert_eq!(unsafe { es_conflict_get(mac.c, id, &mut ci) }, ES_E_ABI);
}

unsafe extern "C" fn collect_version(user: *mut c_void, v: *const es_version_info) {
    let out = &mut *(user as *mut Vec<(String, String)>);
    out.push((read_buf(&(*v).version_id).unwrap(), read_buf(&(*v).device_name).unwrap()));
}

#[test]
fn history_and_restore() {
    let remote = memory();
    let dev = Dev::new("pc", Some(remote), ES_POLICY_ASK);
    dev.put(GTA, b"chapter 1");
    dev.sync();
    dev.put(GTA, b"chapter 2");
    dev.sync();
    let mut versions: Vec<(String, String)> = Vec::new();
    let unit = cstr(GTA);
    unsafe {
        let rc = es_unit_history(dev.c, unit.as_ptr(), Some(collect_version), &mut versions as *mut _ as *mut c_void);
        assert_eq!(rc, ES_OK);
        assert_eq!(versions.len(), 2);
        assert!(versions.iter().all(|(_, d)| d == "pc"));
        assert_eq!(
            es_unit_history(dev.c, cstr("BANOPE").as_ptr(), Some(collect_version), std::ptr::null_mut()),
            ES_E_NOT_FOUND
        );
        // Newest first: restore the older one.
        let old = cstr(&versions[1].0);
        assert_eq!(es_unit_restore(dev.c, unit.as_ptr(), old.as_ptr()), ES_PENDING);
        assert_eq!(es_unit_restore(dev.c, unit.as_ptr(), cstr("b3:ffff").as_ptr()), ES_E_NOT_FOUND);
    }
    dev.wait_for("restore", |e| e.kind == ES_EV_UNIT_APPLIED);
    assert_eq!(dev.saves().get(GTA).map(Vec::as_slice), Some(&b"chapter 1"[..]));
}

#[test]
fn touched_debounces_then_snapshots_and_pushes() {
    let remote = memory();
    let dev = Dev::new("pc", Some(remote.clone()), ES_POLICY_ASK);
    dev.put(GTA, b"autosave");
    unsafe {
        es_container_touched(dev.c, dev.id);
        es_container_touched(dev.c, dev.id);
    }
    let started = Instant::now();
    let evs = dev.wait_for("upload", |e| e.kind == ES_EV_UNIT_UPLOADED);
    assert!(started.elapsed() >= Duration::from_millis(1500), "debounced: {:?}", started.elapsed());
    assert_eq!(evs.iter().filter(|e| e.kind == ES_EV_UNIT_UPLOADED).count(), 1);
    assert!(remote.keys().iter().any(|k| k.contains("/log/")));
}

static LOG_LINES: AtomicUsize = AtomicUsize::new(0);
static WAKES: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn count_log(_user: *mut c_void, level: c_int, target: *const c_char, msg: *const c_char) {
    assert!((0..=4).contains(&level));
    assert!(!CStr::from_ptr(target).to_bytes().is_empty());
    let _ = CStr::from_ptr(msg).to_str().unwrap();
    LOG_LINES.fetch_add(1, Ordering::SeqCst);
}

unsafe extern "C" fn count_wake(user: *mut c_void) {
    assert_eq!(user as usize, 42);
    WAKES.fetch_add(1, Ordering::SeqCst);
}

#[test]
fn logging_notifier_and_event_lifetime() {
    let dir = tempfile::tempdir().unwrap();
    let data = cstr(dir.path().to_str().unwrap());
    let mut cfg = config_init();
    cfg.data_dir = data.as_ptr();
    cfg.log = Some(count_log);
    cfg.log_level = ES_LOG_TRACE;
    let mut c = std::ptr::null_mut();
    unsafe {
        assert_eq!(es_client_create(&cfg, &mut c), ES_OK);
        es_event_set_notifier(c, Some(count_wake), 42 as *mut c_void);
        // An invalid argument is logged from the calling thread.
        let before = LOG_LINES.load(Ordering::SeqCst);
        assert_eq!(es_state_publish(c, std::ptr::null()), ES_E_INVALID_ARG);
        assert!(LOG_LINES.load(Ordering::SeqCst) > before);
        assert_eq!(es_sync_now(c), ES_PENDING);
        let deadline = Instant::now() + Duration::from_secs(10);
        while WAKES.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline, "wake was never called");
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut ev = event_init();
        ev.struct_size = 8;
        assert_eq!(es_event_poll(c, &mut ev), ES_E_ABI);
        ev = event_init();
        let deadline = Instant::now() + Duration::from_secs(10);
        while es_event_poll(c, &mut ev) != 1 {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(ev.type_, ES_EV_SYNC_STARTED);
        assert!(!ev._internal.is_null());
        es_event_release(c, &mut ev);
        assert!(ev._internal.is_null() && ev.message.is_null());
        es_event_release(c, &mut ev); // twice is harmless
        es_event_set_notifier(c, None, std::ptr::null_mut());
        es_client_destroy(c, 5000);
    }
    // Nothing is logged once destroy has returned.
    let after = LOG_LINES.load(Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(LOG_LINES.load(Ordering::SeqCst), after);
}

static CRED_CALLS: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn s3_keys(user: *mut c_void, uri: *const c_char, out: *mut es_credentials) -> c_int {
    assert_eq!(user as usize, 7);
    assert!(CStr::from_ptr(uri).to_str().unwrap().starts_with("s3://"));
    let out = &mut *out;
    assert_eq!(out.struct_size as usize, std::mem::size_of::<es_credentials>());
    assert_eq!(out.access_key_id[0], 0, "zeroed by the library");
    out.kind = ES_CRED_S3;
    copy_str(&mut out.access_key_id, "AKIDEXAMPLE");
    copy_str(&mut out.secret_access_key, "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY");
    CRED_CALLS.fetch_add(1, Ordering::SeqCst);
    0
}

unsafe extern "C" fn needs_sign_in(_: *mut c_void, _: *const c_char, _: *mut es_credentials) -> c_int {
    ES_E_AUTH
}

#[test]
fn remote_add_uses_the_credential_provider() {
    let dev = Dev::new("pc", None, ES_POLICY_ASK);
    let creds = es_credential_provider { user: 7 as *mut c_void, get: Some(s3_keys) };
    // Nothing listens on port 1: unreachable, so the remote is added for later.
    let uri = cstr("s3://saves/emusync/v1?endpoint=http://127.0.0.1:1&path_style=1&region=us-east-1");
    unsafe {
        assert_eq!(es_remote_add(dev.c, uri.as_ptr(), &creds), ES_OFFLINE);
        assert!(CRED_CALLS.load(Ordering::SeqCst) >= 1);
        let failing = es_credential_provider { user: std::ptr::null_mut(), get: Some(needs_sign_in) };
        assert_eq!(es_remote_add(dev.c, uri.as_ptr(), &failing), ES_E_AUTH);
        // The unreachable remote counts as added: another one is refused.
        let other = cstr("s3://elsewhere/emusync/v1?endpoint=http://127.0.0.1:1&path_style=1&region=us-east-1");
        assert_eq!(es_remote_add(dev.c, other.as_ptr(), &creds), ES_E_INVALID_ARG);
        let none = es_credential_provider { user: std::ptr::null_mut(), get: None };
        assert_eq!(es_remote_add(dev.c, uri.as_ptr(), &none), ES_E_INVALID_ARG);
        assert_eq!(es_remote_add(dev.c, cstr("webdav://nas/saves").as_ptr(), &creds), ES_E_INVALID_ARG);
        assert_eq!(es_remote_add(dev.c, cstr("s3://").as_ptr(), &creds), ES_E_INVALID_ARG);
    }
}

/// An ES_CRED_OAUTH_REFRESH answer with an empty token.
unsafe extern "C" fn empty_refresh_token(_: *mut c_void, _: *const c_char, out: *mut es_credentials) -> c_int {
    (*out).kind = ES_CRED_OAUTH_REFRESH;
    0
}

/// S3 keys, whatever the remote.
unsafe extern "C" fn any_s3_keys(_: *mut c_void, _: *const c_char, out: *mut es_credentials) -> c_int {
    let out = &mut *out;
    out.kind = ES_CRED_S3;
    copy_str(&mut out.access_key_id, "AKIDEXAMPLE");
    copy_str(&mut out.secret_access_key, "secret");
    0
}

#[test]
fn dropbox_remotes_take_oauth_credentials() {
    let dev = Dev::new("pc", None, ES_POLICY_ASK);
    let uri = cstr("dropbox:/saves?app_key=testkey");
    unsafe {
        // S3 keys are refused before any request, and so is an empty token.
        let s3 = es_credential_provider { user: std::ptr::null_mut(), get: Some(any_s3_keys) };
        assert_eq!(es_remote_add(dev.c, uri.as_ptr(), &s3), ES_E_AUTH);
        let empty = es_credential_provider { user: std::ptr::null_mut(), get: Some(empty_refresh_token) };
        assert_eq!(es_remote_add(dev.c, uri.as_ptr(), &empty), ES_E_AUTH);
        let needs = es_credential_provider { user: std::ptr::null_mut(), get: Some(needs_sign_in) };
        assert_eq!(es_remote_add(dev.c, uri.as_ptr(), &needs), ES_E_AUTH);
        assert_eq!(es_remote_add(dev.c, cstr("dropbox:/a//b?app_key=k").as_ptr(), &needs), ES_E_INVALID_ARG);
    }
}

#[test]
fn oauth_sign_in_without_a_client() {
    let mut url = [1 as c_char; 2048];
    let mut pending = [1 as c_char; 2048];
    let uri = cstr("dropbox:/saves?app_key=testkey");
    unsafe {
        assert_eq!(
            es_oauth_start(
                uri.as_ptr(),
                cstr("").as_ptr(),
                url.as_mut_ptr(),
                url.len(),
                pending.as_mut_ptr(),
                pending.len()
            ),
            ES_OK
        );
        let url_s = CStr::from_ptr(url.as_ptr()).to_str().unwrap().to_string();
        assert!(url_s.starts_with("https://www.dropbox.com/oauth2/authorize?client_id=testkey&"), "{url_s}");
        assert!(CStr::from_ptr(pending.as_ptr()).to_bytes().len() > 40);

        // Buffers too small: both outputs are "".
        let mut small = [1 as c_char; 8];
        assert_eq!(
            es_oauth_start(
                uri.as_ptr(),
                std::ptr::null(),
                url.as_mut_ptr(),
                url.len(),
                small.as_mut_ptr(),
                small.len()
            ),
            ES_E_NO_SPACE
        );
        assert_eq!((url[0], small[0]), (0, 0));
        // Not Dropbox, NULL arguments.
        let mut p2 = [0 as c_char; 2048];
        let s3 = cstr("s3://b/p");
        assert_eq!(
            es_oauth_start(s3.as_ptr(), std::ptr::null(), url.as_mut_ptr(), url.len(), p2.as_mut_ptr(), p2.len()),
            ES_E_INVALID_ARG
        );
        assert_eq!(
            es_oauth_start(std::ptr::null(), std::ptr::null(), url.as_mut_ptr(), url.len(), p2.as_mut_ptr(), p2.len()),
            ES_E_INVALID_ARG
        );

        // A redirect from another sign-in is refused before any request.
        let mut token = [1 as c_char; 1024];
        let mut account = [1 as c_char; 1024];
        let wrong = cstr("http://127.0.0.1:53682/dropbox?code=C&state=not-this-one");
        let finish = |pending: *const c_char, input: *const c_char, token: &mut [c_char], account: &mut [c_char]| {
            es_oauth_finish(pending, input, token.as_mut_ptr(), token.len(), account.as_mut_ptr(), account.len())
        };
        assert_eq!(finish(pending.as_ptr(), wrong.as_ptr(), &mut token, &mut account), ES_E_AUTH);
        assert_eq!((token[0], account[0]), (0, 0));
        let declined = cstr("http://127.0.0.1:53682/dropbox?error=access_denied");
        assert_eq!(finish(pending.as_ptr(), declined.as_ptr(), &mut token, &mut account), ES_E_AUTH);
        assert_eq!(finish(cstr("garbage").as_ptr(), wrong.as_ptr(), &mut token, &mut account), ES_E_INVALID_ARG);
        assert_eq!(finish(pending.as_ptr(), std::ptr::null(), &mut token, &mut account), ES_E_INVALID_ARG);

        // Waiting needs a loopback redirect; nobody comes in time.
        let mut redirect = [1 as c_char; 1024];
        assert_eq!(
            es_oauth_wait_redirect(pending.as_ptr(), 50, redirect.as_mut_ptr(), redirect.len()),
            ES_E_INVALID_ARG
        );
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let loopback = cstr(&format!("http://127.0.0.1:{port}/dropbox"));
        assert_eq!(
            es_oauth_start(uri.as_ptr(), loopback.as_ptr(), url.as_mut_ptr(), url.len(), p2.as_mut_ptr(), p2.len()),
            ES_OK
        );
        assert_eq!(es_oauth_wait_redirect(p2.as_ptr(), 50, redirect.as_mut_ptr(), redirect.len()), ES_TIMEOUT);
        assert_eq!(redirect[0], 0);
    }
}

#[test]
fn unimplemented_features_are_refused() {
    let dev = Dev::new("pc", None, ES_POLICY_ASK);
    let mut url = [1 as c_char; 16];
    unsafe {
        assert_eq!(
            es_oauth_begin(dev.c, std::ptr::null(), std::ptr::null(), url.as_mut_ptr(), url.len()),
            ES_E_INVALID_ARG
        );
        assert_eq!(url[0], 0);
        assert_eq!(es_oauth_complete(dev.c, std::ptr::null(), std::ptr::null()), ES_E_INVALID_ARG);
        assert_eq!(es_state_publish(dev.c, std::ptr::null()), ES_E_INVALID_ARG);
        assert_eq!(es_state_fetch(dev.c, std::ptr::null(), std::ptr::null(), 0), ES_E_INVALID_ARG);
        let mut si: es_state_info = std::mem::zeroed();
        assert_eq!(
            es_state_query_latest(dev.c, std::ptr::null(), std::ptr::null(), std::ptr::null(), &mut si),
            ES_E_INVALID_ARG
        );
    }
}

#[test]
fn version_and_status_strings() {
    assert_eq!(crate::es_abi_version(), 1);
    let v = unsafe { CStr::from_ptr(crate::es_version_string()) }.to_str().unwrap();
    assert!(v.starts_with(env!("CARGO_PKG_VERSION")));
    let s = unsafe { CStr::from_ptr(crate::es_status_str(crate::EsStatus::NoSpace as i32)) };
    assert_eq!(s.to_str().unwrap(), "no space in container");
    let s = unsafe { CStr::from_ptr(crate::es_status_str(ES_E_LOCKED)) };
    assert_eq!(s.to_str().unwrap(), "repository locked (passphrase needed)");
    let s = unsafe { CStr::from_ptr(crate::es_status_str(crate::EsStatus::Passphrase as i32)) };
    assert_eq!(s.to_str().unwrap(), "wrong passphrase");
    let s = unsafe { CStr::from_ptr(crate::es_status_str(12345)) };
    assert_eq!(s.to_str().unwrap(), "unknown status");
}

#[test]
fn fixed_buffers_truncate_at_char_boundaries() {
    let mut buf = [1 as c_char; 5];
    copy_str(&mut buf, "abcdef");
    assert_eq!(read_buf(&buf).unwrap(), "abcd");
    copy_str(&mut buf, "aé€");
    assert_eq!(read_buf(&buf).unwrap(), "aé");
    assert_eq!(read_buf(&[1, 2]), None, "no NUL");
}

#[test]
fn encrypted_repository_between_two_devices() {
    let remote = memory();
    let pass = cstr("correct horse battery staple");
    let a = Dev::new("pc", None, ES_POLICY_ASK);
    unsafe { assert_eq!(es_repo_create_encrypted(a.c, pass.as_ptr()), ES_OK) };
    assert_eq!(attach(a.c, remote.clone()), ES_OK);
    assert!(contains(&remote.raw_get("repo.json").unwrap(), b"\"format\":2"));
    a.put(GTA, b"gta progress");
    a.sync();
    // The provider sees neither save data nor log records.
    let objects: Vec<String> =
        remote.keys().into_iter().filter(|k| k != "repo.json" && !k.starts_with("_probe/")).collect();
    assert!(objects.iter().any(|k| k.ends_with(".ndjson")));
    for k in &objects {
        let body = remote.raw_get(k).unwrap();
        for marker in [&b"gta progress"[..], GTA.as_bytes(), b"\"op\""] {
            assert!(!contains(&body, marker), "{k} holds {}", String::from_utf8_lossy(marker));
        }
    }

    // A second device without the passphrase: the remote is added, locked.
    let mut b = Dev::new("phone", None, ES_POLICY_ASK);
    assert_eq!(attach(b.c, remote.clone()), ES_E_LOCKED);
    let evs = b.sync();
    let finished = evs.iter().find(|e| e.kind == ES_EV_SYNC_FINISHED).unwrap();
    assert_eq!(finished.status, ES_E_LOCKED);
    assert!(finished.message.as_deref().unwrap_or_default().contains("passphrase"), "{finished:?}");
    assert!(evs.iter().any(|e| e.kind == ES_EV_ERROR && e.status == ES_E_LOCKED));
    unsafe {
        assert_eq!(es_repo_unlock(b.c, cstr("wrong").as_ptr()), ES_E_PASSPHRASE);
        assert_eq!(es_repo_unlock(b.c, pass.as_ptr()), ES_OK);
    }
    let evs = b.sync();
    assert!(evs.iter().any(|e| e.kind == ES_EV_SYNC_FINISHED && e.status == ES_OK), "{evs:#?}");
    assert_eq!(b.saves().get(GTA).map(Vec::as_slice), Some(&b"gta progress"[..]));

    // Restarted, the phone needs no passphrase: the key was kept in its data directory.
    b.reopen();
    assert_eq!(attach(b.c, remote.clone()), ES_OK);
    b.put(RAC, b"rac progress");
    b.sync();
    a.sync();
    assert_eq!(a.saves().get(RAC).map(Vec::as_slice), Some(&b"rac progress"[..]));
}

#[test]
fn encryption_errors() {
    unsafe {
        let dev = Dev::new("pc", None, ES_POLICY_ASK);
        assert_eq!(es_repo_create_encrypted(dev.c, std::ptr::null()), ES_E_INVALID_ARG);
        assert_eq!(es_repo_create_encrypted(dev.c, cstr("").as_ptr()), ES_E_INVALID_ARG);
        assert_eq!(es_repo_unlock(std::ptr::null_mut(), cstr("pw").as_ptr()), ES_E_INVALID_ARG);
        // No remote yet.
        assert_eq!(es_repo_unlock(dev.c, cstr("pw").as_ptr()), ES_E_INVALID_ARG);

        // An unencrypted repository: nothing to unlock, and it can't become encrypted.
        let plain = memory();
        let p = Dev::new("pc", Some(plain.clone()), ES_POLICY_ASK);
        assert_eq!(es_repo_unlock(p.c, cstr("pw").as_ptr()), ES_E_INVALID_ARG);
        let q = Dev::new("phone", None, ES_POLICY_ASK);
        assert_eq!(es_repo_create_encrypted(q.c, cstr("pw").as_ptr()), ES_OK);
        assert_eq!(attach(q.c, plain.clone()), ES_E_INCOMPATIBLE);
        assert!(contains(&plain.raw_get("repo.json").unwrap(), b"\"format\":1"));
        // The passphrase applied to that es_remote_add only.
        assert_eq!(attach(q.c, plain.clone()), ES_OK);

        // The wrong passphrase for an existing encrypted repository: not added.
        let enc = memory();
        let r = Dev::new("a", None, ES_POLICY_ASK);
        assert_eq!(es_repo_create_encrypted(r.c, cstr("right").as_ptr()), ES_OK);
        assert_eq!(attach(r.c, enc.clone()), ES_OK);
        let s = Dev::new("b", None, ES_POLICY_ASK);
        assert_eq!(es_repo_create_encrypted(s.c, cstr("left").as_ptr()), ES_OK);
        assert_eq!(attach(s.c, enc.clone()), ES_E_PASSPHRASE);
        assert!((*s.c).shared.cache().remote.is_none());
        // With the right one it is added and unlocked.
        assert_eq!(es_repo_create_encrypted(s.c, cstr("right").as_ptr()), ES_OK);
        assert_eq!(attach(s.c, enc.clone()), ES_OK);

        // Binding fails after the setup succeeded (the key can't be stored): es_remote_add says
        // so, rather than ES_OK, and the remote is not added.
        let t = Dev::new("c", None, ES_POLICY_ASK);
        let keys = t._dir.path().join("data").join("keys");
        std::fs::write(&keys, b"not a directory").unwrap();
        assert_eq!(es_repo_create_encrypted(t.c, cstr("right").as_ptr()), ES_OK);
        assert_eq!(attach(t.c, enc.clone()), ES_E_IO);
        assert!((*t.c).shared.cache().remote.is_none());
        std::fs::remove_file(&keys).unwrap();
        assert_eq!(es_repo_create_encrypted(t.c, cstr("right").as_ptr()), ES_OK);
        assert_eq!(attach(t.c, enc.clone()), ES_OK);
        assert!((*t.c).shared.cache().remote.is_some());
    }
}

#[test]
fn secret_calls_validate_their_arguments() {
    // None of these reach a credential store: they fail on the arguments first.
    let (svc, name, val) = (cstr("emusync-test"), cstr("name"), cstr("value"));
    let bad = cstr("../name");
    let null = std::ptr::null();
    unsafe {
        assert_eq!(es_secret_set(null, name.as_ptr(), val.as_ptr(), null), ES_E_INVALID_ARG);
        assert_eq!(es_secret_set(svc.as_ptr(), null, val.as_ptr(), null), ES_E_INVALID_ARG);
        assert_eq!(es_secret_set(svc.as_ptr(), name.as_ptr(), null, null), ES_E_INVALID_ARG);
        assert_eq!(es_secret_set(svc.as_ptr(), bad.as_ptr(), val.as_ptr(), null), ES_E_INVALID_ARG);
        assert_eq!(es_secret_set(svc.as_ptr(), name.as_ptr(), cstr("").as_ptr(), null), ES_E_INVALID_ARG);
        assert_eq!(es_secret_delete(svc.as_ptr(), bad.as_ptr(), null), ES_E_INVALID_ARG);

        let mut buf = [b'x' as c_char; 8];
        let mut len = 99usize;
        assert_eq!(
            es_secret_get(svc.as_ptr(), bad.as_ptr(), null, buf.as_mut_ptr(), buf.len(), &mut len),
            ES_E_INVALID_ARG
        );
        assert_eq!((buf[0], len), (0, 0), "out and out_len are cleared on errors");
        assert_eq!(
            es_secret_get(svc.as_ptr(), name.as_ptr(), null, std::ptr::null_mut(), 8, std::ptr::null_mut()),
            ES_E_INVALID_ARG,
            "out NULL with cap > 0"
        );
    }
}

/// The C side of the system-store round trip in emusync_core::secrets: like that one, it runs
/// only with EMUSYNC_TEST_OS_SECRETS=1, because it writes to this machine's credential store.
#[test]
fn secrets_round_trip_through_the_c_abi() {
    if std::env::var_os("EMUSYNC_TEST_OS_SECRETS").is_none() {
        eprintln!("skipped: set EMUSYNC_TEST_OS_SECRETS=1 to use this machine's credential store");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let fallback = cstr(dir.path().to_str().unwrap());
    let svc = cstr("emusync-test");
    let name = cstr(&format!("c-abi-{}", std::process::id()));
    let value = "AKIA-not-a-real-secret/0123456789";
    unsafe {
        let mut len = 0usize;
        assert_eq!(
            es_secret_get(svc.as_ptr(), name.as_ptr(), fallback.as_ptr(), std::ptr::null_mut(), 0, &mut len),
            ES_E_NOT_FOUND
        );
        assert_eq!(es_secret_set(svc.as_ptr(), name.as_ptr(), cstr(value).as_ptr(), fallback.as_ptr()), ES_OK);

        // The size query, then a buffer one byte short, then the right size.
        assert_eq!(
            es_secret_get(svc.as_ptr(), name.as_ptr(), fallback.as_ptr(), std::ptr::null_mut(), 0, &mut len),
            ES_E_NO_SPACE
        );
        assert_eq!(len, value.len() + 1);
        let mut buf = vec![b'x' as c_char; len];
        assert_eq!(
            es_secret_get(svc.as_ptr(), name.as_ptr(), fallback.as_ptr(), buf.as_mut_ptr(), len - 1, &mut len),
            ES_E_NO_SPACE
        );
        assert_eq!(buf[0], 0);
        assert_eq!(
            es_secret_get(svc.as_ptr(), name.as_ptr(), fallback.as_ptr(), buf.as_mut_ptr(), buf.len(), &mut len),
            ES_OK
        );
        assert_eq!(CStr::from_ptr(buf.as_ptr()).to_str().unwrap(), value);

        assert_eq!(es_secret_delete(svc.as_ptr(), name.as_ptr(), fallback.as_ptr()), ES_OK);
        assert_eq!(
            es_secret_get(svc.as_ptr(), name.as_ptr(), fallback.as_ptr(), buf.as_mut_ptr(), buf.len(), &mut len),
            ES_E_NOT_FOUND
        );
        assert_eq!(es_secret_delete(svc.as_ptr(), name.as_ptr(), fallback.as_ptr()), ES_OK);
    }
}

// ---- a host VFS (es_vfs) over emusync's in-memory one, as an Android host would wrap SAF ----

mod host {
    use super::*;
    use emusync_core::vfs::{MemoryVfs, Stat, Vfs};

    unsafe fn mem<'a>(user: *mut c_void) -> &'a MemoryVfs {
        &*(user as *const MemoryVfs)
    }

    unsafe fn s<'a>(p: *const c_char) -> &'a str {
        CStr::from_ptr(p).to_str().unwrap()
    }

    fn status(r: std::io::Result<()>) -> c_int {
        match r {
            Ok(()) => ES_OK,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => ES_E_NOT_FOUND,
            Err(_) => ES_E_IO,
        }
    }

    fn c_stat(st: &Stat) -> es_vfs_stat {
        es_vfs_stat { size: st.size, mtime_unix_ms: st.mtime_ns / 1_000_000, is_dir: st.is_dir as u8 }
    }

    unsafe fn put_str(s: Option<String>, out: *mut c_char, cap: usize) -> usize {
        let Some(s) = s else { return 0 };
        if s.len() < cap {
            std::ptr::copy_nonoverlapping(s.as_ptr().cast(), out, s.len());
            *out.add(s.len()) = 0;
        }
        s.len()
    }

    pub unsafe extern "C" fn stat(u: *mut c_void, p: *const c_char, out: *mut es_vfs_stat) -> c_int {
        match mem(u).stat(s(p)) {
            Ok(st) => {
                *out = c_stat(&st);
                ES_OK
            }
            Err(e) => status(Err(e)),
        }
    }

    pub unsafe extern "C" fn list(u: *mut c_void, p: *const c_char, cb: es_vfs_entry_fn, ctx: *mut c_void) -> c_int {
        match mem(u).list(s(p)) {
            Ok(entries) => {
                for (name, st) in entries {
                    let name = cstr(&name);
                    cb.unwrap()(ctx, name.as_ptr(), &c_stat(&st));
                }
                ES_OK
            }
            Err(e) => status(Err(e)),
        }
    }

    /// In pieces, as a host reading a file descriptor would hand them over.
    pub unsafe extern "C" fn read(u: *mut c_void, p: *const c_char, sink: es_vfs_sink_fn, ctx: *mut c_void) -> c_int {
        match mem(u).read(s(p)) {
            Ok(data) => {
                for piece in data.chunks(1000) {
                    sink.unwrap()(ctx, piece.as_ptr().cast(), piece.len());
                }
                ES_OK
            }
            Err(e) => status(Err(e)),
        }
    }

    pub unsafe extern "C" fn write(u: *mut c_void, p: *const c_char, data: *const c_void, len: usize) -> c_int {
        status(mem(u).write(s(p), std::slice::from_raw_parts(data.cast(), len)))
    }

    pub unsafe extern "C" fn mkdir(u: *mut c_void, p: *const c_char) -> c_int {
        status(mem(u).create_dir(s(p)))
    }

    pub unsafe extern "C" fn rename(u: *mut c_void, p: *const c_char, n: *const c_char) -> c_int {
        status(mem(u).rename(s(p), s(n)))
    }

    pub unsafe extern "C" fn remove(u: *mut c_void, p: *const c_char) -> c_int {
        status(mem(u).remove(s(p)))
    }

    pub unsafe extern "C" fn join(
        u: *mut c_void,
        d: *const c_char,
        n: *const c_char,
        out: *mut c_char,
        cap: usize,
    ) -> usize {
        put_str(Some(mem(u).join(s(d), s(n))), out, cap)
    }

    pub unsafe extern "C" fn parent(u: *mut c_void, p: *const c_char, out: *mut c_char, cap: usize) -> usize {
        put_str(mem(u).parent(s(p)), out, cap)
    }

    pub unsafe extern "C" fn name(u: *mut c_void, p: *const c_char, out: *mut c_char, cap: usize) -> usize {
        put_str(mem(u).name(s(p)), out, cap)
    }
}

/// An `es_vfs` whose `user` is a `MemoryVfs`; it can't set times (`set_mtime` is NULL).
fn host_vfs(user: *const emusync_core::vfs::MemoryVfs) -> es_vfs {
    es_vfs {
        struct_size: std::mem::size_of::<es_vfs>() as u32,
        user: user as *mut c_void,
        stat: Some(host::stat),
        list: Some(host::list),
        read: Some(host::read),
        write: Some(host::write),
        mkdir: Some(host::mkdir),
        rename: Some(host::rename),
        remove: Some(host::remove),
        set_mtime: None,
        join: Some(host::join),
        parent: Some(host::parent),
        name: Some(host::name),
    }
}

#[test]
fn a_host_vfs_holds_the_cards() {
    use emusync_core::vfs::{MemoryVfs, Vfs};
    let remote = memory();
    let pc = Dev::new("VEGA", Some(remote.clone()), ES_POLICY_ASK);
    pc.put(GTA, b"gta");
    pc.sync();

    // The phone keeps its card where only the host can reach it.
    let mem = Box::new(MemoryVfs::with_root("/tree/memcards"));
    mem.write("/tree/memcards/Mcd001.ps2", &blank_card(8192, true, T)).unwrap();
    let vfs = host_vfs(&*mem);
    let dir = tempfile::tempdir().unwrap();
    let data = cstr(dir.path().join("data").to_str().unwrap());
    let mut cfg = config_init();
    cfg.data_dir = data.as_ptr();
    cfg.vfs = &vfs;
    let mut c = std::ptr::null_mut();
    assert_eq!(unsafe { es_client_create(&cfg, &mut c) }, ES_OK);
    assert_eq!(attach(c, remote.clone()), ES_OK);
    let id = add(c, Path::new("/tree/memcards/Mcd001.ps2"), None).unwrap();
    let phone = Dev { c, card: PathBuf::new(), id, _dir: dir };

    phone.sync();
    let names = |img: &[u8]| -> Vec<String> { Ps2Card.list(img).unwrap().into_iter().map(|u| u.name).collect() };
    assert_eq!(names(&mem.read("/tree/memcards/Mcd001.ps2").unwrap()), [GTA]);
    let scratch: Vec<String> = mem.list("/tree/memcards").unwrap().into_iter().map(|(n, _)| n).collect();
    assert_eq!(scratch, ["Mcd001.ps2"], "nothing left behind");

    // The emulator saves on the phone; the PC gets it.
    let img = mem.read("/tree/memcards/Mcd001.ps2").unwrap();
    mem.write("/tree/memcards/Mcd001.ps2", &Ps2Card.apply(&img, &[ApplyOp::Put(save(RAC, b"rac"))]).unwrap()).unwrap();
    phone.sync();
    pc.sync();
    assert!(pc.saves().contains_key(RAC));
    drop(phone);
    drop(mem);
}

/// A host VFS that can hold the worker inside a call, and counts calls.
struct Gate {
    mem: emusync_core::vfs::MemoryVfs,
    hold: Mutex<bool>,
    cv: Condvar,
    inside: std::sync::atomic::AtomicBool,
    calls: AtomicUsize,
}

impl Gate {
    unsafe fn enter<'a>(u: *mut c_void) -> (&'a Gate, *mut c_void) {
        let g = &*(u as *const Gate);
        g.calls.fetch_add(1, Ordering::SeqCst);
        let mut hold = g.hold.lock().unwrap();
        while *hold {
            g.inside.store(true, Ordering::SeqCst);
            hold = g.cv.wait(hold).unwrap();
        }
        (g, &g.mem as *const _ as *mut c_void)
    }
}

mod gated {
    use super::*;
    macro_rules! wrap {
        ($name:ident($($arg:ident: $t:ty),*) -> $r:ty) => {
            pub unsafe extern "C" fn $name(u: *mut c_void, $($arg: $t),*) -> $r {
                let (_, m) = Gate::enter(u);
                host::$name(m, $($arg),*)
            }
        };
    }
    wrap!(stat(p: *const c_char, out: *mut es_vfs_stat) -> c_int);
    wrap!(list(p: *const c_char, cb: es_vfs_entry_fn, ctx: *mut c_void) -> c_int);
    wrap!(read(p: *const c_char, sink: es_vfs_sink_fn, ctx: *mut c_void) -> c_int);
    wrap!(write(p: *const c_char, data: *const c_void, len: usize) -> c_int);
    wrap!(mkdir(p: *const c_char) -> c_int);
    wrap!(rename(p: *const c_char, n: *const c_char) -> c_int);
    wrap!(remove(p: *const c_char) -> c_int);
    wrap!(join(d: *const c_char, n: *const c_char, out: *mut c_char, cap: usize) -> usize);
    wrap!(parent(p: *const c_char, out: *mut c_char, cap: usize) -> usize);
    wrap!(name(p: *const c_char, out: *mut c_char, cap: usize) -> usize);
}

/// `es_client_destroy` that times out while the worker is inside a VFS call waits for that
/// call, and no VFS function is called once it has returned, though the worker goes on.
#[test]
fn no_vfs_call_after_destroy_returns() {
    use emusync_core::vfs::{MemoryVfs, Vfs};
    let gate = Box::new(Gate {
        mem: MemoryVfs::with_root("/tree/memcards"),
        hold: Mutex::new(false),
        cv: Condvar::new(),
        inside: false.into(),
        calls: AtomicUsize::new(0),
    });
    gate.mem.write("/tree/memcards/Mcd001.ps2", &blank_card(8192, true, T)).unwrap();
    let mut vfs = host_vfs(std::ptr::null());
    vfs.user = &*gate as *const Gate as *mut c_void;
    vfs.stat = Some(gated::stat);
    vfs.list = Some(gated::list);
    vfs.read = Some(gated::read);
    vfs.write = Some(gated::write);
    vfs.mkdir = Some(gated::mkdir);
    vfs.rename = Some(gated::rename);
    vfs.remove = Some(gated::remove);
    vfs.join = Some(gated::join);
    vfs.parent = Some(gated::parent);
    vfs.name = Some(gated::name);
    let dir = tempfile::tempdir().unwrap();
    let data = cstr(dir.path().join("data").to_str().unwrap());
    let mut cfg = config_init();
    cfg.data_dir = data.as_ptr();
    cfg.vfs = &vfs;
    let mut c = std::ptr::null_mut();
    assert_eq!(unsafe { es_client_create(&cfg, &mut c) }, ES_OK);
    add(c, Path::new("/tree/memcards/Mcd001.ps2"), None).unwrap();

    // The next VFS call holds the worker.
    *gate.hold.lock().unwrap() = true;
    assert_eq!(unsafe { es_sync_now(c) }, ES_PENDING);
    let deadline = Instant::now() + Duration::from_secs(20);
    while !gate.inside.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the worker never called the VFS");
        std::thread::sleep(Duration::from_millis(5));
    }
    let client = c as usize;
    let destroy = std::thread::spawn(move || unsafe { es_client_destroy(client as *mut Client, 50) });
    // Destroy has timed out on the worker and waits for the call in progress.
    std::thread::sleep(Duration::from_millis(300));
    assert!(!destroy.is_finished());
    *gate.hold.lock().unwrap() = false;
    gate.cv.notify_all();
    destroy.join().unwrap();
    let after = gate.calls.load(Ordering::SeqCst);
    // The worker finishes its sync and shuts down without calling into the host again.
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(gate.calls.load(Ordering::SeqCst), after);
}

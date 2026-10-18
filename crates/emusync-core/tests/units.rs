//! Unit mode (DESIGN §2.1): PS2 cards whose saves sync one by one. Multi-device simulations
//! over the in-memory remote with real card images built by the `ps2.card` adapter.

use emusync_core::{CardMode, ConflictPolicy, ContainerDesc, Engine, EngineOptions, Resolution, SyncReport};
use emusync_formats::ps2::mcfs::{Node, NodeKind, MODE_DIR_DEFAULT, MODE_FILE_DEFAULT};
use emusync_formats::ps2::{blank_card, encode, layout, LAYOUT_META};
use emusync_formats::{ApplyOp, Canonical, ContainerFormat, Ps2Card};
use emusync_remote::MemoryRemote;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

const GTA: &str = "BASLUS-20062GTA3";
const RAC: &str = "BASCUS-97199RAC";
const SOTC: &str = "BASCUS-97472SOTC";
const T: [u8; 8] = [0, 0, 30, 12, 18, 10, 0xEA, 0x07];

struct Device {
    engine: Engine,
    card: PathBuf,
    _dir: tempfile::TempDir,
}

fn save(name: &str, data: &[u8]) -> Canonical {
    let node = Node {
        name: name.as_bytes().to_vec(),
        mode: MODE_DIR_DEFAULT,
        attr: 0,
        created: T,
        modified: T,
        kind: NodeKind::Dir(vec![
            Node {
                name: b"icon.sys".to_vec(),
                mode: MODE_FILE_DEFAULT,
                attr: 0,
                created: T,
                modified: T,
                kind: NodeKind::File(vec![0x50; 964]),
            },
            Node {
                name: b"data".to_vec(),
                mode: MODE_FILE_DEFAULT,
                attr: 0,
                created: T,
                modified: T,
                kind: NodeKind::File(data.to_vec()),
            },
        ]),
    };
    Canonical { name: name.into(), bytes: encode(&node), meta: [(LAYOUT_META.to_string(), layout(&node))].into() }
}

impl Device {
    fn new(remote: &Arc<MemoryRemote>, name: &str, card: &[u8], policy: ConflictPolicy) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let card_path = dir.path().join("memcards").join("Mcd001.ps2");
        std::fs::create_dir_all(card_path.parent().unwrap()).unwrap();
        std::fs::write(&card_path, card).unwrap();
        let mut engine = Engine::open(
            dir.path().join("data"),
            EngineOptions { device_name: Some(name.into()), policy, ..Default::default() },
        )
        .unwrap();
        engine.add_remote(remote.clone()).unwrap();
        engine.add_container(ContainerDesc { location: card_path.clone(), ..Default::default() }).unwrap();
        Device { engine, card: card_path, _dir: dir }
    }

    fn sync(&mut self) -> SyncReport {
        self.engine.sync().unwrap()
    }

    /// The emulator writes saves to the card.
    fn game_writes(&self, ops: &[ApplyOp]) {
        let img = std::fs::read(&self.card).unwrap();
        std::fs::write(&self.card, Ps2Card.apply(&img, ops).unwrap()).unwrap();
    }

    /// Save name → data file contents, as the card holds them now.
    fn saves(&self) -> BTreeMap<String, Vec<u8>> {
        let img = std::fs::read(&self.card).unwrap();
        Ps2Card
            .extract_all(&img)
            .unwrap()
            .into_iter()
            .map(|c| {
                let node = emusync_formats::ps2::decode(&c, T).unwrap();
                let NodeKind::Dir(files) = node.kind else { panic!("{} is not a directory", c.name) };
                let data = files.into_iter().find(|f| f.name == b"data").map(|f| match f.kind {
                    NodeKind::File(d) => d,
                    NodeKind::Dir(_) => vec![],
                });
                (c.name, data.unwrap_or_default())
            })
            .collect()
    }
}

fn formatted() -> Vec<u8> {
    blank_card(8192, true, T)
}

fn unformatted() -> Vec<u8> {
    vec![0xFF; formatted().len()]
}

#[test]
fn different_games_offline_merge_without_conflict() {
    let remote = Arc::new(MemoryRemote::default());
    let mut win = Device::new(&remote, "windows", &formatted(), ConflictPolicy::Ask);
    let mut phone = Device::new(&remote, "android", &formatted(), ConflictPolicy::Ask);
    assert_eq!(win.engine.cards().unwrap()[0].mode, CardMode::Units, "PS2 cards default to unit mode");

    win.game_writes(&[ApplyOp::Put(save(GTA, b"gta v1"))]);
    win.sync();
    phone.sync();
    assert_eq!(phone.saves()[GTA], b"gta v1");

    // Offline on both: Windows plays GTA, the phone plays Ratchet & Clank.
    win.game_writes(&[ApplyOp::Put(save(GTA, b"gta v2"))]);
    phone.game_writes(&[ApplyOp::Put(save(RAC, b"rac v1"))]);
    let r = phone.sync();
    assert!(r.conflicts.is_empty());
    let r = win.sync();
    assert!(r.conflicts.is_empty(), "different saves on one card never conflict");
    assert_eq!(r.applied.len(), 1);
    let r = phone.sync();
    assert!(r.conflicts.is_empty());

    let want: BTreeMap<String, Vec<u8>> =
        [(GTA.to_string(), b"gta v2".to_vec()), (RAC.to_string(), b"rac v1".to_vec())].into();
    assert_eq!(win.saves(), want);
    assert_eq!(phone.saves(), want);
    let cards = win.engine.cards().unwrap();
    let gta = cards[0].units.iter().find(|u| u.name == GTA).unwrap();
    assert_eq!(gta.titles, ["ps2:SLUS-20062"]);
    assert_eq!(gta.state(), "in sync");
}

#[test]
fn same_save_edited_twice_conflicts_alone() {
    let remote = Arc::new(MemoryRemote::default());
    let mut mac = Device::new(&remote, "mac", &formatted(), ConflictPolicy::Ask);
    let mut phone = Device::new(&remote, "android", &formatted(), ConflictPolicy::Ask);
    mac.game_writes(&[ApplyOp::Put(save(GTA, b"v1"))]);
    mac.sync();
    phone.sync();

    mac.game_writes(&[ApplyOp::Put(save(GTA, b"mac"))]);
    phone.game_writes(&[ApplyOp::Put(save(GTA, b"phone")), ApplyOp::Put(save(RAC, b"rac"))]);
    mac.sync();
    let r = phone.sync();
    assert_eq!(r.conflicts.len(), 1, "only GTA conflicts");
    let conflicts = phone.engine.conflicts().unwrap();
    assert_eq!(conflicts[0].unit, format!("ps2:Mcd001:{GTA}"));
    // The phone keeps its own GTA until the conflict is resolved; R&C still syncs.
    assert_eq!(phone.saves()[GTA], b"phone");
    mac.sync();
    assert_eq!(mac.saves()[RAC], b"rac");
    assert_eq!(mac.saves()[GTA], b"mac");

    let mac_tip = conflicts[0].tips.iter().find(|t| t.device == "mac").unwrap().version.id.clone();
    phone.engine.resolve(conflicts[0].id, Resolution::Take(mac_tip)).unwrap();
    phone.sync();
    mac.sync();
    assert_eq!(phone.saves(), mac.saves());
    assert_eq!(phone.saves()[GTA], b"mac");
    // The phone's version is still in history.
    let hist = phone.engine.history(GTA).unwrap();
    assert!(hist.iter().any(|h| h.device == "android" && !h.version.tombstone && h.version.parents.len() == 1));
}

#[test]
fn unformatted_card_on_a_new_device_receives_every_save() {
    let remote = Arc::new(MemoryRemote::default());
    let mut win = Device::new(&remote, "windows", &formatted(), ConflictPolicy::Ask);
    win.game_writes(&[ApplyOp::Put(save(GTA, b"gta")), ApplyOp::Put(save(SOTC, &vec![7; 300_000]))]);
    win.sync();

    // PCSX2 creates new card files unformatted.
    let mut mac = Device::new(&remote, "mac", &unformatted(), ConflictPolicy::Ask);
    let r = mac.sync();
    assert_eq!(r.applied.len(), 2, "{:?}", r.blocked);
    assert_eq!(mac.saves(), win.saves());
    // Nothing new to upload: the Mac's card now matches the cloud.
    let r = mac.sync();
    assert_eq!(r.new_local_versions, 0);
    assert!(r.applied.is_empty());
}

#[test]
fn deletions_sync_and_stay_restorable() {
    let remote = Arc::new(MemoryRemote::default());
    let mut a = Device::new(&remote, "a", &formatted(), ConflictPolicy::Ask);
    let mut b = Device::new(&remote, "b", &formatted(), ConflictPolicy::Ask);
    a.game_writes(&[ApplyOp::Put(save(GTA, b"gta")), ApplyOp::Put(save(RAC, b"rac"))]);
    a.sync();
    b.sync();
    assert_eq!(b.saves().len(), 2);

    // Deleted in the PS2 browser on device A.
    a.game_writes(&[ApplyOp::Delete(GTA.into())]);
    let r = a.sync();
    assert_eq!(r.new_local_versions, 1);
    b.sync();
    assert_eq!(b.saves().keys().collect::<Vec<_>>(), [RAC]);
    let status = b.engine.cards().unwrap();
    assert_eq!(status[0].units.iter().find(|u| u.name == GTA).unwrap().state(), "deleted");

    // Restoring the old version brings it back everywhere.
    let old = b.engine.history(GTA).unwrap().into_iter().find(|h| !h.version.tombstone).unwrap();
    b.engine.restore(GTA, &old.version.id).unwrap();
    b.sync();
    a.sync();
    assert_eq!(a.saves()[GTA], b"gta");
    assert_eq!(b.saves()[GTA], b"gta");
}

#[test]
fn a_wiped_card_is_refilled_not_deleted_everywhere() {
    let remote = Arc::new(MemoryRemote::default());
    let mut a = Device::new(&remote, "a", &formatted(), ConflictPolicy::Ask);
    let mut b = Device::new(&remote, "b", &formatted(), ConflictPolicy::Ask);
    a.game_writes(&[ApplyOp::Put(save(GTA, b"gta")), ApplyOp::Put(save(RAC, b"rac"))]);
    a.sync();
    b.sync();

    // The user deletes the card file and the emulator creates a fresh one.
    std::fs::write(&a.card, unformatted()).unwrap();
    let r = a.sync();
    assert!(r.warnings.iter().any(|w| w.contains("disappeared")), "{:?}", r.warnings);
    assert_eq!(r.applied.len(), 2);
    assert_eq!(a.saves().len(), 2);
    b.sync();
    assert_eq!(b.saves().len(), 2, "nothing was deleted on the other device");
}

#[test]
fn saves_that_do_not_fit_wait_and_the_rest_apply() {
    let remote = Arc::new(MemoryRemote::default());
    let mut a = Device::new(&remote, "a", &formatted(), ConflictPolicy::Ask);
    let mut b = Device::new(&remote, "b", &formatted(), ConflictPolicy::Ask);
    // Device B's card is nearly full of its own big save.
    b.game_writes(&[ApplyOp::Put(save("BASLUS-99999BIG", &vec![1; 7_900_000]))]);
    b.sync();
    a.sync();
    // A's copy of the big save is on A too now; A adds a small and a large save.
    a.game_writes(&[ApplyOp::Put(save(GTA, b"small")), ApplyOp::Delete("BASLUS-99999BIG".into())]);
    a.sync();
    // Deleting the big save on A frees B's card; that and GTA both apply.
    let r = b.sync();
    assert!(r.blocked.is_empty(), "{:?}", r.blocked);
    assert_eq!(b.saves().keys().collect::<Vec<_>>(), [GTA]);

    // Now a save that can't fit next to B's new local giant.
    b.game_writes(&[ApplyOp::Put(save("BASLUS-88888HUGE", &vec![2; 7_000_000]))]);
    b.sync();
    a.game_writes(&[ApplyOp::Put(save(RAC, &vec![3; 2_000_000]))]);
    a.sync();
    let r = b.sync();
    assert_eq!(r.blocked.len(), 1);
    assert!(r.blocked[0].reason.contains("space"), "{}", r.blocked[0].reason);
    assert!(!b.saves().contains_key(RAC));
    // Freeing space lets it through on the next sync.
    b.game_writes(&[ApplyOp::Delete("BASLUS-88888HUGE".into())]);
    let r = b.sync();
    assert!(r.applied.iter().any(|x| x.unit.ends_with(RAC)), "{:?}", r);
    assert_eq!(b.saves()[RAC].len(), 2_000_000);
}

#[test]
fn whole_file_cards_switch_to_unit_mode() {
    let remote = Arc::new(MemoryRemote::default());
    let mut card = formatted();
    card = Ps2Card.apply(&card, &[ApplyOp::Put(save(GTA, b"gta"))]).unwrap();
    // Two devices registered as M0 did: raw format, whole-file mode.
    let mk = |name: &str| {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Mcd001.ps2");
        std::fs::write(&path, &card).unwrap();
        let mut engine = Engine::open(
            dir.path().join("data"),
            EngineOptions { device_name: Some(name.into()), ..Default::default() },
        )
        .unwrap();
        engine.add_remote(remote.clone()).unwrap();
        engine
            .add_container(ContainerDesc { location: path.clone(), format: Some("raw".into()), ..Default::default() })
            .unwrap();
        Device { engine, card: path, _dir: dir }
    };
    let mut a = mk("a");
    let mut b = mk("b");
    a.sync();
    b.sync();
    assert_eq!(a.engine.cards().unwrap()[0].mode, CardMode::WholeFile);

    // Upgrade: re-adding detects ps2.card; switching the mode publishes it.
    for d in [&mut a, &mut b] {
        let loc = d.card.clone();
        d.engine.add_container(ContainerDesc { location: loc, ..Default::default() }).unwrap();
    }
    a.engine.set_card_mode("Mcd001", CardMode::Units).unwrap();
    a.sync();
    let r = b.sync();
    assert_eq!(r.mode_changes, ["ps2:Mcd001"]);
    assert!(r.conflicts.is_empty());
    assert_eq!(r.new_local_versions, 0, "identical saves are adopted, not duplicated");

    // From now on saves merge.
    a.game_writes(&[ApplyOp::Put(save(RAC, b"rac"))]);
    b.game_writes(&[ApplyOp::Put(save(SOTC, b"sotc"))]);
    a.sync();
    b.sync();
    a.sync();
    assert_eq!(a.saves(), b.saves());
    assert_eq!(a.saves().len(), 3);
}

#[test]
fn reordered_card_is_not_a_change() {
    let remote = Arc::new(MemoryRemote::default());
    let mut a = Device::new(&remote, "a", &formatted(), ConflictPolicy::Ask);
    a.game_writes(&[ApplyOp::Put(save(GTA, b"gta")), ApplyOp::Put(save(RAC, b"rac"))]);
    a.sync();
    // Rewriting the same saves in another order (and so another layout on the card).
    a.game_writes(&[ApplyOp::Delete(GTA.into())]);
    a.game_writes(&[ApplyOp::Put(save(GTA, b"gta"))]);
    let r = a.sync();
    // GTA was deleted and re-added between syncs: the net content is unchanged.
    assert_eq!(r.new_local_versions, 0);
}

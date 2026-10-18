//! PCSX2 folder memory cards (DESIGN §5.2, §18): a folder card and a card file of the same
//! logical card sync with each other directly, over the in-memory remote.

use emusync_core::{fsutil, ConflictPolicy, ContainerDesc, Engine, EngineOptions, SyncReport};
use emusync_formats::ps2::mcfs::{Node, NodeKind, MODE_DIR_DEFAULT, MODE_FILE_DEFAULT};
use emusync_formats::ps2::{blank_card, decode, encode, layout, LAYOUT_META};
use emusync_formats::{ApplyOp, Canonical, ContainerFormat, Ps2Card, Ps2Folder};
use emusync_remote::MemoryRemote;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const GTA: &str = "BASLUS-20062GTA3";
const RAC: &str = "BASCUS-97199RAC";
const T: [u8; 8] = [0, 0, 30, 12, 18, 10, 0xEA, 0x07];

fn save(name: &str, data: &[u8]) -> Canonical {
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
    Canonical { name: name.into(), bytes: encode(&node), meta: [(LAYOUT_META.to_string(), layout(&node))].into() }
}

/// What a device's card is: a `.ps2` file, or a PCSX2 folder card.
enum Layout {
    File,
    Folder,
}

struct Device {
    engine: Engine,
    card: PathBuf,
    layout: Layout,
    _dir: tempfile::TempDir,
}

impl Device {
    fn new(remote: &Arc<MemoryRemote>, name: &str, layout: Layout) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let memcards = dir.path().join("memcards");
        std::fs::create_dir_all(&memcards).unwrap();
        let card = match layout {
            Layout::File => {
                let p = memcards.join("Mcd001.ps2");
                std::fs::write(&p, blank_card(8192, true, T)).unwrap();
                p
            }
            Layout::Folder => {
                // As PCSX2 creates a folder card: a directory with an empty superblock file.
                let p = memcards.join("Mcd001");
                std::fs::create_dir(&p).unwrap();
                std::fs::write(p.join("_pcsx2_superblock"), b"").unwrap();
                p
            }
        };
        let mut engine = Engine::open(
            dir.path().join("data"),
            EngineOptions { device_name: Some(name.into()), policy: ConflictPolicy::Ask, ..Default::default() },
        )
        .unwrap();
        engine.add_remote(remote.clone()).unwrap();
        engine.add_container(ContainerDesc { location: card.clone(), ..Default::default() }).unwrap();
        Device { engine, card, layout, _dir: dir }
    }

    fn sync(&mut self) -> SyncReport {
        self.engine.sync().unwrap()
    }

    fn image(&self) -> Vec<u8> {
        match self.layout {
            Layout::File => std::fs::read(&self.card).unwrap(),
            Layout::Folder => fsutil::read_tree(&self.card).unwrap().encode(),
        }
    }

    fn fmt(&self) -> &'static dyn ContainerFormat {
        match self.layout {
            Layout::File => &Ps2Card,
            Layout::Folder => &Ps2Folder,
        }
    }

    /// The emulator writes saves to the card.
    fn game_writes(&self, ops: &[ApplyOp]) {
        let img = self.fmt().apply(&self.image(), ops).unwrap();
        match self.layout {
            Layout::File => std::fs::write(&self.card, img).unwrap(),
            Layout::Folder => {
                fsutil::replace_dir(&self.card, &emusync_formats::tree::Tree::decode(&img).unwrap()).unwrap()
            }
        }
    }

    /// Save name → data file contents, as the card holds them now.
    fn saves(&self) -> BTreeMap<String, Vec<u8>> {
        self.fmt()
            .extract_all(&self.image())
            .unwrap()
            .into_iter()
            .map(|c| {
                let node = decode(&c, T).unwrap();
                let NodeKind::Dir(files) = node.kind else { panic!("{} is not a directory", c.name) };
                let data = files.into_iter().find(|f| f.name == b"data").map(|f| match f.kind {
                    NodeKind::File(d) => d,
                    NodeKind::Dir(_) => vec![],
                });
                (c.name, data.unwrap_or_default())
            })
            .collect()
    }

    /// Canonical bytes of every save on the card.
    fn units(&self) -> BTreeMap<String, Vec<u8>> {
        self.fmt().extract_all(&self.image()).unwrap().into_iter().map(|c| (c.name, c.bytes)).collect()
    }
}

fn host(card: &Path, rel: &str) -> PathBuf {
    rel.split('/').fold(card.to_path_buf(), |p, s| p.join(s))
}

#[test]
fn folder_card_and_card_file_sync_with_each_other() {
    let remote = Arc::new(MemoryRemote::default());
    let mut mac = Device::new(&remote, "macos", Layout::Folder);
    let mut phone = Device::new(&remote, "android", Layout::File);
    let card_ids: Vec<String> = [&mac, &phone].iter().map(|d| d.engine.cards().unwrap()[0].card_id.clone()).collect();
    assert_eq!(card_ids, ["ps2:Mcd001", "ps2:Mcd001"], "a folder card and a card file are the same logical card");
    assert_eq!(mac.engine.containers().unwrap()[0].format, "ps2.card.folder");

    // GTA saved into the folder card on macOS reaches the card file on the phone.
    mac.game_writes(&[ApplyOp::Put(save(GTA, b"gta v1"))]);
    assert_eq!(mac.sync().new_local_versions, 1);
    let r = phone.sync();
    assert_eq!(r.applied.len(), 1, "{:?}", r.blocked);
    assert_eq!(phone.saves()[GTA], b"gta v1");

    // Offline on both: different games merge with no conflict.
    mac.game_writes(&[ApplyOp::Put(save(GTA, b"gta v2"))]);
    phone.game_writes(&[ApplyOp::Put(save(RAC, b"rac v1"))]);
    assert!(phone.sync().conflicts.is_empty());
    let r = mac.sync();
    assert!(r.conflicts.is_empty() && r.blocked.is_empty(), "{r:?}");
    assert_eq!(r.applied.len(), 1);
    phone.sync();

    let want: BTreeMap<String, Vec<u8>> =
        [(GTA.to_string(), b"gta v2".to_vec()), (RAC.to_string(), b"rac v1".to_vec())].into();
    assert_eq!(mac.saves(), want);
    assert_eq!(phone.saves(), want);
    assert_eq!(mac.units(), phone.units(), "the same canonical saves on both layouts");

    // The folder holds the save the way PCSX2 writes it.
    assert!(host(&mac.card, "_pcsx2_superblock").metadata().unwrap().len() == 8192, "formatted on first apply");
    assert!(host(&mac.card, "BASCUS-97199RAC/_pcsx2_index").is_file());
    assert_eq!(std::fs::read(host(&mac.card, "BASCUS-97199RAC/data")).unwrap(), b"rac v1");
    for suffix in [".emusync-tmp", ".emusync-old", ".emusync-swap"] {
        let mut name = mac.card.file_name().unwrap().to_os_string();
        name.push(suffix);
        assert!(!mac.card.with_file_name(name).exists(), "{suffix} left behind");
    }

    // Applying and re-reading are stable: nothing new to publish on either side.
    for d in [&mut mac, &mut phone] {
        let r = d.sync();
        assert_eq!((r.new_local_versions, r.applied.len(), r.conflicts.len()), (0, 0, 0), "{r:?}");
    }

    // A deletion on the phone reaches the folder card as PCSX2 deletes: renamed aside.
    phone.game_writes(&[ApplyOp::Delete(RAC.into())]);
    phone.sync();
    mac.sync();
    assert_eq!(mac.saves().keys().collect::<Vec<_>>(), [GTA]);
    assert!(host(&mac.card, "_pcsx2_deleted_BASCUS-97199RAC/data").is_file());
}

#[test]
fn identical_saves_on_both_layouts_are_adopted() {
    let remote = Arc::new(MemoryRemote::default());
    let mut mac = Device::new(&remote, "macos", Layout::Folder);
    let mut win = Device::new(&remote, "windows", Layout::File);
    // The same save on both devices before either synced (copied by hand, say).
    mac.game_writes(&[ApplyOp::Put(save(GTA, b"same"))]);
    win.game_writes(&[ApplyOp::Put(save(GTA, b"same"))]);
    mac.sync();
    let r = win.sync();
    assert!(r.conflicts.is_empty(), "identical content is adopted, not a conflict: {r:?}");
    assert_eq!(r.new_local_versions, 0);
    let unit = &win.engine.cards().unwrap()[0].units[0];
    assert_eq!((unit.name.as_str(), unit.state()), (GTA, "in sync"));
}

#[test]
fn never_applies_to_an_open_folder_card() {
    let remote = Arc::new(MemoryRemote::default());
    let mut mac = Device::new(&remote, "macos", Layout::Folder);
    let mut phone = Device::new(&remote, "android", Layout::File);
    let id = mac.engine.containers().unwrap()[0].id;
    phone.game_writes(&[ApplyOp::Put(save(RAC, b"rac v1"))]);
    phone.sync();

    // PCSX2 has the folder card open: it would overwrite outside changes on its next flush.
    mac.engine.set_busy(id, true);
    let before = mac.image();
    let r = mac.sync();
    assert!(r.applied.is_empty());
    assert!(r.blocked.iter().any(|b| b.reason.contains("in use")), "{r:?}");
    assert_eq!(mac.image(), before, "the folder is untouched while open");
    assert!(mac.engine.cards().unwrap()[0].units.iter().any(|u| u.inbox.is_some()));

    // Released: the first sync applies what waited in the inbox.
    mac.engine.set_busy(id, false);
    let r = mac.sync();
    assert_eq!(r.applied.len(), 1, "{r:?}");
    assert_eq!(mac.saves()[RAC], b"rac v1");
}

#[test]
fn interrupted_folder_replace_is_finished_on_next_sync() {
    let remote = Arc::new(MemoryRemote::default());
    let mut mac = Device::new(&remote, "macos", Layout::Folder);
    mac.game_writes(&[ApplyOp::Put(save(GTA, b"gta v1"))]);
    mac.sync();
    // Simulate a crash in the middle of a replace: new tree written and journalled, the old
    // folder moved aside, the new one not yet renamed into place.
    let next = Ps2Folder.apply(&mac.image(), &[ApplyOp::Put(save(GTA, b"gta v2"))]).unwrap();
    let mut name = mac.card.file_name().unwrap().to_os_string();
    let base = name.clone();
    name.push(".emusync-tmp");
    let tmp = mac.card.with_file_name(&name);
    let tree = emusync_formats::tree::Tree::decode(&next).unwrap();
    fsutil::replace_dir(&tmp, &tree).unwrap(); // writes the tree at `tmp`
    let mut journal = base.clone();
    journal.push(".emusync-swap");
    std::fs::write(mac.card.with_file_name(journal), b"x").unwrap();
    let mut old = base;
    old.push(".emusync-old");
    std::fs::rename(&mac.card, mac.card.with_file_name(old)).unwrap();
    assert!(!mac.card.exists());

    let r = mac.sync();
    assert!(r.blocked.is_empty(), "{r:?}");
    assert_eq!(mac.saves()[GTA], b"gta v2", "rolled forward");
    assert_eq!(r.new_local_versions, 1);
}

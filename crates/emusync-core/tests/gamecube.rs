//! GameCube cards (DESIGN §25): Dolphin's raw card on one device and its GCI folder on another
//! are the same logical card and sync per save, over the in-memory remote.

use emusync_core::{fsutil, ConflictPolicy, ContainerDesc, Engine, EngineOptions};
use emusync_formats::gc::card::blank_card;
use emusync_formats::gc::gci::parse_gci;
use emusync_formats::gc::{canonical, decode, Save};
use emusync_formats::tree::Tree;
use emusync_formats::{ApplyOp, ContainerFormat, GcCard, GciFolder};
use emusync_remote::MemoryRemote;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

/// A save as a game writes it: `blocks` blocks of `fill`.
fn save(game: &[u8; 4], maker: &[u8; 2], file: &str, blocks: u16, fill: u8) -> Save {
    let mut e = [0u8; 64];
    e[0..4].copy_from_slice(game);
    e[4..6].copy_from_slice(maker);
    e[6] = 0xFF;
    e[7] = 2;
    e[8..8 + file.len()].copy_from_slice(file.as_bytes());
    e[0x28..0x2C].copy_from_slice(&0x2F00_0000u32.to_be_bytes());
    e[0x34] = 4;
    e[0x38..0x3A].copy_from_slice(&blocks.to_be_bytes());
    Save { dentry: e, data: vec![fill; blocks as usize * 0x2000] }
}

enum Layout {
    Raw,
    Gci,
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
        let gc = dir.path().join("Dolphin Emulator").join("GC");
        let card = match layout {
            Layout::Raw => {
                std::fs::create_dir_all(&gc).unwrap();
                let p = gc.join("MemoryCardA.USA.raw");
                std::fs::write(&p, blank_card(16, *b"emusync-test", 0, false)).unwrap();
                p
            }
            // As Dolphin leaves it before the first save: an empty folder.
            Layout::Gci => {
                let p = gc.join("USA").join("Card A");
                std::fs::create_dir_all(&p).unwrap();
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

    fn fmt(&self) -> &'static dyn ContainerFormat {
        match self.layout {
            Layout::Raw => &GcCard,
            Layout::Gci => &GciFolder,
        }
    }

    fn image(&self) -> Vec<u8> {
        match self.layout {
            Layout::Raw => std::fs::read(&self.card).unwrap(),
            Layout::Gci => fsutil::read_tree(&self.card).unwrap().encode(),
        }
    }

    /// Dolphin writes saves to the card.
    fn game_writes(&self, ops: &[ApplyOp]) {
        let img = self.fmt().apply(&self.image(), ops).unwrap();
        match self.layout {
            Layout::Raw => std::fs::write(&self.card, img).unwrap(),
            Layout::Gci => fsutil::replace_dir(&self.card, &Tree::decode(&img).unwrap()).unwrap(),
        }
    }

    /// Unit name → canonical bytes of every save on the card.
    fn units(&self) -> BTreeMap<String, Vec<u8>> {
        self.fmt().extract_all(&self.image()).unwrap().into_iter().map(|c| (c.name, c.bytes)).collect()
    }
}

#[test]
fn raw_card_and_gci_folder_sync_with_each_other() {
    let remote = Arc::new(MemoryRemote::default());
    let mut pc = Device::new(&remote, "windows", Layout::Raw);
    let mut phone = Device::new(&remote, "android", Layout::Gci);
    for d in [&pc, &phone] {
        let c = &d.engine.containers().unwrap()[0];
        assert_eq!(c.card_id, "gc:MemoryCardA.USA", "a raw card and a GCI folder are the same card");
    }
    assert_eq!(pc.engine.containers().unwrap()[0].format, "gc.card");
    assert_eq!(phone.engine.containers().unwrap()[0].format, "gc.gci");

    // Saves made on the PC reach the phone as Dolphin's GCI files.
    let metroid = save(b"GM8E", b"01", "MetroidPrime A", 1, 1);
    let rogue = save(b"GSWE", b"64", "RogueLeader", 3, 2);
    pc.game_writes(&[ApplyOp::Put(canonical(&metroid)), ApplyOp::Put(canonical(&rogue))]);
    pc.engine.sync().unwrap();
    phone.engine.sync().unwrap();
    assert_eq!(phone.units(), pc.units());
    let file = phone.card.join("01-GM8E-MetroidPrime A.gci");
    assert_eq!(parse_gci(&std::fs::read(&file).unwrap()).unwrap().data, metroid.data);

    // A save changed on the phone, and a save deleted there, reach the PC card.
    let metroid2 = save(b"GM8E", b"01", "MetroidPrime A", 1, 9);
    phone.game_writes(&[ApplyOp::Put(canonical(&metroid2)), ApplyOp::Delete(rogue.unit_name())]);
    phone.engine.sync().unwrap();
    pc.engine.sync().unwrap();
    assert_eq!(pc.units(), phone.units());
    assert_eq!(pc.units().len(), 1);
    assert_eq!(pc.units()["GM8E01-MetroidPrime A"], canonical(&metroid2).bytes);
    GcCard.validate(&pc.image()).unwrap();
}

/// A copy-protected save carries the destination card's serial on a raw card, and none in a GCI
/// file (Dolphin fills it in), yet it is one version everywhere.
#[test]
fn copy_protected_saves_follow_the_card() {
    let remote = Arc::new(MemoryRemote::default());
    let mut a = Device::new(&remote, "a", Layout::Gci);
    let mut b = Device::new(&remote, "b", Layout::Raw);
    let pso = save(b"GPOE", b"8P", "PSO_SYSTEM", 3, 7);
    a.game_writes(&[ApplyOp::Put(canonical(&pso))]);
    a.engine.sync().unwrap();
    b.engine.sync().unwrap();
    assert_eq!(b.units(), a.units());
    // On the raw card the save holds that card's serial (a blank card's first save starts at
    // block 5); in the GCI file the serial fields are zero.
    let raw = std::fs::read(&b.card).unwrap();
    let mut expected = pso.clone();
    expected.set_serial(&raw[..32]);
    assert_ne!(expected.data, pso.data);
    assert_eq!(&raw[5 * 0x2000..8 * 0x2000], &expected.data[..]);
    let gci = std::fs::read(a.card.join("8P-GPOE-PSO_SYSTEM.gci")).unwrap();
    assert_eq!(parse_gci(&gci).unwrap().data, decode(&canonical(&pso)).unwrap().data);
    // The serial is no change: nothing new is recorded or uploaded on either device.
    for d in [&mut a, &mut b] {
        let report = d.engine.sync().unwrap();
        assert_eq!((report.new_local_versions, report.uploaded_versions), (0, 0), "{report:?}");
    }
}

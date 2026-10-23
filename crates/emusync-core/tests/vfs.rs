//! Containers behind a VFS (DESIGN §28): a device whose cards live in a [`MemoryVfs`], which
//! has no paths and can't set modification times or rename over a file (like Android's SAF),
//! syncs with a device on the native file system: a PS2 card file, a Dolphin GCI folder and a
//! PSP memory stick.

use emusync_core::vfs::{self, MemoryVfs, Vfs};
use emusync_core::{ContainerDesc, Engine, EngineOptions};
use emusync_formats::gc::{self, Save};
use emusync_formats::ps2::mcfs::{Node, NodeKind, MODE_DIR_DEFAULT, MODE_FILE_DEFAULT};
use emusync_formats::ps2::{blank_card, encode, layout, LAYOUT_META};
use emusync_formats::{ApplyOp, Canonical, ContainerFormat, GcCard, GciFolder, Ps2Card};
use emusync_remote::MemoryRemote;
use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

const T: [u8; 8] = [0, 0, 30, 12, 18, 10, 0xEA, 0x07];

fn ps2_save(name: &str, data: &[u8]) -> Canonical {
    let node = Node {
        name: name.as_bytes().to_vec(),
        mode: MODE_DIR_DEFAULT,
        attr: 0,
        created: T,
        modified: T,
        kind: NodeKind::Dir(vec![Node {
            name: b"data".to_vec(),
            mode: MODE_FILE_DEFAULT,
            attr: 0,
            created: T,
            modified: T,
            kind: NodeKind::File(data.to_vec()),
        }]),
    };
    Canonical { name: name.into(), bytes: encode(&node), meta: [(LAYOUT_META.to_string(), layout(&node))].into() }
}

fn gc_save(fill: u8) -> Save {
    let mut e = [0u8; 64];
    e[0..4].copy_from_slice(b"GSWE");
    e[4..6].copy_from_slice(b"64");
    e[6] = 0xFF;
    e[7] = 2;
    e[8..8 + 12].copy_from_slice(b"RogueLeader\0");
    e[0x28..0x2C].copy_from_slice(&0x2F00_0000u32.to_be_bytes());
    e[0x34] = 4;
    e[0x38..0x3A].copy_from_slice(&2u16.to_be_bytes());
    Save { dentry: e, data: vec![fill; 2 * 0x2000] }
}

fn engine(data: PathBuf, name: &str, remote: &Arc<MemoryRemote>, vfs: Option<Arc<dyn Vfs>>) -> Engine {
    let opts = EngineOptions { device_name: Some(name.into()), vfs, ..Default::default() };
    let mut e = Engine::open(data, opts).unwrap();
    e.add_remote(remote.clone()).unwrap();
    e
}

fn add(e: &mut Engine, location: &str) -> String {
    let id = e.add_container(ContainerDesc { location: location.into(), ..Default::default() }).unwrap();
    e.containers().unwrap().into_iter().find(|c| c.id == id).unwrap().card_id
}

fn no_scratch(v: &MemoryVfs, dir: &str) {
    let left: Vec<String> =
        v.list(dir).unwrap().into_iter().map(|(n, _)| n).filter(|n| vfs::is_scratch_name(n)).collect();
    assert!(left.is_empty(), "{dir}: {left:?}");
}

#[test]
fn a_ps2_card_file_syncs_through_a_vfs() {
    let remote = Arc::new(MemoryRemote::new(Default::default()));
    let dir = tempfile::tempdir().unwrap();
    let native_card = dir.path().join("Mcd001.ps2");
    fs::write(
        &native_card,
        Ps2Card.apply(&blank_card(8192, true, T), &[ApplyOp::Put(ps2_save("BASLUS-20062GTA3", b"gta"))]).unwrap(),
    )
    .unwrap();
    let mut pc = engine(dir.path().join("pc"), "VEGA", &remote, None);
    assert_eq!(add(&mut pc, &native_card.to_string_lossy()), "ps2:Mcd001");

    let mem = Arc::new(MemoryVfs::with_root("/sd/memcards"));
    mem.write("/sd/memcards/Mcd001.ps2", &blank_card(8192, true, T)).unwrap();
    let mut phone = engine(dir.path().join("phone"), "Thor", &remote, Some(mem.clone()));
    assert_eq!(add(&mut phone, "/sd/memcards/Mcd001.ps2"), "ps2:Mcd001");

    pc.sync().unwrap();
    phone.sync().unwrap();
    let names = |img: &[u8]| -> BTreeSet<String> { Ps2Card.list(img).unwrap().into_iter().map(|u| u.name).collect() };
    assert_eq!(names(&mem.read("/sd/memcards/Mcd001.ps2").unwrap()), ["BASLUS-20062GTA3".to_string()].into());
    no_scratch(&mem, "/sd/memcards");

    // The emulator on the phone saves another game; the PC gets it.
    let img = mem.read("/sd/memcards/Mcd001.ps2").unwrap();
    mem.write(
        "/sd/memcards/Mcd001.ps2",
        &Ps2Card.apply(&img, &[ApplyOp::Put(ps2_save("BASCUS-97199RAC", b"rac"))]).unwrap(),
    )
    .unwrap();
    phone.sync().unwrap();
    pc.sync().unwrap();
    assert_eq!(names(&fs::read(&native_card).unwrap()).len(), 2);
}

#[test]
fn folders_sync_through_a_vfs() {
    let remote = Arc::new(MemoryRemote::new(Default::default()));
    let dir = tempfile::tempdir().unwrap();

    // The PC: a raw GameCube card and a PSP memory stick, each with a save.
    let raw = dir.path().join("MemoryCardA.USA.raw");
    let card = gc::card::blank_card(16, *b"emusync-test", 0, false);
    fs::write(&raw, GcCard.apply(&card, &[ApplyOp::Put(gc::canonical(&gc_save(7)))]).unwrap()).unwrap();
    let savedata = dir.path().join("PSP").join("SAVEDATA");
    fs::create_dir_all(savedata.join("ULUS10041S0")).unwrap();
    fs::write(savedata.join("ULUS10041S0/PARAM.SFO"), b"\0PSF").unwrap();
    fs::write(savedata.join("ULUS10041S0/DATA.BIN"), [3; 2000]).unwrap();
    let mut pc = engine(dir.path().join("pc"), "VEGA", &remote, None);
    add(&mut pc, &raw.to_string_lossy());
    add(&mut pc, &savedata.to_string_lossy());

    // The phone: Dolphin's empty GCI folder, and PPSSPP's memory stick before any save.
    let mem = Arc::new(MemoryVfs::with_root("/tree/dolphin/GC/USA/Card A"));
    mem.create_dir("/tree/PSP").unwrap();
    let mut phone = engine(dir.path().join("phone"), "Thor", &remote, Some(mem.clone()));
    assert_eq!(add(&mut phone, "/tree/dolphin/GC/USA/Card A"), "gc:MemoryCardA.USA");
    assert_eq!(add(&mut phone, "/tree/PSP/SAVEDATA"), "psp:SAVEDATA");

    pc.sync().unwrap();
    let report = phone.sync().unwrap();
    assert_eq!(report.applied.len(), 2, "{report:?}");
    let gci = GciFolder.extract_all(&mem.read_tree("/tree/dolphin/GC/USA/Card A").unwrap().encode()).unwrap();
    assert_eq!(gci, vec![gc::canonical(&gc_save(7))]);
    assert_eq!(mem.read("/tree/PSP/SAVEDATA/ULUS10041S0/DATA.BIN").unwrap(), [3; 2000]);
    no_scratch(&mem, "/tree/dolphin/GC/USA");
    no_scratch(&mem, "/tree/PSP/SAVEDATA");

    // The phone saves; the PC gets it.
    mem.write("/tree/PSP/SAVEDATA/ULUS10041S0/DATA.BIN", &[4; 2100]).unwrap();
    phone.sync().unwrap();
    pc.sync().unwrap();
    assert_eq!(fs::read(savedata.join("ULUS10041S0/DATA.BIN")).unwrap(), [4; 2100]);
}

/// A replace interrupted on the phone's storage is finished at the next scan.
#[test]
fn interrupted_replaces_are_finished_at_the_next_scan() {
    let remote = Arc::new(MemoryRemote::new(Default::default()));
    let dir = tempfile::tempdir().unwrap();
    let mem = Arc::new(MemoryVfs::with_root("/sd"));
    let with_save =
        Ps2Card.apply(&blank_card(8192, true, T), &[ApplyOp::Put(ps2_save("BASLUS-20062GTA3", b"gta"))]).unwrap();
    mem.write("/sd/Mcd001.ps2", &blank_card(8192, true, T)).unwrap();
    let mut phone = engine(dir.path().join("phone"), "Thor", &remote, Some(mem.clone()));
    add(&mut phone, "/sd/Mcd001.ps2");
    // Crashed after the journal and the first rename: no card under its name.
    mem.write("/sd/Mcd001.ps2.emusync-tmp", &with_save).unwrap();
    mem.write("/sd/Mcd001.ps2.emusync-swap", b"j").unwrap();
    mem.rename("/sd/Mcd001.ps2", "Mcd001.ps2.emusync-old").unwrap();
    phone.sync().unwrap();
    assert_eq!(mem.read("/sd/Mcd001.ps2").unwrap(), with_save);
    no_scratch(&mem, "/sd");
}

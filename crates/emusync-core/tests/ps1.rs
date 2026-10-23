//! PlayStation cards (DESIGN §29): DuckStation's `.mcd` on one device, RetroArch's `.srm` or a
//! DexDrive `.gme` on another, are one logical card and sync per save, over the in-memory remote.

use emusync_core::{ConflictPolicy, ContainerDesc, Engine, EngineOptions};
use emusync_formats::ps1::{blank_card, canonical, Save, BLOCK, CARD_SIZE};
use emusync_formats::{ApplyOp, ContainerFormat, Ps1Card};
use emusync_remote::MemoryRemote;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

/// A save as a game writes it: a title frame, then `fill`.
fn save(name: &str, blocks: usize, fill: u8) -> Save {
    let mut data = vec![fill; blocks * BLOCK];
    data[..4].copy_from_slice(&[b'S', b'C', 0x11, blocks as u8]);
    Save { name: name.as_bytes().to_vec(), size: (blocks * BLOCK) as u32, data }
}

/// A DexDrive `.gme` file holding `card` (psx-spx's layout).
fn gme(card: &[u8]) -> Vec<u8> {
    let mut h = vec![0u8; 0xF40];
    h[..12].copy_from_slice(b"123-456-STD\0");
    h[0x10..0x15].copy_from_slice(&[0, 0, 1, 0, 1]);
    for f in 0..16 {
        h[0x15 + f] = card[f * 0x80];
        h[0x25 + f] = card[f * 0x80 + 8];
    }
    h.extend_from_slice(card);
    h
}

struct Device {
    engine: Engine,
    card: PathBuf,
    _dir: tempfile::TempDir,
}

impl Device {
    /// A device with the card file `file` (written from `image` unless `None`), bound to the
    /// logical card `shared_card_1` (DuckStation's slot 1 card).
    fn new(remote: &Arc<MemoryRemote>, name: &str, file: &str, image: Option<Vec<u8>>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let card = dir.path().join("memcards").join(file);
        std::fs::create_dir_all(card.parent().unwrap()).unwrap();
        if let Some(img) = image {
            std::fs::write(&card, img).unwrap();
        }
        let mut engine = Engine::open(
            dir.path().join("data"),
            EngineOptions { device_name: Some(name.into()), policy: ConflictPolicy::Ask, ..Default::default() },
        )
        .unwrap();
        engine.add_remote(remote.clone()).unwrap();
        let desc = ContainerDesc { location: card.clone(), card: Some("shared_card_1".into()), ..Default::default() };
        engine.add_container(desc).unwrap();
        let c = &engine.containers().unwrap()[0];
        assert_eq!((c.format.as_str(), c.card_id.as_str()), ("ps1.card", "ps1:shared_card_1"), "{file}");
        Device { engine, card, _dir: dir }
    }

    fn image(&self) -> Vec<u8> {
        std::fs::read(&self.card).unwrap()
    }

    /// The emulator writes saves to the card.
    fn game_writes(&self, ops: &[ApplyOp]) {
        std::fs::write(&self.card, Ps1Card.apply(&self.image(), ops).unwrap()).unwrap();
    }

    /// Unit name → canonical bytes of every save on the card.
    fn units(&self) -> BTreeMap<String, Vec<u8>> {
        Ps1Card.extract_all(&self.image()).unwrap().into_iter().map(|c| (c.name, c.bytes)).collect()
    }
}

#[test]
fn duckstation_and_retroarch_cards_sync_with_each_other() {
    let remote = Arc::new(MemoryRemote::default());
    let mut pc = Device::new(&remote, "windows", "shared_card_1.mcd", Some(blank_card()));
    let mut phone = Device::new(&remote, "android", "Final Fantasy VII.srm", Some(blank_card()));

    let ff7 = save("BASCUS-94163FF7-S01", 1, 1);
    let gt = save("BASCUS-94194GT", 3, 2);
    pc.game_writes(&[ApplyOp::Put(canonical(&ff7)), ApplyOp::Put(canonical(&gt))]);
    pc.engine.sync().unwrap();
    phone.engine.sync().unwrap();
    assert_eq!(phone.units(), pc.units());
    assert_eq!(phone.units().len(), 2);

    // A save changed on the phone and one deleted there reach the PC.
    let ff7b = save("BASCUS-94163FF7-S01", 1, 9);
    phone.game_writes(&[ApplyOp::Put(canonical(&ff7b)), ApplyOp::Delete(gt.unit_name())]);
    phone.engine.sync().unwrap();
    pc.engine.sync().unwrap();
    assert_eq!(pc.units(), phone.units());
    assert_eq!(pc.units()["BASCUS-94163FF7-S01"], canonical(&ff7b).bytes);
    assert_eq!(pc.units().len(), 1);
    Ps1Card.validate(&pc.image()).unwrap();

    // Saves of different games made offline on each device merge.
    let pc_only = save("BESLES-00001PC", 2, 3);
    let phone_only = save("BISLPS-00002PHONE", 1, 4);
    pc.game_writes(&[ApplyOp::Put(canonical(&pc_only))]);
    phone.game_writes(&[ApplyOp::Put(canonical(&phone_only))]);
    for _ in 0..2 {
        pc.engine.sync().unwrap();
        phone.engine.sync().unwrap();
    }
    assert_eq!(pc.units(), phone.units());
    assert_eq!(pc.units().len(), 3);
}

#[test]
fn a_dexdrive_card_syncs_with_a_raw_card() {
    let remote = Arc::new(MemoryRemote::default());
    let mut pc = Device::new(&remote, "windows", "shared_card_1.mcd", Some(blank_card()));
    let mut dex = Device::new(&remote, "dexdrive", "backup.gme", Some(gme(&blank_card())));
    let ff7 = save("BASCUS-94163FF7-S01", 2, 1);
    dex.game_writes(&[ApplyOp::Put(canonical(&ff7))]);
    dex.engine.sync().unwrap();
    pc.engine.sync().unwrap();
    assert_eq!(pc.units(), dex.units());
    // And back, into the .gme, which keeps its header.
    let gt = save("BASCUS-94194GT", 1, 2);
    pc.game_writes(&[ApplyOp::Put(canonical(&gt))]);
    pc.engine.sync().unwrap();
    dex.engine.sync().unwrap();
    assert_eq!(dex.units(), pc.units());
    let img = dex.image();
    assert_eq!(img.len(), 0xF40 + CARD_SIZE);
    assert!(img.starts_with(b"123-456-STD"));
}

/// A new device whose emulator hasn't made its card yet: the card is created, as DuckStation
/// would format it, with the saves from the cloud.
#[test]
fn a_missing_card_is_created_for_the_saves_synced_to_it() {
    let remote = Arc::new(MemoryRemote::default());
    let mut pc = Device::new(&remote, "windows", "shared_card_1.mcd", Some(blank_card()));
    let ff7 = save("BASCUS-94163FF7-S01", 1, 1);
    pc.game_writes(&[ApplyOp::Put(canonical(&ff7))]);
    pc.engine.sync().unwrap();

    let mut phone = Device::new(&remote, "android", "epsxe000.mcr", None);
    let report = phone.engine.sync().unwrap();
    assert!(phone.card.exists(), "{report:?}");
    assert_eq!(phone.units(), pc.units());
    assert_eq!(phone.image().len(), CARD_SIZE);
    let again = phone.engine.sync().unwrap();
    assert_eq!((again.new_local_versions, again.uploaded_versions), (0, 0), "{again:?}");
}

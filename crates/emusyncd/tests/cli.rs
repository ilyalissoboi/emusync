//! CLI tests. The S3 round trip is gated behind `EMUSYNC_S3_TEST_URI` (plus `AWS_ACCESS_KEY_ID`
//! / `AWS_SECRET_ACCESS_KEY`), e.g. a local moto server or MinIO:
//! `EMUSYNC_S3_TEST_URI="s3://emusync-test/ci?endpoint=http://127.0.0.1:9000&path_style=1"`.

use emusync_formats::ps2::mcfs::{Node, NodeKind, MODE_DIR_DEFAULT, MODE_FILE_DEFAULT};
use emusync_formats::ps2::{blank_card, encode, layout, LAYOUT_META};
use emusync_formats::{ApplyOp, Canonical, ContainerFormat, Ps2Card};
use std::path::Path;
use std::process::Command;

fn emusync(data: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_emusync")).arg("--data-dir").arg(data).args(args).output().unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text)
}

fn ok(data: &Path, args: &[&str]) -> String {
    let (success, text) = emusync(data, args);
    assert!(success, "emusync {args:?} failed:\n{text}");
    text
}

#[test]
fn card_add_and_list_without_remote() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let card = dir.path().join("Mcd001.ps2");
    std::fs::write(&card, b"card").unwrap();
    let out = ok(&data, &["card", "add", card.to_str().unwrap()]);
    assert!(out.contains("card ps2:Mcd001"), "{out}");
    let out = ok(&data, &["card", "list"]);
    assert!(out.contains("ps2:Mcd001") && out.contains("whole-file"), "{out}");
    let (success, out) = emusync(&data, &["sync"]);
    assert!(!success && out.contains("no remote configured"), "{out}");
    let (success, out) = emusync(&data, &["card", "add", dir.path().join("Mcd002.ps2").to_str().unwrap()]);
    assert!(!success && out.contains("not a file"), "{out}");
    let (success, out) = emusync(&data, &["remote", "add", "ftp://nope"]);
    assert!(!success && out.contains("unsupported scheme"), "{out}");
}

/// A save directory with one data file, as a game would write it.
fn save(name: &str, data: &[u8]) -> ApplyOp {
    let t = [0, 0, 30, 12, 18, 10, 0xEA, 0x07];
    let file = Node {
        name: b"data".to_vec(),
        mode: MODE_FILE_DEFAULT,
        attr: 0,
        created: t,
        modified: t,
        kind: NodeKind::File(data.to_vec()),
    };
    let node = Node {
        name: name.as_bytes().to_vec(),
        mode: MODE_DIR_DEFAULT,
        attr: 0,
        created: t,
        modified: t,
        kind: NodeKind::Dir(vec![file]),
    };
    ApplyOp::Put(Canonical {
        name: name.into(),
        bytes: encode(&node),
        meta: [(LAYOUT_META.to_string(), layout(&node))].into(),
    })
}

fn write_saves(card: &Path, ops: &[ApplyOp]) {
    let img = std::fs::read(card).unwrap();
    std::fs::write(card, Ps2Card.apply(&img, ops).unwrap()).unwrap();
}

fn save_names(card: &Path) -> Vec<String> {
    Ps2Card.list(&std::fs::read(card).unwrap()).unwrap().into_iter().map(|u| u.name).collect()
}

#[test]
fn ps2_cards_sync_per_save() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let card = dir.path().join("Mcd001.ps2");
    std::fs::write(&card, blank_card(8192, true, [0; 8])).unwrap();
    write_saves(&card, &[save("BASLUS-20062GTA3", b"gta")]);

    let out = ok(&data, &["card", "inspect", card.to_str().unwrap()]);
    assert!(out.contains("format ps2.card") && out.contains("BASLUS-20062GTA3") && out.contains("ps2:SLUS-20062"));
    let blank = dir.path().join("Mcd002.ps2");
    std::fs::write(&blank, vec![0xFF; 8_650_752]).unwrap();
    let out = ok(&data, &["card", "inspect", blank.to_str().unwrap()]);
    assert!(out.contains("unformatted"), "{out}");
    assert!(!data.exists(), "inspect doesn't create a data directory");

    let out = ok(&data, &["card", "add", card.to_str().unwrap()]);
    assert!(out.contains("card ps2:Mcd001 (unit mode"), "{out}");
    let out = ok(&data, &["card", "list"]);
    assert!(out.contains("units") && out.contains("BASLUS-20062GTA3"), "{out}");

    // A card in unit mode is addressed by save, not by card.
    let (success, out) = emusync(&data, &["history", "Mcd001"]);
    assert!(!success && out.contains("name a save"), "{out}");

    let out = ok(&data, &["card", "mode", "Mcd001", "whole-file", "--no-sync"]);
    assert!(out.contains("whole-file mode"), "{out}");
    let out = ok(&data, &["card", "list"]);
    assert!(out.contains("whole-file"), "{out}");
    let out = ok(&data, &["history", "Mcd001"]);
    assert!(out.contains("ps2:Mcd001:@card"), "{out}");
}

/// A fresh repository URI under `EMUSYNC_S3_TEST_URI` for one test run.
fn s3_test_uri(tag: &str) -> Option<String> {
    let Ok(base) = std::env::var("EMUSYNC_S3_TEST_URI") else {
        eprintln!("skipped: EMUSYNC_S3_TEST_URI not set");
        return None;
    };
    let run = format!(
        "{tag}-{}",
        std::process::id() as u64 * 1_000_003
            + std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64
    );
    Some(match base.split_once('?') {
        Some((path, q)) => format!("{}/{run}?{q}", path.trim_end_matches('/')),
        None => format!("{}/{run}", base.trim_end_matches('/')),
    })
}

#[test]
fn s3_ps2_saves_between_two_data_dirs() {
    let Some(uri) = s3_test_uri("cli-ps2") else { return };
    let dir = tempfile::tempdir().unwrap();
    let (pc, phone) = (dir.path().join("pc"), dir.path().join("phone"));
    let (pc_card, phone_card) = (pc.join("Mcd001.ps2"), phone.join("Mcd001.ps2"));
    for (d, card) in [(&pc, &pc_card), (&phone, &phone_card)] {
        std::fs::create_dir_all(d).unwrap();
        std::fs::write(card, vec![0xFF; 8_650_752]).unwrap(); // as PCSX2 creates them
    }
    let (pc_data, phone_data) = (pc.join("data"), phone.join("data"));
    ok(&pc_data, &["--device-name", "pc", "remote", "add", &uri]);
    ok(&pc_data, &["card", "add", pc_card.to_str().unwrap()]);
    ok(&phone_data, &["--device-name", "phone", "remote", "add", &uri]);
    ok(&phone_data, &["card", "add", phone_card.to_str().unwrap()]);

    // The PC formats its card and saves GTA; the phone, offline, saves Ratchet & Clank.
    std::fs::write(&pc_card, blank_card(8192, true, [0; 8])).unwrap();
    write_saves(&pc_card, &[save("BASLUS-20062GTA3", &vec![1; 200_000])]);
    let out = ok(&pc_data, &["sync"]);
    assert!(out.contains("uploaded 1 version"), "{out}");
    std::fs::write(&phone_card, blank_card(8192, true, [0; 8])).unwrap();
    write_saves(&phone_card, &[save("BASCUS-97199RAC", b"rac")]);
    let out = ok(&phone_data, &["sync"]);
    assert!(!out.contains("CONFLICT") && out.contains("applied   ps2:Mcd001:BASLUS-20062GTA3"), "{out}");
    let out = ok(&pc_data, &["sync"]);
    assert!(out.contains("applied   ps2:Mcd001:BASCUS-97199RAC"), "{out}");
    assert_eq!(save_names(&pc_card), ["BASLUS-20062GTA3", "BASCUS-97199RAC"]);
    assert_eq!(save_names(&phone_card), ["BASCUS-97199RAC", "BASLUS-20062GTA3"]);

    // Edit on the phone, see it on the PC, and walk the save's history.
    write_saves(&phone_card, &[save("BASLUS-20062GTA3", b"later")]);
    ok(&phone_data, &["sync"]);
    ok(&pc_data, &["sync"]);
    let history = ok(&pc_data, &["history", "BASLUS-20062GTA3"]);
    assert_eq!(history.lines().filter(|l| l.contains(" pc ") || l.contains(" phone ")).count(), 2, "{history}");
    let first = history.lines().find(|l| l.contains("first")).unwrap();
    ok(&pc_data, &["restore", "BASLUS-20062GTA3", first.split_whitespace().next().unwrap()]);
    ok(&phone_data, &["sync"]);
    let phone_img = std::fs::read(&phone_card).unwrap();
    let gta = Ps2Card.extract_all(&phone_img).unwrap().into_iter().find(|c| c.name == "BASLUS-20062GTA3").unwrap();
    let ApplyOp::Put(original) = save("BASLUS-20062GTA3", &vec![1; 200_000]) else { unreachable!() };
    assert_eq!(gta.bytes, original.bytes);
}

#[test]
fn s3_sync_between_two_data_dirs() {
    let Some(uri) = s3_test_uri("cli") else { return };
    let dir = tempfile::tempdir().unwrap();
    let (pc, phone) = (dir.path().join("pc"), dir.path().join("phone"));
    for d in [&pc, &phone] {
        std::fs::create_dir_all(d.join("memcards")).unwrap();
    }
    let pc_card = pc.join("memcards/Mcd001.ps2");
    let phone_card = phone.join("memcards/Mcd001.ps2");
    let image: Vec<u8> = (0..1_200_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
    std::fs::write(&pc_card, &image).unwrap();
    std::fs::write(&phone_card, vec![0xFFu8; 1_200_000]).unwrap();
    let pc_data = pc.join("data");
    let phone_data = phone.join("data");

    ok(&pc_data, &["--device-name", "pc", "remote", "add", &uri]);
    ok(&pc_data, &["card", "add", pc_card.to_str().unwrap()]);
    let out = ok(&pc_data, &["sync"]);
    assert!(out.contains("uploaded 1 version"), "{out}");

    // The phone has a blank card: first sync is a conflict, resolved by taking the PC's card.
    ok(&phone_data, &["--device-name", "phone", "remote", "add", &uri]);
    ok(&phone_data, &["card", "add", phone_card.to_str().unwrap()]);
    let out = ok(&phone_data, &["sync"]);
    assert!(out.contains("CONFLICT"), "{out}");
    let conflicts = ok(&phone_data, &["conflicts"]);
    let pc_version = conflicts
        .lines()
        .find(|l| l.contains(" pc "))
        .and_then(|l| l.split_whitespace().next())
        .unwrap_or_else(|| panic!("{conflicts}"))
        .to_string();
    ok(&phone_data, &["resolve", "Mcd001", "--take", &pc_version]);
    assert_eq!(std::fs::read(&phone_card).unwrap(), image);

    // Play on the phone, sync back to the PC.
    let mut edited = image.clone();
    edited[600_000..600_512].copy_from_slice(&[0x42; 512]);
    std::fs::write(&phone_card, &edited).unwrap();
    ok(&phone_data, &["sync"]);
    let out = ok(&pc_data, &["sync"]);
    assert!(out.contains("applied"), "{out}");
    assert_eq!(std::fs::read(&pc_card).unwrap(), edited);

    // History has everything, and an old version can be restored.
    let history = ok(&pc_data, &["history", "Mcd001"]);
    assert!(history.lines().count() >= 5, "{history}");
    let first = history.lines().find(|l| l.contains("first") && l.contains(" pc ")).unwrap();
    let id = first.split_whitespace().next().unwrap();
    ok(&pc_data, &["restore", "Mcd001", id]);
    assert_eq!(std::fs::read(&pc_card).unwrap(), image);
    ok(&phone_data, &["sync"]);
    assert_eq!(std::fs::read(&phone_card).unwrap(), image);
}

#[test]
fn service_needs_a_remote_and_reports_status() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let (success, out) = emusync(&data, &["service", "install", "--no-start"]);
    assert!(!success && out.contains("no remote configured"), "{out}");
    let out = ok(&data, &["service", "status"]);
    assert!(out.contains("watcher: not running"), "{out}");
    assert!(out.contains(&format!("{}", data.join("logs").join("watch.log").display())), "{out}");
    let out = ok(&data, &["service", "stop"]);
    assert!(out.contains("not running"), "{out}");
}

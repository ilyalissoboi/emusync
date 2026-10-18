//! CLI tests. The S3 round trip is gated behind `EMUSYNC_S3_TEST_URI` (plus `AWS_ACCESS_KEY_ID`
//! / `AWS_SECRET_ACCESS_KEY`), e.g. a local moto server or MinIO:
//! `EMUSYNC_S3_TEST_URI="s3://emusync-test/ci?endpoint=http://127.0.0.1:9000&path_style=1"`.

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

#[test]
fn s3_sync_between_two_data_dirs() {
    let Ok(base) = std::env::var("EMUSYNC_S3_TEST_URI") else {
        eprintln!("skipped: EMUSYNC_S3_TEST_URI not set");
        return;
    };
    // A fresh repository prefix per run.
    let run = format!(
        "cli-{}",
        std::process::id() as u64 * 1_000_003
            + std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64
    );
    let uri = match base.split_once('?') {
        Some((path, q)) => format!("{}/{run}?{q}", path.trim_end_matches('/')),
        None => format!("{}/{run}", base.trim_end_matches('/')),
    };
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

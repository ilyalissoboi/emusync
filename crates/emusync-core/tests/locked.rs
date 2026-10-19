//! Cards another program has locked. On Windows, PCSX2 opens card files, and the files of a
//! folder card's loaded saves, without sharing for as long as a game runs.
#![cfg(windows)]

use emusync_core::{ContainerDesc, Engine, EngineOptions, LOCKED};
use emusync_remote::MemoryRemote;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn device(dir: &Path, name: &str, remote: &Arc<MemoryRemote>) -> (Engine, PathBuf, PathBuf) {
    let cards = dir.join(name);
    std::fs::create_dir_all(&cards).unwrap();
    let (c1, c2) = (cards.join("Mcd001.bin"), cards.join("Mcd002.bin"));
    std::fs::write(&c1, b"one v1").unwrap();
    std::fs::write(&c2, b"two v1").unwrap();
    let opts = EngineOptions { device_name: Some(name.into()), ..Default::default() };
    let mut e = Engine::open(cards.join("data"), opts).unwrap();
    e.add_remote(remote.clone()).unwrap();
    for c in [&c1, &c2] {
        e.add_container(ContainerDesc { location: c.clone(), ..Default::default() }).unwrap();
    }
    e.sync().unwrap();
    (e, c1, c2)
}

#[test]
fn a_locked_card_is_skipped_and_the_rest_syncs() {
    let dir = tempfile::tempdir().unwrap();
    let remote = Arc::new(MemoryRemote::default());
    let (mut a, a1, a2) = device(dir.path(), "a", &remote);
    let (mut b, b1, b2) = device(dir.path(), "b", &remote);

    std::fs::write(&b1, b"one v2").unwrap();
    std::fs::write(&b2, b"two v2").unwrap();
    b.sync().unwrap();

    // Another program holds Mcd001 open without sharing: the cycle doesn't fail, Mcd001 is
    // reported as locked and left alone, and Mcd002 still syncs.
    let lock = std::fs::OpenOptions::new().read(true).write(true).share_mode(0).open(&a1).unwrap();
    let r = a.sync().unwrap();
    assert!(r.blocked.iter().any(|x| x.unit == "file:Mcd001:@card" && x.reason == LOCKED), "{:?}", r.blocked);
    assert_eq!(r.applied.len(), 1);
    assert_eq!(std::fs::read(&a2).unwrap(), b"two v2");
    drop(lock);
    assert_eq!(std::fs::read(&a1).unwrap(), b"one v1");

    // Released: the next cycle applies it.
    let r = a.sync().unwrap();
    assert!(r.blocked.is_empty(), "{:?}", r.blocked);
    assert_eq!(std::fs::read(&a1).unwrap(), b"one v2");
}

//! `Engine::check_repository`: credentials that reach another repository at the same URI (a
//! sign-in to another Dropbox account) are refused before they are used (DESIGN §30.2).

use emusync_core::{Engine, EngineOptions, Error};
use emusync_remote::{MemoryRemote, Remote};
use std::sync::Arc;

fn engine(dir: &std::path::Path, name: &str) -> Engine {
    let opts = EngineOptions { device_name: Some(name.into()), ..Default::default() };
    Engine::open(dir.join(name), opts).unwrap()
}

#[test]
fn another_repository_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mine = Arc::new(MemoryRemote::default());
    let empty = MemoryRemote::default();
    let other = Arc::new(MemoryRemote::default());

    // Not bound yet: anything goes.
    let mut a = engine(dir.path(), "a");
    a.check_repository(&empty).unwrap();

    a.add_remote(mine.clone()).unwrap();
    a.check_repository(&*mine).unwrap();
    // No repository there, or another one: another account.
    assert!(matches!(a.check_repository(&empty), Err(Error::Incompatible(_))));
    let mut b = engine(dir.path(), "b");
    b.add_remote(other.clone()).unwrap();
    let e = a.check_repository(&*other).unwrap_err();
    assert!(matches!(e, Error::Incompatible(_)) && e.to_string().contains("another account"), "{e}");
    // A remote that can't be reached says so, rather than passing.
    mine.set_offline(true);
    assert!(a.check_repository(&*mine).unwrap_err().is_offline());
    assert_eq!(mine.describe(), other.describe());
}

//! Backend conformance suite (DESIGN §13). Every [`Remote`] must pass it; run it from a test
//! against a fresh remote (`run(&remote)`). It works in its own random namespace, so it can run
//! against a shared bucket.

use crate::{create_only, probe_conditional_writes, PutOutcome, Remote, Result};

/// What the suite found out about the backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub conditional_writes: bool,
}

/// Runs every check, panicking with a description on the first failure.
pub fn run(remote: &dyn Remote) -> Result<Report> {
    let ns = format!("conformance/{}/", uuid::Uuid::now_v7());
    let k = |s: &str| format!("{ns}{s}");

    // Missing objects.
    assert_eq!(remote.get(&k("missing"))?, None, "get of a missing key must be None");
    assert!(!remote.head(&k("missing"))?, "head of a missing key must be false");

    // Conditional-write probe, then create-only semantics in whichever mode applies.
    let conditional = probe_conditional_writes(remote)?;
    for (i, mode) in [conditional, false].into_iter().enumerate() {
        let key = k(&format!("create-only-{i}"));
        assert_eq!(create_only(remote, &key, b"first", mode)?, PutOutcome::Created);
        assert_eq!(
            create_only(remote, &key, b"second", mode)?,
            PutOutcome::AlreadyExists,
            "second create-only write must report AlreadyExists (conditional={mode})"
        );
        assert_eq!(remote.get(&key)?.as_deref(), Some(&b"first"[..]), "create-only write must not overwrite");
        assert!(remote.head(&key)?);
    }

    // Binary round trip, including an empty object.
    let blob: Vec<u8> = (0..70_000u32).map(|i| (i * 31 % 251) as u8).collect();
    remote.put(&k("bin"), &blob, conditional)?;
    assert_eq!(remote.get(&k("bin"))?, Some(blob));
    remote.put(&k("empty"), b"", true)?;
    assert_eq!(remote.get(&k("empty"))?, Some(Vec::new()));

    // Ordered listing with start-after, as used for log segments.
    let log = k("devices/dev-a/log/");
    for seq in 1..=5u64 {
        remote.put(&format!("{log}{seq:016x}.ndjson"), format!("{seq}").as_bytes(), true)?;
    }
    remote.put(&k("devices/dev-b/log/0000000000000001.ndjson"), b"b", true)?;
    remote.put(&k("devices/dev-b/device.json"), b"{}", true)?;
    let all = remote.list(&log, None)?;
    let expect: Vec<String> = (1..=5u64).map(|s| format!("{log}{s:016x}.ndjson")).collect();
    assert_eq!(all, expect, "list must return every key under the prefix in lexical order");
    let after = remote.list(&log, Some(&expect[1]))?;
    assert_eq!(after, expect[2..].to_vec(), "list must honour start-after");
    assert!(remote.list(&log, Some(&expect[4]))?.is_empty());
    assert!(remote.list(&k("nothing/"), None)?.is_empty());

    // Directory listing, as used to discover devices.
    assert_eq!(remote.list_dirs(&k("devices/"))?, vec!["dev-a".to_string(), "dev-b".to_string()]);

    Ok(Report { conditional_writes: conditional })
}

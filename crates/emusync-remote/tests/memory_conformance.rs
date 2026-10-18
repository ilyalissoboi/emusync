use emusync_remote::{conformance, MemoryRemote, MemoryRemoteOptions};

#[test]
fn memory_remote_with_conditional_writes() {
    let r = MemoryRemote::default();
    let report = conformance::run(&r).unwrap();
    assert!(report.conditional_writes);
}

#[test]
fn memory_remote_without_conditional_writes_falls_back_to_head() {
    let r = MemoryRemote::new(MemoryRemoteOptions { conditional_writes: false, ..Default::default() });
    let report = conformance::run(&r).unwrap();
    assert!(!report.conditional_writes, "probe must detect missing If-None-Match support");
}

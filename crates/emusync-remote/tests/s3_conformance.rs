//! S3 conformance, gated behind `EMUSYNC_S3_TEST_URI` (for example
//! `s3://emusync-test/ci?endpoint=http://127.0.0.1:9000&path_style=1` for a local MinIO).
//! Credentials come from `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`.

use emusync_remote::{conformance, EnvCredentials, S3Config, S3Remote};

fn remote() -> Option<S3Remote> {
    let uri = std::env::var("EMUSYNC_S3_TEST_URI").ok()?;
    Some(S3Remote::new(S3Config::parse(&uri).unwrap(), Box::new(EnvCredentials)).unwrap())
}

#[test]
fn s3_conformance() {
    let Some(r) = remote() else {
        eprintln!("skipped: EMUSYNC_S3_TEST_URI not set");
        return;
    };
    let report = conformance::run(&r).unwrap();
    eprintln!("conditional writes supported: {}", report.conditional_writes);
}

#[test]
fn s3_conformance_paginated_listing() {
    let Some(mut r) = remote() else {
        eprintln!("skipped: EMUSYNC_S3_TEST_URI not set");
        return;
    };
    r.max_keys = Some(2);
    conformance::run(&r).unwrap();
}

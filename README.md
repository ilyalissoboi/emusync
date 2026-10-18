# emusync

Native library enabling seamless cloud-based save sync between game emulators on different
platforms. Design: [docs/DESIGN.md](docs/DESIGN.md). C API contract: [include/emusync.h](include/emusync.h).

**Status: M0.** Memory cards sync as whole files (byte-identical mirrors) through Amazon S3 or
any S3-compatible service, with full version history and conflict detection. Per-save sync
for PS2 cards is M1.

## Crates

| Crate | Purpose |
|---|---|
| `emusync-core` | Data model, version DAG, SQLite state, sync engine |
| `emusync-formats` | Container format adapters (M0: `raw`) |
| `emusync-remote` | `Remote` trait, S3 backend (SigV4 + rustls), in-memory remote, conformance suite |
| `emusync-ffi` | C ABI (stub in M0) |
| `emusyncd` | The `emusync` command-line tool |

## Quick start

```sh
cargo build --release
export AWS_ACCESS_KEY_ID=... AWS_SECRET_ACCESS_KEY=...

# On every device (the card name defaults to the file stem, so Mcd001.ps2 matches everywhere):
emusync remote add "s3://my-saves/emusync/v1?region=eu-west-1"
emusync card add ~/PCSX2/memcards/Mcd001.ps2
emusync sync

emusync card list
emusync history Mcd001
emusync restore Mcd001 <version>
emusync conflicts
emusync resolve Mcd001 --take <version>    # or --keep-local
```

For MinIO or other S3-compatible services:
`s3://<bucket>/<prefix>?endpoint=http://nas.local:9000&path_style=1`.

## Tests

```sh
cargo test
```

The S3 conformance suite and the CLI round trip run against a real endpoint when
`EMUSYNC_S3_TEST_URI` and `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` are set. Any
S3-compatible server works. Community MinIO builds are no longer distributed, so locally
the easiest option is [moto](https://github.com/getmoto/moto)'s server, with signature
checking turned on:

```sh
pip install "moto[server]"
INITIAL_NO_AUTH_ACTION_COUNT=3 moto_server -p 5056 &
# create an IAM user + access key and the bucket "emusync-test" (the first 3 calls are unauthenticated)
EMUSYNC_S3_TEST_URI="s3://emusync-test/ci?endpoint=http://127.0.0.1:5056&path_style=1" cargo test
```

`EMUSYNC_SIM_SEEDS=200 cargo test --release -p emusync-core --test sim` runs a longer
randomized partition simulation.

## License

MIT OR Apache-2.0.

# emusync

Native library enabling seamless cloud-based save sync between game emulators on different
platforms. Design: [docs/DESIGN.md](docs/DESIGN.md). C API contract: [include/emusync.h](include/emusync.h).

**Status: M1.** PS2 memory cards (`.ps2`, the PCSX2/ARMSX2 default) sync **per save** through
Amazon S3 or any S3-compatible service: saves of different games played offline on different
devices merge without conflicts, and every save has its own version history. Other card
formats sync as whole files (byte-identical mirrors). Folder cards and a background watcher
for unmodified PCSX2 come next.

## Crates

| Crate | Purpose |
|---|---|
| `emusync-core` | Data model, version DAG, SQLite state, sync engine |
| `emusync-formats` | Container format adapters: `ps2.card` (PS2 memory card file system), `raw` |
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
emusync sync                               # run with the emulator closed

emusync card list                          # cards and the saves on them
emusync card inspect Mcd002.ps2            # list the saves in any card file (read-only)
emusync history BASLUS-20062GTA3           # a save's versions
emusync restore BASLUS-20062GTA3 <version>
emusync conflicts
emusync resolve BASLUS-20062GTA3 --take <version>    # or --keep-local
```

A new card that PCSX2 created but nothing has formatted yet (every byte `0xFF`) counts as
empty; emusync formats it when the first save arrives. Cards synced by M0 as whole files keep
working; to switch one to per-save sync, run `emusync card add <file>` again on each device,
then `emusync card mode Mcd001 units` on one of them. `emusync card mode Mcd001 whole-file`
goes back to byte-identical mirrors.

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
eval "$(python .github/scripts/moto_setup.py | sed 's/^/export /')"
EMUSYNC_S3_TEST_URI="s3://emusync-test/ci?endpoint=http://127.0.0.1:5056&path_style=1" cargo test
```

`EMUSYNC_SIM_SEEDS=200 cargo test --release -p emusync-core --test sim` runs a longer
randomized partition simulation.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion
in this work by you, as defined in the Apache-2.0 license, shall be dual licensed as above,
without any additional terms or conditions.

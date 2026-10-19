# emusync

Native library enabling seamless cloud-based save sync between game emulators on different
platforms. Design: [docs/DESIGN.md](docs/DESIGN.md). C API contract: [include/emusync.h](include/emusync.h).

**Status: M1.** PS2 memory cards (`.ps2`, the PCSX2/ARMSX2 default) sync **per save** through
Amazon S3 or any S3-compatible service: saves of different games played offline on different
devices merge without conflicts, and every save has its own version history. PCSX2 folder
cards hold the same saves, so a folder card on one device syncs with a `.ps2` file on another.
Other card formats sync as whole files (byte-identical mirrors). `emusync watch` keeps
unmodified PCSX2 in sync with no manual steps, and `emusync service install` starts it at
every login.

## Crates

| Crate | Purpose |
|---|---|
| `emusync-core` | Data model, version DAG, SQLite state, sync engine |
| `emusync-formats` | Container format adapters: `ps2.card` (PS2 memory card file system), `ps2.card.folder` (PCSX2 folder cards), `raw` |
| `emusync-remote` | `Remote` trait, S3 backend (SigV4 + rustls), in-memory remote, conformance suite |
| `emusync-ffi` | C ABI (stub in M0) |
| `emusyncd` | The `emusync` command-line tool, including the `emusync watch` sidecar; `emusyncw`, the same tool without a console window on Windows |

## Quick start

```sh
cargo build --release
export AWS_ACCESS_KEY_ID=... AWS_SECRET_ACCESS_KEY=...

# On every device (the card name defaults to the file stem, so Mcd001.ps2 matches everywhere):
emusync remote add "s3://my-saves/emusync/v1?region=eu-west-1"
emusync card add ~/PCSX2/memcards/Mcd001.ps2
# (a PCSX2 folder card is added the same way: emusync card add ~/PCSX2/memcards/Mcd001)
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

## Watching PCSX2 (sidecar mode)

`emusync watch` runs in the foreground next to an unmodified emulator and syncs without any
manual steps:

```sh
emusync remote add "s3://my-saves/emusync/v1?region=eu-west-1"   # once per device
emusync watch                                                    # leave it running
```

- **Cards:** it picks up every card file in the emulator's default folder and registers it
  (PCSX2: `Documents\PCSX2\memcards` on Windows, `~/Library/Application Support/PCSX2/memcards`
  on macOS, `~/.config/PCSX2/memcards` or the Flatpak equivalent on Linux). Cards added with
  `emusync card add` are watched too. `--no-discover` limits it to those.
- **Uploads:** when a card file changes, it syncs once the writes have been quiet for 2 s
  (`--debounce-ms`), and again when the emulator exits. **On Windows**, PCSX2 locks a card's
  files while a game runs, so nothing can read them. The watcher logs `waiting   <card>: PCSX2
  has it open` once, and your saves upload as soon as you close the game or exit PCSX2. On
  macOS and Linux they upload while you play.
- **Downloads never touch an open card.** While PCSX2 is running, its cards are left alone:
  newer versions from other devices are fetched and wait (`held back` in the log), then are
  applied as soon as PCSX2 exits. Your own saves still upload while you play.
- **Polling:** it checks the remote every 30 s, doubling up to 15 min while nothing changes
  (`--poll-min-secs`, `--poll-max-secs`), and right away when PCSX2 starts. Any change resets
  the interval, so downloads are usually in place before the next launch.
- **Conflicts:** the newest version wins (`--policy newest`, the default here) and the other
  stays in `emusync history`, so nothing is lost. `--policy ask` records conflicts for
  `emusync resolve` instead.

The log (stderr, with UTC times) says what was applied, uploaded or held back; `-v` adds
debug detail. When stderr is not a terminal (as when started at login), the log goes to
`logs/watch.log` in the data directory instead, rotated at 1 MiB with three old files kept.
Ctrl+C (or SIGTERM, or `emusync service stop`) finishes the current cycle, then does a final
sync unless the emulator is running; a second Ctrl+C exits at once. Card writes are atomic, so
stopping at any point is safe. Only one watcher runs per data directory. Don't run
`emusync sync` against the same data directory while the watcher is running, since it doesn't
know the emulator is open.

### Starting at login

```sh
emusync service install     # start `emusync watch` at every login, and now
emusync service status      # installed? running? the end of the log
emusync service stop        # stop after a final sync (until next login or `service start`)
emusync service start
emusync service uninstall   # stop and stop starting at login
```

`service install` registers the watcher for your user only, without admin rights: a Task
Scheduler logon task named `emusync` on Windows (running `emusyncw.exe`, so no console window
opens; keep it next to `emusync.exe`), a LaunchAgent
(`~/Library/LaunchAgents/io.github.ilyalissoboi.emusync.plist`) on macOS, and a systemd user
unit (`~/.config/systemd/user/emusync.service`) on Linux, including the Steam Deck. It takes
the options of `emusync watch` (`--profile`, `--policy`, `--no-discover`, …) plus `--no-start`;
run it again to change them. The data directory is fixed at install time. A watcher that crashes
is restarted; one that was stopped is not, until the next login.

**Credentials.** A watcher started at login doesn't see variables set in your shell, and
`service install` never copies your keys anywhere. Either:

- on Windows, save `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` as user environment
  variables (`setx AWS_ACCESS_KEY_ID …`, or Settings > System > About > Advanced system
  settings > Environment Variables); the logon task inherits them; or
- put them in a file only you can read, and pass it to the watcher:

  ```sh
  mkdir -p ~/.config/emusync
  touch ~/.config/emusync/credentials.env
  chmod 600 ~/.config/emusync/credentials.env
  # then edit it to hold two lines: AWS_ACCESS_KEY_ID=… and AWS_SECRET_ACCESS_KEY=…
  emusync service install --env-file ~/.config/emusync/credentials.env
  ```

  The file holds `KEY=VALUE` lines (`#` comments and `export` allowed) and is read at each
  start, so rotating keys only needs a `service stop` / `service start`. `emusync watch
  --env-file` works the same in a terminal.

On Windows, logging off ends the watcher without a final sync; anything not yet uploaded is
uploaded at the next start.

Emulators are described by JSON profiles (`crates/emusyncd/profiles/`): process names, default
card folders and card file extensions for each OS, plus optional lock files that exist only
while the emulator runs (each OS section needs at least one of the two). `--profile
my-emulator.json` loads another one, and `--profile` can be
repeated. Cards that no watched profile claims are written only while none of the watched
emulators is running.

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

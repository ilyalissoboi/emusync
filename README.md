# emusync

Native library enabling seamless cloud-based save sync between game emulators on different
platforms. Design: [docs/DESIGN.md](docs/DESIGN.md). C API contract: [include/emusync.h](include/emusync.h).

**Status: M2.** PS2 memory cards (`.ps2`, the PCSX2/ARMSX2 default) sync **per save** through
Amazon S3 or any S3-compatible service: saves of different games played offline on different
devices merge without conflicts, and every save has its own version history. PCSX2 folder
cards hold the same saves, so a folder card on one device syncs with a `.ps2` file on another.
GameCube cards sync per save too, whether Dolphin keeps them as `.raw` images or GCI folders
(see [GameCube cards](#gamecube-cards)). Other card formats sync as whole files (byte-identical mirrors). `emusync watch` keeps
unmodified PCSX2 in sync with no manual steps, and `emusync service install` starts it at
every login. Emulators embed it through the C ABI (`include/emusync.h`, ABI v1), as the ARMSX2
Android port does. Repositories can be [end-to-end encrypted](#end-to-end-encryption).

## Crates

| Crate | Purpose |
|---|---|
| `emusync-core` | Data model, version DAG, SQLite state, sync engine |
| `emusync-formats` | Container format adapters: `ps2.card` (PS2 memory card file system), `ps2.card.folder` (PCSX2 folder cards), `gc.card` (GameCube card images), `gc.gci` (Dolphin GCI folders), `raw` |
| `emusync-remote` | `Remote` trait, S3 backend (SigV4 + rustls), in-memory remote, conformance suite |
| `emusync-ffi` | C ABI v1 (`include/emusync.h`): static library, or shared with `cargo rustc -p emusync-ffi --lib --crate-type cdylib` ([desktop](#building-for-windows-macos-and-linux), [Android](#building-for-android)) |
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

## GameCube cards

Dolphin keeps a slot's card either as a raw image (`GC/MemoryCardA.USA.raw`) or as a GCI folder
(`GC/USA/Card A`, one `.gci` file per save, Dolphin's default). Both sync per save, and they are
the same logical card, `gc:MemoryCardA.USA`, so one device can use a raw card and another a
GCI folder:

```sh
emusync card add ~/"Dolphin Emulator/GC/MemoryCardA.USA.raw"   # a raw card
emusync card add ~/"Dolphin Emulator/GC/USA/Card A"            # or a GCI folder, even an empty one
emusync card inspect MemoryCardA.EUR.raw                       # list the saves on any card
```

With an emulator integration (Dolphin's), a card or GCI folder that doesn't exist yet on a device
is created when saves arrive for it, as Dolphin would create it, so a game's first boot on a new
device finds them. (`emusync card add` takes only a card that exists.) Saves are named
`<game code><maker>-<file>` (`GM8E01-MetroidPrime A`). Saves that check the card
they are on (Phantasy Star Online, F-Zero GX) work on every card: emusync rewrites the card's
serial into them as Dolphin does. Sync with Dolphin closed: it keeps the card in memory while a
game runs. Details: [DESIGN §25](docs/DESIGN.md#25-implementation-notes-gamecube-cards).

## End-to-end encryption

A repository can be encrypted, so the storage provider (or anyone who gets the bucket) sees
only object names and sizes: no saves, no game names, no device names. Choose it when you
create the repository, on a new prefix:

```sh
# First device: creates the encrypted repository and asks for a new passphrase (twice).
emusync remote add "s3://my-saves/emusync/v2?region=eu-west-1" --encrypt

# Every other device: asks for the passphrase once (a wrong one leaves the remote added but
# locked; `emusync remote unlock` asks again).
emusync remote add "s3://my-saves/emusync/v2?region=eu-west-1"

emusync status            # encryption: end-to-end (unlocked on this device)
emusync remote unlock     # enter the passphrase again, e.g. after `emusync remote lock`
emusync remote lock       # forget this device's copy of the key
```

- **The passphrase is needed once per device.** The repository key it unlocks is kept in the
  data directory (`keys/`): protected with DPAPI for your Windows user on Windows, and in a
  file only your user can read on macOS and Linux. Through the C ABI it is in the data directory
  the app passes, on Android its private storage.
- **It is never a command-line argument and never logged.** On a terminal it is read without
  echo; otherwise (scripts) it is the first line of stdin: `emusync remote unlock < passfile`.
- **It cannot be recovered.** Without it no device can read the repository; keep it in a
  password manager.
- **Encryption is chosen at creation.** An existing unencrypted repository stays unencrypted
  (`--encrypt` on it is refused); to switch, create an encrypted one on a new prefix and add it
  with a new data directory on each device. Unencrypted repositories work exactly as before.
- **Older emusync versions refuse an encrypted repository** ("unsupported repository format"),
  so they never write unencrypted data into it. Update every device first.
- While a device is locked, `emusync sync` and `emusync watch` stop with "run `emusync remote
  unlock`"; nothing is uploaded unencrypted.

Blobs, logs and device descriptions are encrypted with XChaCha20-Poly1305 under a random
256-bit repository key; `repo.json` holds that key wrapped with a key derived from the
passphrase by Argon2id (64 MiB, 3 passes). Blob names are keyed BLAKE3 hashes, so the provider
can't check whether you have a particular save. Details: [DESIGN.md §22](docs/DESIGN.md#22-implementation-notes-end-to-end-encryption).

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

## Building for Windows, macOS and Linux

An emulator links the C ABI as a shared library: `emusync.dll` (with the import library
`emusync.lib`) on Windows, `libemusync.dylib` on macOS, `libemusync.so` on Linux. It is shared
rather than static because it bundles its own SQLite, zstd and ring, and only the `es_*` functions
are exported, so they cannot clash with the emulator's own copies (PCSX2, Dolphin and PPSSPP all
link zstd).

From a CMake build, include [`bindings/cmake/EmuSync.cmake`](bindings/cmake/EmuSync.cmake). It
runs cargo for the platform being built (both architectures, joined into a universal library,
when `CMAKE_OSX_ARCHITECTURES` names `arm64` and `x86_64`), defines the imported target
`emusync::emusync`, and `emusync_deploy_runtime()` copies the library next to the executable and
points the executable's rpath there. Without cargo, rustc or the Rust target it sets
`EMUSYNC_FOUND` to `OFF`, so the emulator can compile the feature out. On Android the same include
uses `EmuSyncAndroid.cmake` (below), so a project that builds desktop and Android from one CMake
tree needs only this:

```cmake
include(${CMAKE_SOURCE_DIR}/Externals/emusync/bindings/cmake/EmuSync.cmake)
if(EMUSYNC_FOUND)
  target_link_libraries(myemu PRIVATE emusync::emusync)
  target_compile_definitions(myemu PRIVATE HAS_EMUSYNC)
  emusync_deploy_runtime(myemu)   # a macOS app bundle: DESTINATION <bundle>/Contents/Frameworks
endif()
```

For a build that doesn't use CMake (PCSX2's Visual Studio solution), stage a package instead:

```sh
scripts/build-desktop.sh                   # this machine's target, release, into target/desktop
scripts/build-desktop.sh --target aarch64-apple-darwin --target x86_64-apple-darwin   # universal
scripts/check-desktop-lib.sh target/desktop/lib/libemusync.so                          # Linux, macOS
```

It holds `include/emusync.h`, and `bin/emusync.dll`, `bin/emusync.pdb` and `lib/emusync.lib` on
Windows, or `lib/libemusync.dylib` / `lib/libemusync.so`. Windows builds need MSVC or clang-cl
(the `*-pc-windows-msvc` targets); the DLL depends only on system libraries and the Visual C++
runtime. [`bindings/cmake/desktop-test`](bindings/cmake/desktop-test/CMakeLists.txt) is a complete
example; CI builds it on Windows (Visual Studio generator), macOS (universal) and Linux, runs the C
smoke test against the deployed library, checks it, and uploads each package as a
`libemusync-desktop-<os>` artifact.

## Keeping an emulator's keys in the system's credential store

An emulator that embeds emusync has S3 keys, and maybe an encryption passphrase, to keep between
runs. Instead of each emulator writing its own storage, the C ABI keeps them in the system's
credential store: Credential Manager on Windows, the Keychain on macOS, the Secret Service
(GNOME Keyring, KWallet) on Linux.

```c
es_secret_set("PCSX2", "s3-secret-access-key", secret, config_dir);

char buf[1024];
size_t len;
if (es_secret_get("PCSX2", "s3-secret-access-key", config_dir, buf, sizeof buf, &len) == ES_OK)
    /* use buf */;
```

They need no client, so the keys can be read before sync starts, and they may block while the
system asks the user to unlock the keychain: call them off the UI thread. On Linux without a
running Secret Service (a headless machine, the Steam Deck in game mode) the secret goes to a
file only the user can read under the directory passed last (`<dir>/secrets/<service>/<name>`).
Android and iOS apps use their platform keystore themselves; there the calls fail with
`ES_E_INVALID_ARG`. Details are in [`include/emusync.h`](include/emusync.h).

## Building for Android

The C ABI builds as a shared library, `libemusync.so`, for an Android app's native code to link
(this is how ARMSX2 is meant to use it). It needs Rust with the Android target and an Android
NDK; cargo-ndk is not needed.

```sh
rustup target add aarch64-linux-android        # and x86_64-linux-android for emulators
export ANDROID_NDK_HOME=~/Android/Sdk/ndk/29.0.14206865
scripts/build-android.sh                       # --target x86_64-linux-android, --api 26, --debug, --out DIR
scripts/check-android-so.sh target/android/arm64-v8a/libemusync.so
```

The library lands in `target/android/<abi>/libemusync.so`; ship it with
[`include/emusync.h`](include/emusync.h). It is built for API 26 with the NDK's clang, has the
soname `libemusync.so`, is aligned for 16 KB pages, needs only `libc`, `libm` and `libdl`, and
exports only the `es_*` functions: the SQLite, zstd and ring code inside it stays private, so it
cannot clash with the app's own copies. The scripts run on Linux, macOS and Windows (Git Bash).

From an app's CMake build (the Android Gradle Plugin's `externalNativeBuild`), include
[`bindings/cmake/EmuSyncAndroid.cmake`](bindings/cmake/EmuSyncAndroid.cmake) instead. It runs
cargo for the ABI being built, defines the imported target `emusync::emusync`, and copies
`libemusync.so` into the per-ABI library directory so that it is packaged in the APK. Without
cargo, rustc or the Rust target it sets `EMUSYNC_ANDROID_FOUND` to `OFF` and adds nothing, so the
app can compile the feature out:

```cmake
include(${CMAKE_SOURCE_DIR}/3rdparty/emusync/bindings/cmake/EmuSyncAndroid.cmake)
if(EMUSYNC_ANDROID_FOUND)
  target_link_libraries(emucore PRIVATE emusync::emusync)
  target_compile_definitions(emucore PRIVATE HAS_EMUSYNC)
endif()
```

[`bindings/cmake/android-test`](bindings/cmake/android-test/CMakeLists.txt) is a complete
example; CI builds it for arm64-v8a and x86_64, checks the result, runs the C smoke test on an
x86_64 emulator, and uploads `libemusync.so` and `emusync.h` as the `libemusync-android`
artifact.

### The Android AAR

[`bindings/android`](bindings/android) builds `emusync-release.aar` for apps that don't want a
Rust toolchain in their build ([DESIGN §26](docs/DESIGN.md#26-implementation-notes-android-aar)).
The emulator's native code owns the emusync client, as in PCSX2 and ARMSX2. The AAR gives it the
library, and gives the Kotlin side the rest of an integration:

- `libemusync.so` (arm64-v8a, x86_64) packaged into the APK, and a Prefab package so the native
  build finds it:

  ```cmake
  find_package(emusync REQUIRED CONFIG)   # needs buildFeatures { prefab = true }
  target_link_libraries(emucore PRIVATE emusync::emusync)
  ```

- `SyncSettings`: storage address, keys, passphrase and device name, with the secrets encrypted
  by an Android Keystore key, plus emusync's data folder for each storage (not backed up).
- `SyncController`: starts and stops sync in the background to match the settings, and reports
  when the app goes to the background and comes back. It drives the native side through
  `SyncBackend`, a small interface the app implements over its own JNI:

  ```kotlin
  object NativeSync : SyncBackend {          // each call goes to the app's own JNI functions
      override val isAvailable get() = NativeApp.syncAvailable()
      override fun start(config: SyncConfig) = NativeApp.syncStart(config.dataDir.path,
          config.deviceName, config.appVersion, config.remoteUri, config.accessKeyId,
          config.secretAccessKey, config.passphrase)   // null when started, else the reason
      override fun stop() = NativeApp.syncStop()
      override val isRunning get() = NativeApp.syncRunning()
      override fun syncNow() = NativeApp.syncNow()
      override fun status() = NativeApp.syncStatus()
      override fun setAppBackgrounded(backgrounded: Boolean) = NativeApp.syncBackgrounded(backgrounded)
  }

  val sync = SyncController(SyncSettings(context), NativeSync, BuildConfig.VERSION_NAME)
  sync.attachToAppLifecycle()     // once, on the main thread
  sync.startInBackground()        // at app start, and startAsync { error -> ... } after a change
  ```

Building it needs Rust with the Android targets and the NDK (r29), like the library alone; on
Windows the build calls `scripts/build-android.sh` through Git's bash.

```sh
cd bindings/android
./gradlew :emusync:assembleRelease       # emusync/build/outputs/aar/emusync-release.aar
```

Each release (a `vX.Y.Z` tag) publishes `emusync-X.Y.Z.aar` on the
[Releases](https://github.com/ilyalissoboi/emusync/releases) page with its SHA-256; an app
downloads a pinned version in its build. It adds the `.aar` and the one library it depends on:

```kotlin
implementation(files("libs/emusync-release.aar"))
implementation("androidx.lifecycle:lifecycle-process:2.11.0")
```

CI builds it, checks the libraries inside, runs its instrumented tests and a Prefab consumer
([`prefab-test`](bindings/android/prefab-test)) on an emulator, and uploads it as the
`emusync-aar` artifact.

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

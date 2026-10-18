# emusync — Design

A native library that keeps emulator save data (memory cards, SRAM, save folders, and
optionally save states) in sync across devices and operating systems through the cloud.

**Driving use case:** play a PS2 game in PCSX2 on Windows, continue in PCSX2 on macOS, then
continue in ARMSX2 on Android. Memory card progress follows you with no manual copying. If you
play on two devices while offline, nothing is lost.

Status: design draft, v0.1. The C API contract is sketched in [`include/emusync.h`](../include/emusync.h).

---

## 1. Goals and non-goals

### Goals

1. **Never lose a save.** Every version that is overwritten, locally or remotely, can be
   recovered. This rule outranks every other goal.
2. **Sync per save, not per file.** A PS2 memory card holds saves for many games. Two devices
   playing *different* games offline must merge cleanly, with no conflict.
3. **Work across emulators and layouts.** PCSX2 with an 8 MB `.ps2` file card, PCSX2 with a
   folder card, and ARMSX2 with a per-game card all hold the same logical saves and must
   interoperate.
4. **Embeddable.** A small C ABI that emulators (C++, Kotlin/JNI, Swift) can link without
   pulling in an async runtime, a GUI, or GPL-incompatible terms.
5. **Bring your own storage.** Works with a reference sync server, but also with plain object or
   file storage (S3, WebDAV, Dropbox, Google Drive, OneDrive, iCloud Drive, or a local folder
   synced by Syncthing). None of these may be required.
6. **Mobile-friendly.** Survives Android process death, metered networks, scoped storage (SAF)
   and short background time budgets.

### Non-goals (v1)

- Merging the *contents* of a single save. Game saves are opaque binary data with
  game-specific checksums, so the library picks versions and never splices bytes.
- Making save states portable across incompatible emulator builds. The library carries states
  and filters them by compatibility. It never converts them.
- Netplay or real-time shared sessions.
- Hosting ROMs, BIOS files, or any copyrighted content. Only user-generated save data is synced.

---

## 2. The core idea: containers vs. save units

Emulators store saves in **containers**: physical files or folders with emulator-specific
layouts. The library breaks each container into **save units**, the smallest pieces of data a
game owns on its own, and syncs those.

| Platform | Container formats (local) | Save unit | Title identity |
|---|---|---|---|
| PS2 | `.ps2` raw MCFS image (8–64 MB, with ECC); PCSX2 folder card | Root directory on the card (`BASLUS-20312…`) | Disc serial (`SLUS-20312`) |
| PS1 | `.mcd`/`.mcr`/`.srm` raw 128 KB; `.gme`/`.vgs`/`.vmp` headered | Save block chain (1–15 blocks) | Serial in the block name |
| GameCube | `.raw` card image; Dolphin GCI folder | GCI file | Game code + maker code |
| Cartridge systems | `.srm`, `.sav`, `.eep`, `.fla` (RetroArch, mGBA, …) | The whole file, normalised | ROM hash (No-Intro SHA-1/CRC) |
| PSP | `SAVEDATA/<dir>` | Directory | Directory prefix (`ULUS10041`) |
| Anything else | Any file or directory | The whole file or directory (opaque) | Set by the host |

Because sync works on units, a PS2 save made on Windows in a shared `Mcd001.ps2` can be written
into the per-game card ARMSX2 uses on Android, and back again. Only the unit has to match.
Cards never have to be byte-identical.

```
 Windows (PCSX2)            cloud repository                 Android (ARMSX2)
 ┌──────────────┐          ┌───────────────────┐           ┌─────────────────┐
 │ Mcd001.ps2   │ extract  │ unit ps2:BASLUS-  │  apply    │ SLUS-20312.ps2  │
 │  ├ BASLUS-203│────────▶ │  20312GTA3        │ ────────▶ │  └ BASLUS-203…  │
 │  ├ BESLES-50…│          │  v1 → v2 → v3     │           │                 │
 │  └ BASCUS-97…│          │ unit ps2:BESLES-… │           │ Mcd001.ps2      │
 └──────────────┘          └───────────────────┘           └─────────────────┘
```

### 2.1 Unit identity

`unit_id = platform ":" namespace ":" native_name`, for example
`ps2:default:BASLUS-20312GTA3`.

- `native_name` is the name inside the container: the PS2 directory name, the PS1 block
  filename, or the GCI `gamecode+filename`.
- `namespace` defaults to `default`. If a device holds two *different* copies of the same unit
  in two containers (a user who keeps a backup card, for example), the second copy gets the
  namespace of its container, so the copies never silently overwrite each other.
- Each unit is tagged with the **titles** it belongs to, parsed from the name (for PS2, the
  `BA`/`BE`/`BI` region prefix plus the serial). The tag decides what to pull before a game
  starts. Saves the parser can't recognise still sync. They are just not pulled ahead of time.

### 2.2 Canonical form and hashing

Each format adapter turns a unit into a **canonical form**:

- PS2: a sorted list of `(path, mode bits, size, bytes)` from the save directory. Directory and
  file timestamps are kept as **metadata** and left out of the content hash, so a game that
  rewrites identical bytes does not create a new version.
- PS1: the save's data blocks in order, plus title/frame metadata. Block-link fields are
  rebuilt on apply and left out of the hash.
- Raw SRAM: the bytes with emulator-specific padding and trailers removed (for example mGBA's
  RTC footer or RetroArch's rounding to a power of two). The trailers are kept as metadata so
  they can be restored on the same emulator.

`content_hash = BLAKE3(canonical_bytes)`. When end-to-end encryption is on, the hash is keyed
(see §7).

---

## 3. Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│ Host: PCSX2 fork / ARMSX2 fork / emusyncd sidecar / companion app   │
└──────────────┬──────────────────────────────────────────────────────┘
               │ C ABI (emusync.h) · C++ RAII header · Kotlin AAR · Swift pkg
┌──────────────▼──────────────────────────────────────────────────────┐
│ emusync-ffi       stable ABI, struct versioning, event queue        │
├─────────────────────────────────────────────────────────────────────┤
│ emusync-core                                                        │
│  ┌──────────────┐ ┌──────────────┐ ┌───────────────┐ ┌───────────┐  │
│  │ Session &    │ │ Sync engine  │ │ Local state DB│ │ Snapshot  │  │
│  │ lease manager│ │ (DAG, merge, │ │ (SQLite: units│ │ store     │  │
│  │              │ │  outbox)     │ │  cursors, …)  │ │ (history) │  │
│  └──────────────┘ └──────────────┘ └───────────────┘ └───────────┘  │
├───────────────────────────────┬─────────────────────────────────────┤
│ emusync-formats (adapters)    │ emusync-remote (backends)           │
│  ps2.card · ps2.card.folder   │  server · s3 · webdav · dropbox ·   │
│  ps1.card · gc.card · gc.gci  │  gdrive · onedrive · folder         │
│  sram · dir · raw             │                                     │
├───────────────────────────────┴─────────────────────────────────────┤
│ Platform layer: VFS (POSIX/Win32/SAF callbacks), atomic replace,    │
│ network reachability, background-task hooks, keystore               │
└─────────────────────────────────────────────────────────────────────┘
```

### 3.1 Implementation language: Rust core with a C ABI

**Recommendation: Rust**, exposing a C ABI generated with `cbindgen` and checked by hand.

- The library parses binary images downloaded from the network (card images and save blobs),
  so they are **untrusted input**. Memory safety plus fuzzing (`cargo fuzz`) on every parser is
  worth a lot here.
- Cross-compiling is first-class for every target: `x86_64/aarch64-pc-windows-msvc`,
  `aarch64/x86_64-apple-darwin`, `aarch64-apple-ios`, `aarch64/armv7/x86_64-linux-android`
  (via `cargo-ndk`), and Linux.
- The ecosystem covers what we need: `rustls`, `rusqlite` (bundled SQLite), `zstd`, `blake3`,
  `chacha20poly1305`, `argon2`, and `fastcdc`.
- C++ emulators link it as a static library through CMake (`corrosion`) or as prebuilt
  binaries. Neither the API nor the build exposes Rust.

The alternative is C++20, which matches emulator codebases and lowers the bar for contributors.
It is reasonable if the maintainers are C++-only, but then parsers must be fuzzed under
ASan/UBSan, and HTTP/TLS needs a dependency such as libcurl plus mbedTLS.

**License:** MIT OR Apache-2.0. Both can be linked into GPLv3 emulators (PCSX2, ARMSX2, Dolphin,
DuckStation) and into closed-source frontends.

### 3.2 Threading model

- The library owns one internal worker runtime (a single-threaded async executor plus a small
  blocking pool for file I/O). The host never has to provide one.
- Every API function is thread-safe. Only `es_session_begin` (with a timeout the caller sets),
  `es_app_suspend` (with a time budget), and explicit `*_wait` calls block. All other calls
  queue work and return within microseconds.
- **Events are pulled, not pushed.** The host calls `es_event_poll` on whichever thread it
  likes, usually the UI thread. An optional `wake` callback tells it "events are available"
  from the library thread. This avoids calling back into emulator code from a foreign thread,
  a common source of deadlocks during VM shutdown.

---

## 4. Data model

```
Repository (one per user / sync space)
 ├─ Device        id (UUIDv7), name, platform, emulator id + version
 ├─ Unit          unit_id, platform, titles[], format_hint
 │   └─ Version   version_id = BLAKE3(header), parents[0..2], content_hash,
 │                manifest (chunk list), meta {timestamps, icon title, size},
 │                origin device, HLC timestamp, playtime_ms, tombstone?
 ├─ State         state_id, title, slot, compat_key, chunk manifest, screenshot,
 │                linked unit versions (see §8), origin device, HLC
 └─ Lease         title, device, expires_at (advisory "playing now" marker)
```

- **Versions form a DAG for each unit**, as in git. A version with one parent is a normal edit.
  A version with two parents records a conflict resolution. The **head** of a unit is its set of
  tips. One tip means a clean state; two or more mean a conflict.
- **Ordering never depends on wall clocks.** Ancestry comes from parent links. Wall time (as a
  hybrid logical clock, HLC, so clock skew between devices does no harm) is used only for display
  and as the tie-break in automatic policies.
- **Tombstones** record deletions as versions, so deleting a save on one device is recoverable
  everywhere.

### 4.1 Local state DB (SQLite, in the host-provided `data_dir`)

| Table | Purpose |
|---|---|
| `containers` | Registered containers: format, location, flags, last-seen fingerprint (size + mtime + quick hash) |
| `local_units` | unit_id → container, current local `content_hash`, **base version** (the last version synced at this location) |
| `versions` | Cache of remote version headers (the full DAG metadata, which is small) |
| `outbox` | Durable queue of local versions waiting to upload, with blobs staged in `data_dir/staging` |
| `inbox` | Remote versions fetched but not yet applied, because their container is in use |
| `cursors` | Read position in each remote device log (§6) |
| `conflicts` | Open conflicts awaiting a policy decision or the user |
| `snapshots` | Local history ring: every unit version this device has ever overwritten |

The outbox and inbox make the engine **crash-safe**. Every step is idempotent and resumes after
process death, which matters most on Android.

---

## 5. Format adapters

```rust
trait ContainerFormat {
    fn id(&self) -> &'static str;                         // "ps2.card"
    fn probe(&self, c: &dyn Container) -> Confidence;     // auto-detection
    fn list(&self, c: &dyn Container) -> Result<Vec<UnitRef>>;           // cheap: names + quick hashes
    fn extract(&self, c: &dyn Container, u: &UnitRef) -> Result<Canonical>;
    fn plan_apply(&self, c: &dyn Container, ops: &[ApplyOp]) -> Result<ApplyPlan>; // checks free space etc.
    fn apply(&self, c: &dyn Container, plan: ApplyPlan, out: &mut dyn Write) -> Result<()>;
}
enum ApplyOp { Put(Canonical), Delete(UnitName) }
```

Every apply follows the same steps, whatever the format:

1. Read the container and parse it fully. If it fails validation, **refuse** and never modify
   it.
2. Build the new image in memory, or in a temporary file for large containers.
3. **Re-parse the new image and re-extract every unit.** Units that were touched must hash to
   their expected values, and untouched units must hash the same as before. Any mismatch aborts
   the apply.
4. Save the previous unit versions to the local snapshot store.
5. Write `name.emusync-tmp`, fsync it, and atomically replace the original (`rename` on POSIX,
   `ReplaceFileW` on Windows). Where atomic replace isn't available (SAF), use a journalled
   copy: write the temp file, record the intent in the DB, copy over the original, verify, and
   clear the intent. Interrupted copies are rolled forward on the next start.

### 5.1 `ps2.card`: raw MCFS image (PCSX2, ARMSX2, AetherSX2/NetherSX2, real-hardware dumps)

- Layout: 528-byte pages (512 data + 16 spare, holding 12 bytes of Hamming ECC per 128-byte
  chunk), 2-page clusters, a superblock (`Sony PS2 Memory Card Format`), indirect FAT → FAT, and
  512-byte directory entries. 8 MB cards are 8,650,752 bytes with ECC and 8,388,608 without.
  PCSX2 also supports 16/32/64 MB cards. All sizes are handled.
- `list` walks the root directory and returns the subdirectories. `.` and `..` are skipped.
- `apply` frees the old unit's clusters, allocates new ones (first-fit, keeping files
  contiguous when it can, as the console does), writes directory entries with the timestamps
  from metadata, updates the FAT, and recomputes ECC for every touched page.
- **Card full:** `plan_apply` fails with `NO_SPACE`. The unit stays in the inbox and the host
  gets `ES_EV_APPLY_BLOCKED` with an option to place the unit in a per-game card (§5.4).
- Prior art to check against: PCSX2's `FolderMemoryCard` (which already converts between folder
  and image at runtime), `mymc`, and `ps2mc-browser`. Golden images from all of them, plus
  console-formatted dumps, go in the test fixtures.

### 5.2 `ps2.card.folder`: PCSX2 folder memory card

- One host directory per save. `_pcsx2_superblock` and `_pcsx2_index` hold the metadata
  (ordering, attributes, timestamps).
- PCSX2 keeps the folder card in memory while a VM runs and writes it back on flush. Anything
  modified externally while it is open is lost. The adapter therefore **never applies to a
  folder card that is open** (§6.3).
- Because it shares the PS2 canonical form with `ps2.card`, a folder card on macOS and a file
  card on Android sync with no conversion step.

### 5.3 Other v1 and v2 adapters

- `ps1.card`: raw 128 KB images plus headered variants. Units are block chains. Apply rebuilds
  the directory frames and link fields.
- `gc.card` / `gc.gci`: Dolphin raw images and GCI folders (v2).
- `sram`: normalises cartridge save files between emulators (v2, with a quirk table for each
  emulator).
- `dir`: any directory tree as one unit (PSP `SAVEDATA/*`, Citra/Azahar and others).
- `raw`: any single file as one opaque unit. This always works and is the fallback.

### 5.4 Placement: where incoming units go

A unit that is new to a device has to land in some container. The rules, in order:

1. If the unit already exists locally, update it in place.
2. If the unit belongs to the title being started, use the container the host reported as
   mounted in slot 1 for this session. That might be a per-game card.
3. Otherwise use the container flagged `ES_CF_DEFAULT_PLACEMENT`.
4. Otherwise keep it in the inbox and place it the first time its title starts. This is lazy
   placement, the default on mobile to save storage.

Containers flagged `ES_CF_MIRROR_ALL` receive every unit for their platform during idle syncs.

---

## 6. Sync protocol

### 6.1 Repository layout that works on plain storage

The repository is designed so that **no file is ever written by more than one device**. That
removes any need for locks or compare-and-swap, so it runs unchanged on S3, WebDAV, Dropbox,
Drive, OneDrive, iCloud, or a Syncthing-shared folder.

```
emusync/v1/
  repo.json                       format version, repo id, KDF params, wrapped key (if E2E)
  blobs/<hh>/<hash>               immutable zstd(+encrypted) chunks, content-addressed
  devices/<device-id>/
    device.json                   name, platform, emulator, last-seen
    log/<seq:016x>.ndjson         append-only log segments; each segment is immutable once written
    compact/<seq:016x>.json       optional snapshot of everything this device has published up to seq
```

Log records (one JSON object per line):

```json
{"op":"version","unit":"ps2:default:BASLUS-20312GTA3","id":"b3:…","parents":["b3:…"],
 "content":"b3:…","chunks":["b3:…"],"size":91136,"meta":{"title":"GTA III","mtime":"…"},
 "hlc":"2026-10-18T11:02:07.120Z/0003","playtime_ms":5520000,"titles":["ps2:SLUS-20062"]}
{"op":"state","id":"…","title":"ps2:SLUS-20062","slot":"resume","compat":"pcsx2:sv=…","chunks":[…],"links":{…}}
{"op":"lease","title":"ps2:SLUS-20062","expires":"…"}
{"op":"lease_release","title":"ps2:SLUS-20062"}
```

A **sync cycle** works like this:

1. **Push.** Upload any blobs in the outbox the remote doesn't have yet (probe with HEAD or a
   bulk `has` call), then write *one new log segment* with this device's new records. Blobs come
   first and the log second, so a log never refers to a missing blob.
2. **Pull.** For every other device, list `log/` segments past the stored cursor, fetch them,
   and merge the records into the local `versions` table. A new device starts from each
   device's latest `compact/` snapshot instead of replaying every log.
3. **Reconcile** each affected unit (§6.2).
4. **Apply** results to containers that are not in use. Results for containers in use go to the
   inbox.

The cost is one LIST per device per cycle. Each device writes a compaction snapshot every N
segments, and segments older than everyone's snapshots can be garbage-collected.

**Reference server (optional):** the same records through an HTTP API, plus server push
(WebSocket or long-poll on desktop, FCM/APNs on mobile), atomic blob dedup, quotas, and a web
UI for history and restore. On plain storage the client polls instead: on title start, on title
end, when the app returns to the foreground, and on a back-off timer (30 s to 15 min).

### 6.2 Reconciliation for each unit

Inputs: `L` (the current local content), `B` (the local base version), and `T` (the set of
remote tips).

| Situation | Action |
|---|---|
| `hash(L) == B.content` and `T == {B}` | Nothing to do |
| Local changed, `T == {B}` | New version `V(parent=B)`. Push it and set base = V |
| Local unchanged, a single tip `R` descends from `B` | **Fast-forward**: apply R, set base = R |
| Local changed **and** a remote tip descends from `B` | **Conflict** (true divergence) |
| More than one remote tip | **Conflict** (another pair of devices diverged) |
| `hash(L) == R.content` for some tip | Adopt R as base with no apply (same bytes reached independently) |

**Conflict policy** (set per client and overridable per title):

- `ask` (the default when a UI is attached): raise `ES_EV_CONFLICT` with both sides' device,
  time, playtime, size, and the in-game title from `icon.sys`. The local container is left
  alone until the conflict is resolved, and play can continue on local data.
- `newest` (the default when headless, as in the sidecar): the higher HLC wins. The losing side
  is **published as a branch tip** and kept, never discarded, so it stays restorable from any
  device.
- `prefer_local` / `prefer_remote`: for scripted setups.

Resolving writes a merge version with both tips as parents, which closes the conflict on every
device.

**First sync on a device that already has saves.** Local units with no base version are compared
with the remote by hash. Identical units are adopted silently. Differing ones become conflicts,
grouped into a single "first sync" prompt: *use cloud / use this device / decide per game*.

### 6.3 When sync happens

What matters most for correctness is never changing a container while the emulator has it
open, and never snapshotting a save halfway through being written.

| Hook (host calls) | Library does |
|---|---|
| `es_session_begin(title, mounted containers, timeout)` | Places a lease. Pulls the title's units, blocking up to `timeout` (default 3 s, after which play goes ahead offline). Applies fast-forwards and pending inbox items to the mounted containers. Reports another device's lease ("still playing on *Desktop*?"). **Must be called before the emulator opens the cards.** |
| `es_container_touched(c)` | Marks the container dirty and arms a 2 s debounce. |
| Debounce expires (library) | Takes a **quiescent snapshot**: extracts only units whose quick hash changed, checks that they parse cleanly, and queues them in the outbox. PS2 saves are written in several steps, so a snapshot that fails validation is retried at the next quiet point. |
| `es_container_flushed(c)` | Snapshots right away. This is the reliable point, called after the emulator fsyncs or closes the card. |
| `es_session_end(title, played_ms)` | Final snapshot, push, lease release. Then applies queued inbox items to containers that are now closed. |
| `es_app_suspend(budget_ms)` | Mobile `onPause`/`onStop`: flushes everything to the outbox (on disk), then tries to push within the budget. On Android the AAR also schedules a `WorkManager` job (network constraint, expedited) so the upload finishes after the process dies. |
| `es_sync_now()` | Full background cycle for containers that are not in use. |

**Sidecar mode (`emusyncd`)** is for emulators that can't be modified. It watches registered
containers for changes and detects that the emulator is running through a per-emulator profile
(process name, plus the presence of a lock file where one exists). Remote changes are applied
**only while the emulator is not running**. Uploads use the same debounce, plus a "process
exited" trigger. This is less precise than embedded hooks but safe, and it covers unmodified
PCSX2 on Windows and macOS on day one.

### 6.4 Leases ("playing now")

Leases are advisory. They are log records with a 10-minute expiry, renewed every 4 minutes while
a session is open. Their only job is UX: warning *before* a conflict happens. A device that is
offline can't see leases and simply plays on. Reconciliation still keeps the data safe.

**Handoff (v2):** when device B starts a title leased by A and A is online (reference server
only), A gets `ES_EV_HANDOFF_REQUESTED`. A cooperating emulator can then flush the cards,
optionally publish a `resume` state, and end its session, so B's `session_begin` finds fresh
data.

---

## 7. Transfer, storage, and security

- **Chunking:** blobs under 256 KB (most saves) are stored whole. Larger blobs, such as save
  states (PCSX2 `.p2s` files run 10–40 MB), are split with FastCDC (16/64/256 KB
  min/avg/max) so a new state of the same game re-uploads only the chunks that changed.
- **Compression:** zstd level 3 for each chunk. States are often already compressed, so a chunk
  stays uncompressed if compression saves less than 5 %.
- **Integrity:** every chunk is verified against its hash on download, and a mismatch is
  treated as a missing blob.
- **End-to-end encryption (optional, on by default for third-party storage):**
  - A random 256-bit repository key, wrapped with a key derived by Argon2id from the user's
    passphrase, is stored in `repo.json`. It is also kept in the OS keystore on each device
    (Windows Credential Manager / DPAPI, macOS/iOS Keychain, Android Keystore).
  - Chunks are encrypted with XChaCha20-Poly1305. Content addresses use keyed BLAKE3, so the
    storage provider can't confirm "this user has save X".
  - Log segments are encrypted as well. Only file names and sizes are visible to the provider.
- **Credentials:** the library never shows UI. OAuth backends use a host-supplied **token
  provider callback**. The library ships PKCE helpers (`es_oauth_begin` returns a URL;
  `es_oauth_complete` takes the redirect), but the host owns the browser or intent.
- **Retention** (configurable):
  - Unit versions: the last 20, plus one per day for 30 days, plus one per month for a year.
    Branch tips and tombstones are kept for at least 30 days.
  - States: the last 3 for each `(title, slot, compat_key)`.
  - Local snapshot store: 100 MB, kept as a ring with LRU eviction, but a unit's most recently
    overwritten version is never evicted.
- **Metered networks:** each host sets whether states sync on cellular (default: off) and
  whether saves sync on cellular (default: on, since they are small).

---

## 8. Save states

States are a separate class from saves, and opt-in for each client:

- **They are tied to a compatibility key.** The host supplies a `compat_key` string, for
  example `pcsx2:sv=<g_SaveVersion>:arch-indep`. The library lists and fetches only states
  whose key matches the local one exactly. Whether ARMSX2 and desktop PCSX2 states are
  compatible depends on the fork tracking upstream's state version. The emulator knows that and
  the library does not guess.
- **They are linked to saves.** When publishing a state, the library records the current version
  of every unit for that title (`links`). If the local card later holds a unit that is *newer*
  than the state's link, loading the state on another device could make the game overwrite newer
  progress with stale data. `es_state_query_latest` reports this as `stale_vs_saves` so the
  host can warn the user. PCSX2's "eject memory cards on state load" behaviour lowers the risk,
  and hosts should keep it on.
- **The `resume` slot** makes "continue where I left off" work: publish it on `session_end` or
  `app_suspend`, and offer it in `session_begin` when the compat key matches and it is newer
  than the local resume state.

---

## 9. Platform specifics

| Platform | Notes |
|---|---|
| **Windows** | Static lib (`.lib`) or DLL. `ReplaceFileW` for atomic replace. File-change detection via `ReadDirectoryChangesW` (sidecar). DPAPI / Credential Manager. |
| **macOS** | Universal static lib or XCFramework. FSEvents (sidecar). Keychain. Code-signing friendly, with no runtime downloads. |
| **Android** | AAR with JNI glue and a Kotlin API (coroutines `Flow` for events). **VFS callbacks** let the core read and write `content://` URIs through SAF when ARMSX2 stores cards in a user-chosen folder. `WorkManager` for deferred upload. `ConnectivityManager` for metered detection. Android Keystore. |
| **iOS** (later) | XCFramework + SwiftPM. `BGProcessingTask` for deferred sync. Background time is short, so `app_suspend` stages to the outbox and pushes at the next launch. |
| **Linux** | Same as desktop, with inotify for the sidecar. Covers the Steam Deck. |

**VFS abstraction:** if the host passes no VFS, native paths are used. A host-supplied
`es_vfs` lets it route every container access through its own I/O, for SAF on Android, a
sandbox, or an in-memory card during tests. The library's private `data_dir` always uses
native paths.

---

## 10. Integration walkthrough: the PS2 use case

### 10.1 PCSX2 fork (Windows/macOS)

The embedded integration is small. It centres on the VM lifecycle in `VMManager` and the
memory-card file layer (`FileMcd_*` in `pcsx2/SIO/Memcard/`):

```cpp
// At startup (once)
es_config cfg = ES_CONFIG_INIT;
cfg.data_dir = EmuFolders::DataRoot + "/emusync";
cfg.emulator_id = "pcsx2"; cfg.emulator_version = BuildVersion::GitRev;
es_client* sync = nullptr; es_client_create(&cfg, &sync);
es_remote_add(sync, Host::GetStringSettingValue("EmuSync", "Remote").c_str(), &token_provider);
for (each configured card) es_container_add(sync, &desc /* "ps2.card" or "ps2.card.folder" */, &id);

// VMManager::Initialize, after the serial is known and BEFORE FileMcd_EmuOpen()
es_title t = ES_TITLE_INIT; t.platform = "ps2"; t.serial = serial.c_str();
es_session_begin_args a = ES_SESSION_BEGIN_INIT;
a.title = &t; a.mounted = slot_ids; a.mounted_count = 2; a.timeout_ms = 3000;
es_session_info info; es_session_begin(sync, &a, &info);       // pulls + applies to closed cards
if (info.other_device_active[0]) Host::AddOSDMessage(...);       // "Still playing on Pixel 9?"

// FileMemoryCard::Save / FolderMemoryCard flush
es_container_touched(sync, slot_ids[port]);
// FileMcd_EmuClose / after FolderMemoryCard::Flush
es_container_flushed(sync, slot_ids[port]);

// VMManager::Shutdown
es_session_end(sync, info.id, played_ms);

// UI thread tick (or the wake callback)
es_event ev; while (es_event_poll(sync, &ev)) { ImGuiFullscreen::Handle(ev); es_event_release(sync, &ev); }
```

### 10.2 ARMSX2 fork (Android)

The ARMSX2 native core shares the PCSX2 hooks above. The Kotlin layer adds:

```kotlin
val sync = EmuSync.create(context, EmuSyncConfig(emulatorId = "armsx2", emulatorVersion = BuildConfig.VERSION_NAME))
sync.addRemote("dropbox:", tokenProvider = DropboxTokens(context))
sync.addContainer(ContainerDesc(format = "ps2.card", location = cardUri /* content:// works */))

override fun onPause()  { sync.appSuspend(budgetMs = 1500) }    // flush to outbox and schedule WorkManager
override fun onResume() { sync.appResume() }
lifecycleScope.launch { sync.events.collect { ui.handle(it) } }
```

### 10.3 End-to-end scenario

1. **Windows evening:** you play *GTA III* (SLUS-20062). Saving in-game writes
   `Mcd001.ps2`, PCSX2 calls `touched`, and after the debounce the library extracts unit
   `BASLUS-20062…`, which is new. It uploads version v1 and releases the lease when you quit.
2. **Mac next morning:** `session_begin(SLUS-20062)` pulls v1 and fast-forwards it into the Mac's
   folder card, all before PCSX2 opens the card. You play and save, which creates v2 (parent v1).
3. **Android on the train, offline:** `session_begin` times out after 3 s and play goes ahead on
   the stale v1. You also start *Ratchet & Clank* and save it (a new, independent unit). The GTA
   save becomes v3' (parent v1). Both versions wait in the outbox.
4. **Back online:** the R&C unit uploads with no conflict, because different saves on the same
   card are independent. GTA has tips v2 (Mac) and v3' (Android), both children of v1, so it is a
   conflict. ARMSX2 shows *"GTA III — Mac: today 09:12, 3h41m played · This phone: today 18:40,
   3h05m played"*. You pick one, and a merge version records the choice. The other stays in
   history and can be restored from any device.

---

## 11. Failure modes

| Failure | Handling |
|---|---|
| Process killed mid-upload | Outbox and staged blobs are on disk; the upload resumes on next start or in WorkManager. The log is written only after its blobs, so there are no dangling references. |
| Killed mid-apply | Atomic rename, or a journalled copy rolled forward on start. The original stays intact until the swap. |
| Snapshot taken during a multi-step save | Debounce, parse validation, and retry. `flushed` is the authoritative point. |
| Card full on the target device | `APPLY_BLOCKED` event. The unit stays in the inbox, and the host can offer a per-game card. |
| Clock skew between devices | Ordering comes from the DAG. HLC is used only for display and tie-breaks. |
| Corrupt or malicious remote data | Hash verification. Parsers are fuzzed and bounds-checked. Applies are verified before the swap. A bad container is never modified. |
| Different card sizes or formats on each device | Units are format-independent. Placement only checks for free space. |
| Regions (SLUS vs SLES) | Different titles with different save names. No implicit cross-region mapping (could be opt-in later). |
| Accidental delete on one device | A tombstone propagates, but every device keeps history for at least 30 days and can restore it. |
| Two devices playing at once | Lease warning ahead of time. After that, a normal conflict, and nothing is lost. |
| Remote quota exceeded | Push fails with `ES_E_QUOTA`. Saves get priority over states, and the retention policy GC runs early. |

---

## 12. Repository layout and build

```
emusync/
  crates/
    emusync-core/       model, sync engine, local DB, scheduler
    emusync-formats/    ps2mc, ps1mc, gc, sram, dir, raw adapters (no I/O deps; pure parse/build)
    emusync-remote/     backends behind a `Remote` trait
    emusync-ffi/        C ABI, event queue, struct-size versioning
    emusyncd/           sidecar daemon + `emusync` CLI (inspect cards, export .psu, restore versions)
    emusync-server/     reference server (axum + Postgres/SQLite + S3 blobs)
  include/emusync.h     the C ABI contract (checked in; CI fails if cbindgen output drifts)
  bindings/cpp/         emusync.hpp: RAII wrappers, std::string_view, expected<>
  bindings/android/     Gradle module → AAR (JNI + Kotlin)
  bindings/apple/       SwiftPM package → XCFramework
  fixtures/             golden card images, emulator quirk samples
  fuzz/                 cargo-fuzz targets for every adapter
```

**ABI stability:** every input struct starts with `struct_size`, so fields can be added to the
end. Handles are opaque. Strings are UTF-8. Out-strings are either fixed buffers or owned by an
event and released explicitly. `es_abi_version()` lets a host refuse a mismatched library.

---

## 13. Testing strategy

- **Round-trip properties** (proptest) for every adapter: `extract(apply(card, ops))` equals the
  expected units, and untouched units stay byte-identical.
- **Cross-implementation checks:** cards built by emusync must be read correctly by PCSX2
  (headless boot of a homebrew save-lister ELF), `mymc`, and the reverse direction.
- **Fuzzing** of every parser, with corpora seeded from real cards.
- **Simulated sync:** a deterministic multi-device simulator with an in-memory remote and
  injected partitions, crashes, and clock skew. The invariant to check: every version ever
  created stays reachable, and all devices converge once they are connected.
- **Backend conformance suite** run against every `Remote`, including eventually-consistent
  listing (S3, Drive) and slow propagation (Syncthing folders).
- **Device lab:** the Windows → macOS → Android scenario from §10.3, scripted, run on every
  release.

---

## 14. Milestones

| Milestone | Scope |
|---|---|
| **M0: Core** | Data model, DAG reconciliation, SQLite state, outbox/inbox, `folder` remote, `raw` adapter, CLI. Syncs any file between machines through a Syncthing or Dropbox folder. |
| **M1: PS2** | `ps2.card` + `ps2.card.folder` adapters with full round-trip and fuzz coverage. `emusyncd` with a PCSX2 profile for Windows/macOS/Linux. |
| **M2: Embedded + mobile** | C ABI freeze (v1), C++ header, Android AAR with SAF VFS. ARMSX2 fork and PCSX2 fork integrations. WebDAV, S3, Dropbox, Google Drive backends. E2E encryption. |
| **M3: States + server** | Save states with compat keys, links, and the `resume` slot. Reference server with push and leases. Handoff. |
| **M4: More platforms** | PS1 (DuckStation, SwanStation/Beetle in RetroArch), GameCube (Dolphin), cartridge SRAM normalisation, PSP. RetroArch frontend integration. |

## 15. Open questions

1. **Upstream vs. fork:** would PCSX2 upstream accept optional hooks behind a build flag? If
   not, the desktop path relies on the sidecar plus a small fork.
2. **ARMSX2 storage:** confirm where it keeps cards by default and whether it uses SAF tree URIs
   or app-specific storage. That decides whether the VFS callback path is needed in M2.
3. **State compatibility:** does ARMSX2 track upstream `g_SaveVersion` closely enough that
   desktop and Android states are interchangeable in practice?
4. **Per-game card naming:** PCSX2 and ARMSX2 support per-game cards with different naming
   conventions. Placement handles either, but the default should match what each emulator
   expects.
5. **Hosted service:** is a first-party server in scope (with operating cost and privacy
   obligations), or BYO storage only?

# emusync — Design

A native library that keeps emulator save data (memory cards, SRAM, save folders, and
optionally save states) in sync across devices and operating systems through the cloud.

**Driving use case:** play a PS2 game in PCSX2 on Windows, continue in PCSX2 on macOS, then
continue in ARMSX2 on Android. Memory card progress follows you with no manual copying. If you
play on two devices while offline, nothing is lost.

Status: design draft, v0.2 (one-file-per-card layout made the default; S3 is the first backend). The C API contract is sketched in [`include/emusync.h`](../include/emusync.h).

---

## 1. Goals and non-goals

### Goals

1. **Never lose a save.** Every version that is overwritten, locally or remotely, can be
   recovered. This rule outranks every other goal.
2. **Sync per save, not per file.** A PS2 memory card holds saves for many games. Two devices
   playing *different* games offline must merge cleanly, with no conflict.
3. **Cards just work with default settings.** The default layout in PCSX2, ARMSX2 and most
   other emulators is one image file per memory card (`Mcd001.ps2`, `Mcd002.ps2`). That layout
   must sync with zero configuration beyond choosing the cloud storage. Other layouts (folder
   cards, per-game cards) must interoperate with it.
4. **Embeddable.** A small C ABI that emulators (C++, Kotlin/JNI, Swift) can link without
   pulling in an async runtime, a GUI, or GPL-incompatible terms.
5. **Bring your own storage, starting with S3.** The repository runs on plain object storage
   and needs no server of its own. **Amazon S3 and S3-compatible services are the first
   supported backend** (§6.2). The layout also allows WebDAV, Dropbox, Google Drive, OneDrive,
   iCloud Drive, a Syncthing folder, or an optional reference server later, so no single provider
   is ever required.
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

## 2. The core idea: cards, containers, and save units

The model has three layers:

- **Logical card**: what the user thinks of as "Memory Card 1". It has a name and is the same
  card on every device: `Mcd001.ps2` on Windows, macOS and Android all hold logical card
  `ps2:Mcd001`.
- **Container**: the local file or folder that holds a logical card on one device. That can be
  a card file, a folder card, or a per-game card.
- **Save unit**: one game's save inside a card, such as a PS2 save directory. Versions and
  conflicts are tracked per unit.

### 2.1 One file per memory card (the default layout)

The most common setup, and the default in PCSX2 (and ARMSX2, which inherits its settings), is
**one image file per memory card**, shared by every game: `memcards/Mcd001.ps2` in slot 1 and
`Mcd002.ps2` in slot 2. DuckStation's shared cards, Dolphin's Card A/B `.raw` images, and
ePSXe's `.mcr` files follow the same pattern. This is the primary layout, and every default in
the library is tuned for it:

- **Cards match by name, with no setup needed.** When a host registers a card file, it is bound
  to the logical card named after the file stem (`Mcd001`), unless the host picks another name.
  A user who keeps the emulator's defaults on every device gets matching cards automatically.
  Where file names differ between emulators (DuckStation's `shared_card_1.mcd` versus another
  PS1 emulator's `epsxe000.mcr`, say), the host or user sets the logical name explicitly.
- **Saves stay on their card.** A save on `Mcd002` on Windows lands on `Mcd002` on Android,
  never on `Mcd001`.
- **Sync inside a card is still per save.** Play *GTA III* on Windows and *Ratchet & Clank* on an
  offline phone, both on `Mcd001`, and the card merges with no conflict. Afterwards each device's
  `Mcd001.ps2` holds the same saves with the same contents. The files are not necessarily
  byte-identical, because free-cluster allocation can differ, but games and emulators read the
  card's file system, not its byte layout, so this makes no difference to them.
- **Every save on the card syncs.** A shared card container receives all units of its logical
  card during idle syncs, not only those of the game being played, so opening the emulator's
  memory card manager on any device shows the same card.
- **Card size is per device.** Devices may use different card sizes (8 MB standard, or PCSX2's
  16–64 MB). If a unit doesn't fit on a smaller card, it is held back and reported (§5.1); it is
  never dropped.

#### Whole-file mode

Each logical card can instead be set to **whole-file mode**. The card image itself is then the
only unit (`ps2:Mcd001:@card`), synced through the `raw` adapter:

| | Unit mode (default) | Whole-file mode |
|---|---|---|
| Result on each device | Same saves, file layout may differ | Byte-identical card files |
| Offline play on two devices, different games | Merges cleanly | **Conflict**: the user picks one card, the other stays in history |
| Upload per save | The changed save (tens of KB to a few MB) | Changed chunks of the image (typically 100–300 KB, thanks to content-defined chunking, §7) |
| Requires a format adapter | Yes | No: works for any card format |

Whole-file mode exists for formats without an adapter (where it is the only mode), for users
who want exact mirrors, and for the first milestone, which ships before the PS2 adapter (§14).
The mode is a property of the **logical card**, recorded in the repository (§6.1), so every
device syncs a card the same way. Changing it is an explicit operation (`es_card_set_mode`). A
card image that fails validation is never synced in either mode, so corruption doesn't spread;
the host gets an error and the image is left alone.

### 2.2 Other layouts

- **Folder cards** (a PCSX2 option): a folder card bound to `Mcd001` holds the same logical card
  as a `Mcd001.ps2` file on another device. Both use the same canonical units, so the two
  interoperate with no conversion.
- **Per-game cards** (the DuckStation default, optional in PCSX2): a per-game container has a
  **title scope**. It holds only the part of a logical card that belongs to one title. The host
  binds it to the shared logical card of its slot (`Mcd001` for slot 1), so a device with
  per-game cards and a device with one shared card still exchange saves: GTA's units from the
  shared `Mcd001` go into the GTA per-game card, and back. A host can bind per-game cards to a
  separate logical card if the user wants them kept apart.

### 2.3 Save units by platform

| Platform | Container formats (local) | Save unit | Title identity |
|---|---|---|---|
| PS2 | `.ps2` raw MCFS image (8–64 MB, with ECC); PCSX2 folder card | Root directory on the card (`BASLUS-20312…`) | Disc serial (`SLUS-20312`) |
| PS1 | `.mcd`/`.mcr`/`.srm` raw 128 KB; `.gme`/`.mem` headered (§29) | Save: a chain of 1–15 blocks | Serial in the save's name |
| GameCube | `.raw` card image; Dolphin GCI folder | GCI file | Game code + maker code |
| Cartridge systems | `.srm`, `.sav`, `.eep`, `.fla` (RetroArch, mGBA, …) | The whole file, normalised | ROM hash (No-Intro SHA-1/CRC) |
| PSP | `PSP/SAVEDATA` folder (§27) | Save folder (`ULUS10041S0`) | Product code (`ULUS10041`) |
| Anything else | Any file or directory | The whole file or directory (opaque) | Set by the host |

```
 Windows (PCSX2)           S3 repository: card ps2:Mcd001       Android (ARMSX2)
 ┌────────────────┐       ┌──────────────────────────────┐     ┌────────────────┐
 │ Mcd001.ps2     │extract│ BASLUS-20062GTA3  v1→v2→v3   │apply│ Mcd001.ps2     │
 │  ├ BASLUS-20062│──────▶│ BASCUS-97199RAC   v1→v2      │────▶│  ├ BASLUS-20062│
 │  └ BASCUS-97199│◀──────│ BESLES-50330…     v1         │◀────│  ├ BASCUS-97199│
 ├────────────────┤       ├──────────────────────────────┤     │  └ BESLES-50330│
 │ Mcd002.ps2     │──────▶│ card ps2:Mcd002 (separate)   │────▶│ Mcd002.ps2     │
 └────────────────┘       └──────────────────────────────┘     └────────────────┘
```

### 2.4 Unit identity

`unit_id = platform ":" card ":" native_name`, for example `ps2:Mcd001:BASLUS-20062GTA3`. A card
in whole-file mode has the single unit `ps2:Mcd001:@card`.

- `native_name` is the name inside the card: the PS2 directory name, the PS1 block filename, or
  the GCI game code, maker code and file name (`GM8E01-MetroidPrime A`, §25.2).
- Because the card is part of the identity, the same save directory on two different cards
  (a user who keeps a backup copy on `Mcd002`, for example) is two independent units, and the
  copies never overwrite each other.
- Each unit is tagged with the **titles** it belongs to, parsed from the name (for PS2, the
  `BA`/`BE`/`BI` region prefix plus the serial). The tag decides what to pull first when a game
  starts, and which per-game container a unit belongs in. Saves the parser can't recognise still
  sync with their card.

### 2.5 Canonical form and hashing

Each format adapter turns a unit into a **canonical form**:

- PS2: a sorted list of `(path, mode bits, size, bytes)` from the save directory. Directory and
  file timestamps are kept as **metadata** and left out of the content hash, so a game that
  rewrites identical bytes does not create a new version.
- PS1: the save's name and size field and its data blocks in chain order (§29.2). Where the
  blocks are and the links between them are rebuilt on apply and left out of the hash.
- Raw SRAM: the bytes with emulator-specific padding and trailers removed (for example mGBA's
  RTC footer or RetroArch's rounding to a power of two). The trailers are kept as metadata so
  they can be restored on the same emulator.

`content_hash = BLAKE3(canonical_bytes)`. When end-to-end encryption is on, the names the
storage provider sees are keyed hashes of it (see §7, §22).

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
 ├─ Card          card_id ("ps2:Mcd001"), platform, mode (units | whole-file), HLC
 ├─ Unit          unit_id, card_id, titles[], format_hint
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
| `containers` | Registered containers: format, location, logical card, title scope, flags, last-seen fingerprint (size + mtime + quick hash) |
| `cards` | Logical cards known to the repository and their sync mode |
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
   `ReplaceFileW` on Windows). Where atomic replace isn't available (SAF), the file is replaced
   with a journal of renames, as directories are, rolled forward after a crash (§28.2).

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
  gets `ES_EV_APPLY_BLOCKED`, so it can tell the user which save didn't fit and suggest freeing
  space or using a bigger card. The unit is applied automatically once there is room.
- Prior art to check against: PCSX2's `FolderMemoryCard` (which already converts between folder
  and image at runtime), `mymc`, and `ps2mc-browser`. Golden images from all of them, plus
  console-formatted dumps, go in the test fixtures.

### 5.2 `ps2.card.folder`: PCSX2 folder memory card

- One host directory per save. `_pcsx2_superblock` and `_pcsx2_index` hold the metadata
  (ordering, attributes, timestamps).
- PCSX2 keeps the folder card in memory while a VM runs and writes it back on flush. Anything
  modified externally while it is open is lost. The adapter therefore **never applies to a
  folder card that is open** (§6.4).
- Because it shares the PS2 canonical form with `ps2.card`, a folder card on macOS and a file
  card on Android sync with no conversion step.
- Implementation notes, including the layout as read in PCSX2's source and the atomic
  directory replace: §19.

### 5.3 Other v1 and v2 adapters

- `ps1.card`: raw 128 KB images and the DexDrive and Connectix headered variants. Units are
  saves (block chains). Apply rebuilds the directory frames and link fields (§29).
- `gc.card` / `gc.gci`: GameCube raw card images and Dolphin GCI folders, with the same units
  (§25).
- `sram`: normalises cartridge save files between emulators (v2, with a quirk table for each
  emulator).
- `psp.savedata`: a PSP memory stick's `SAVEDATA` folder, one unit per save folder (§27).
- `dir`: any directory tree as one unit (Citra/Azahar and others).
- `raw`: any single file as one opaque unit. This always works, and it implements whole-file
  mode for cards (§2.1). For whole-file cards, apply still goes through the steps above, and
  step 1 runs the card's format adapter when one exists, so a corrupt image is never published or
  written.

### 5.4 Placement: where incoming units go

Every unit belongs to a logical card, so placement only has to pick which local container of
that card receives it:

1. If the unit already exists locally, update it in place.
2. If the device has a **title-scoped** (per-game) container for this card and the unit's
   title, use it.
3. Otherwise use the device's **shared** container for the card (for example `Mcd001.ps2`).
4. If the device has no container for the card at all (say, Android has only `Mcd001` but the
   save is on `Mcd002`), keep the unit in the inbox and report `ES_EV_APPLY_BLOCKED` with
   `ES_E_NOT_FOUND`. It is placed as soon as the host registers a container for that card. The
   library never creates card files on its own, because the emulator owns their format and
   settings.

---

## 6. Sync protocol

### 6.1 Repository layout that works on plain storage

The repository is designed so that **no file is ever written by more than one device**. That
removes any need for locks or compare-and-swap. It runs on S3, the first supported backend
(§6.2), and the same layout works unchanged on WebDAV, Dropbox, Drive, OneDrive, iCloud, or a
Syncthing-shared folder when those backends are added.

```
emusync/v1/
  repo.json                       format version, repo id, KDF params, wrapped key (if E2E)
  blobs/<hh>/<hash>               immutable zstd(+encrypted) chunks, content-addressed
  devices/<device-id>/
    device.json                   name, platform, emulator, created; rewritten after a rename
    log/<seq:016x>.ndjson         append-only log segments; each segment is immutable once written
    compact/<seq:016x>.json       optional snapshot of everything this device has published up to seq
```

Log records (one JSON object per line):

```json
{"op":"card","card":"ps2:Mcd001","mode":"units","hlc":"2026-10-12T19:40:00.000Z/0000"}
{"op":"device","name":"AYN Thor","hlc":"2026-10-23T05:00:00.000Z/0000"}
{"op":"version","unit":"ps2:Mcd001:BASLUS-20062GTA3","id":"b3:…","parents":["b3:…"],
 "content":"b3:…","chunks":["b3:…"],"size":91136,"meta":{"title":"GTA III","mtime":"…"},
 "hlc":"2026-10-18T11:02:07.120Z/0003","playtime_ms":5520000,"titles":["ps2:SLUS-20062"]}
{"op":"state","id":"…","title":"ps2:SLUS-20062","slot":"resume","compat":"pcsx2:sv=…","chunks":[…],"links":{…}}
{"op":"lease","title":"ps2:SLUS-20062","expires":"…"}
{"op":"lease_release","title":"ps2:SLUS-20062"}
```

A `card` record is written the first time any device publishes a logical card, and again when
`es_card_set_mode` changes its mode. The record with the highest HLC wins. When a card switches
mode, every device takes a final snapshot in the old mode before syncing in the new one, so the
switch loses no history.

A **sync cycle** works like this:

1. **Push.** Upload any blobs in the outbox the remote doesn't have yet (probe with HEAD or a
   bulk `has` call), then write *one new log segment* with this device's new records. Blobs come
   first and the log second, so a log never refers to a missing blob.
2. **Pull.** For every other device, list `log/` segments past the stored cursor, fetch them,
   and merge the records into the local `versions` table. A new device starts from each
   device's latest `compact/` snapshot instead of replaying every log.
3. **Reconcile** each affected unit (§6.3).
4. **Apply** results to containers that are not in use. Results for containers in use go to the
   inbox.

The cost is one LIST per device per cycle. Each device writes a compaction snapshot every N
segments, and segments older than everyone's snapshots can be garbage-collected.

**Reference server (later, optional):** the same records through an HTTP API, plus server push
(WebSocket or long-poll on desktop, FCM/APNs on mobile), atomic blob dedup, quotas, and a web
UI for history and restore. On S3 and other plain storage the client polls instead: on title
start, on title end, when the app returns to the foreground, and on a back-off timer (30 s to
15 min) while the emulator is open.

### 6.2 S3 backend (first supported remote)

**Scope.** Amazon S3, plus S3-compatible services (Cloudflare R2, Backblaze B2, Wasabi, MinIO,
and others) through a custom endpoint, with both virtual-hosted and path-style addressing.

```
s3://<bucket>/<prefix>?region=eu-west-1
s3://<bucket>/<prefix>?endpoint=https://<account>.r2.cloudflarestorage.com&region=auto
s3://<bucket>/<prefix>?endpoint=http://nas.local:9000&path_style=1
```

**How the layout maps onto S3.** Each file in §6.1 is one object under the prefix. Only five
operations are used:

| Operation | Used for |
|---|---|
| `PutObject` with `If-None-Match: *` | Every write: blobs, log segments, compaction snapshots, the first `device.json`. Writes are create-only, except that a renamed device overwrites its own `device.json` (plain `PutObject`). |
| `GetObject` | Fetching blobs, segments and snapshots |
| `ListObjectsV2` with `prefix` + `start-after` | Finding new log segments past each device's cursor. Zero-padded hex sequence numbers make lexical order equal log order. |
| `HeadObject` | Rare existence checks during repair |
| `DeleteObjects` | Garbage collection only (§7), and only from a device allowed to delete |

Chunks are at most 256 KB and log segments are small, so multipart upload is never needed.

**Create-only writes.** S3 conditional writes (`If-None-Match: *`) enforce the "never overwrite"
rule on the server:

- On a blob, `412 Precondition Failed` means the blob already exists. Since blobs are named by
  their contents, that counts as success. The local DB records every content hash seen in any
  log, so most blobs that already exist are skipped without a request.
- On a log segment, `412` means another installation is using this device's ID. That happens
  when an app backup is restored onto a second phone. The device then mints a new ID and writes
  its segment under that, so the two installs never interleave logs.
- Conditional-write support differs between S3-compatible providers. Setup probes it once with
  a test write. If it is missing, the backend falls back to HEAD-before-PUT, which is only racy
  in the cloned-device case above.

**Consistency.** AWS S3 has strong read-after-write and list-after-write consistency, so a pull
straight after another device's push sees it. Some S3-compatible services list with a delay.
The protocol tolerates that, because every write is an append (a renamed device's
`device.json` aside, which is only a label): an object that isn't visible
yet is fetched on the next cycle, and a missing object is never read as a deletion.

**Credentials.**

- **v1: per-user access keys.** The user creates an IAM user (or an R2/B2 API token) limited
  to the repository prefix. Least-privilege policy:

  ```json
  {
    "Version": "2012-10-17",
    "Statement": [
      { "Sid": "ListRepo", "Effect": "Allow", "Action": "s3:ListBucket",
        "Resource": "arn:aws:s3:::my-saves",
        "Condition": { "StringLike": { "s3:prefix": ["emusync/v1/*"] } } },
      { "Sid": "ReadWriteRepo", "Effect": "Allow",
        "Action": ["s3:GetObject", "s3:PutObject"],
        "Resource": "arn:aws:s3:::my-saves/emusync/v1/*" }
    ]
  }
  ```

  `s3:DeleteObject` is needed only on the device that runs garbage collection, typically one
  desktop. Every other device, phones included, can hold keys that can't delete anything. That
  limits the damage a leaked phone key can do, and with bucket versioning (below) even
  overwrites are recoverable.
- The host supplies keys through the **credential provider callback** (`es_credentials`,
  `ES_CRED_S3`). Platform bindings keep them in the OS keystore, and the library never writes
  them to `data_dir`.
- The same callback can return **temporary STS credentials** (with session token and expiry),
  for example from a Cognito identity pool or a small token-vending service. The library asks
  for fresh credentials before the current ones expire. This is the path to a hosted offering
  without changing the protocol.

**Recommended bucket settings.** The `emusync s3 check` CLI command verifies these and prints
what's missing:

- Block Public Access on, with default encryption (SSE-S3). This is independent of emusync's
  own end-to-end encryption, which is still recommended (§7).
- **Versioning on**, with a lifecycle rule that expires noncurrent versions after 30 days. This
  safety net catches client bugs, including bad garbage collection.
- A lifecycle rule that aborts incomplete multipart uploads. emusync doesn't use multipart, but
  other tools sharing the bucket might.

**Implementation.** SigV4 signing via the `aws-sigv4` crate on top of the library's own
`rustls` HTTP client, rather than the full AWS SDK, which keeps the binary small on mobile. The
S3 backend implements the same `Remote` trait as future backends.

**Cost.** Requests dominate. Three devices, each polling about 1,500 times a month while
emulators are open and listing the other two devices' logs each time, come to roughly 9,000
LIST requests a month, about $0.05 at S3 Standard prices ($0.005 per 1,000). Uploading saves
adds a few hundred PUTs. Storing saves with history is well under 1 GB. Save states, if enabled,
are the largest item: about 2 GB for 20 games × 3 slots × 30 MB, roughly $0.05 a month. Downloads
stay inside AWS's free monthly transfer allowance at this scale. Push notifications aren't
available from S3 to clients, so latency is bounded by the poll triggers above (game start
always polls first).

### 6.3 Reconciliation for each unit

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

### 6.4 When sync happens

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

### 6.5 Leases ("playing now")

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
  - Implemented as described in §22, with one refinement: content hashes stay plain BLAKE3
    inside the (encrypted) logs and the local state, and only the blob *names* the provider
    sees are keyed.
- **Credentials:** the library never shows UI. Every backend gets credentials from a
  host-supplied **credential provider callback**. For S3, the callback returns access keys or
  temporary STS credentials (§6.2). For OAuth backends added later (Dropbox, Drive, OneDrive),
  it returns a bearer token, and the library will ship PKCE helpers (`es_oauth_begin` returns a
  URL; `es_oauth_complete` takes the redirect), with the host owning the browser or intent.
- **Retention** (configurable):
  - Unit versions: the last 20, plus one per day for 30 days, plus one per month for a year.
    Branch tips and tombstones are kept for at least 30 days.
  - States: the last 3 for each `(title, slot, compat_key)`.
  - Local snapshot store: 100 MB, kept as a ring with LRU eviction, but a unit's most recently
    overwritten version is never evicted.
  - Remote garbage collection runs only on a device whose credentials allow deletes (on S3,
    `s3:DeleteObject`, §6.2). It deletes only blobs and segments that no retained version or
    compaction snapshot refers to.
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
| **Android** | AAR with the library for the host's native code (Prefab) and a Kotlin API for settings, Keystore secrets and lifecycle (§26). **VFS callbacks** let the core read and write `content://` URIs through SAF when ARMSX2 stores cards in a user-chosen folder. `WorkManager` for deferred upload. `ConnectivityManager` for metered detection. Android Keystore. |
| **iOS** (later) | XCFramework + SwiftPM. `BGProcessingTask` for deferred sync. Background time is short, so `app_suspend` stages to the outbox and pushes at the next launch. |
| **Linux** | Same as desktop, with inotify for the sidecar. Covers the Steam Deck. |

**VFS abstraction:** if the host passes no VFS, native paths are used. A host-supplied
`es_vfs` lets it route every container access through its own I/O, for SAF on Android, a
sandbox, or an in-memory card during tests. The library's private `data_dir` always uses
native paths. Implemented in §28: the host implements the callbacks in C.

---

## 10. Integration walkthrough: the PS2 use case

### 10.1 PCSX2 fork (Windows/macOS)

The embedded integration is small. It centres on the VM lifecycle in `VMManager` and the
memory-card file layer (`FileMcd_*` in `pcsx2/SIO/Memcard/`):

```cpp
// At startup (once)
es_config cfg = ES_CONFIG_INIT;
const std::string data_dir = Path::Combine(EmuFolders::DataRoot, "emusync");
cfg.data_dir = data_dir.c_str();
cfg.emulator_id = "pcsx2"; cfg.emulator_version = BuildVersion::GitRev;
es_client* sync = nullptr; es_client_create(&cfg, &sync);
// e.g. "s3://my-saves/emusync/v1?region=eu-west-1"; keys come from the OS keystore via the callback
es_remote_add(sync, Host::GetStringSettingValue("EmuSync", "Remote").c_str(), &keystore_credentials);

// Default PCSX2 layout: one file per card. The logical card defaults to the file stem ("Mcd001").
for (u32 slot = 0; slot < 2; slot++) {
  es_container_desc d = ES_CONTAINER_DESC_INIT;
  d.format   = "ps2.card";                        // or "ps2.card.folder" for folder cards
  const std::string path = MemoryCardPathForSlot(slot);   // PCSX2's configured Mcd00N.ps2
  d.location = path.c_str();                               // copied by es_container_add
  es_container_add(sync, &d, &slot_ids[slot]);
}

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

The ARMSX2 native core shares the PCSX2 hooks above. The sketch below is the original plan for
its Kotlin layer, with Kotlin owning the client; the integrations built since keep the client in
the native core and the Kotlin side in the AAR's `SyncSettings` and `SyncController` (§26.1):

```kotlin
val sync = EmuSync.create(context, EmuSyncConfig(emulatorId = "armsx2", emulatorVersion = BuildConfig.VERSION_NAME))
sync.addRemote("s3://my-saves/emusync/v1?region=eu-west-1", credentials = KeystoreS3Credentials(context))
// Same default layout as desktop: Mcd001.ps2 / Mcd002.ps2 bind to logical cards Mcd001 / Mcd002
sync.addContainer(ContainerDesc(format = "ps2.card", location = mcd001Uri /* content:// works */))
sync.addContainer(ContainerDesc(format = "ps2.card", location = mcd002Uri))

override fun onPause()  { sync.appSuspend(budgetMs = 1500) }    // flush to outbox and schedule WorkManager
override fun onResume() { sync.appResume() }
lifecycleScope.launch { sync.events.collect { ui.handle(it) } }
```

### 10.3 End-to-end scenario

All three devices use the emulator defaults: `Mcd001.ps2` in slot 1 and `Mcd002.ps2` in slot 2,
synced to one S3 bucket.

1. **Windows evening:** you play *GTA III* (SLUS-20062). Saving in-game writes
   `Mcd001.ps2`, PCSX2 calls `touched`, and after the debounce the library extracts unit
   `ps2:Mcd001:BASLUS-20062…`, which is new. It uploads version v1 and releases the lease when
   you quit.
2. **Mac next morning:** `session_begin(SLUS-20062)` pulls v1 and fast-forwards it into the Mac's
   `Mcd001.ps2`, all before PCSX2 opens the card. (It would work the same if the Mac used a
   folder card.) You play and save, which creates v2 (parent v1).
3. **Android on the train, offline:** `session_begin` times out after 3 s and play goes ahead on
   the stale v1. You also start *Ratchet & Clank* and save it (a new, independent unit). The GTA
   save becomes v3' (parent v1). Both versions wait in the outbox.
4. **Back online:** the R&C unit uploads with no conflict, because different saves on the same
   card are independent. GTA has tips v2 (Mac) and v3' (Android), both children of v1, so it is a
   conflict. ARMSX2 shows *"GTA III — Mac: today 09:12, 3h41m played · This phone: today 18:40,
   3h05m played"*. You pick one, and a merge version records the choice. The other stays in
   history and can be restored from any device.
5. **Next idle sync on Windows and Mac:** R&C's save is written into their `Mcd001.ps2` too, so
   all three cards now hold the same saves. Had `Mcd001` been in whole-file mode, step 4 would
   instead have been a conflict over the entire card, even with no GTA change on the phone.

---

## 11. Failure modes

| Failure | Handling |
|---|---|
| Process killed mid-upload | Outbox and staged blobs are on disk; the upload resumes on next start or in WorkManager. The log is written only after its blobs, so there are no dangling references. |
| Killed mid-apply | Atomic rename, or a journalled copy rolled forward on start. The original stays intact until the swap. |
| Snapshot taken during a multi-step save | Debounce, parse validation, and retry. `flushed` is the authoritative point. |
| Card full on the target device | `APPLY_BLOCKED` event (`ES_E_NO_SPACE`). The unit stays in the inbox and applies once there is room. |
| Logical card missing on a device (has `Mcd001`, not `Mcd002`) | `APPLY_BLOCKED` event (`ES_E_NOT_FOUND`). Units wait in the inbox until the host registers that card. The library never creates card files itself. |
| App backup restored onto a second device (cloned device ID) | A create-only S3 write of a log segment fails with `412`. The clone mints a new device ID before writing anything. |
| S3-compatible provider with delayed listing | Objects not yet listed are picked up next cycle. Absence is never read as deletion. |
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
    emusync-remote/     `Remote` trait; S3 first (SigV4 + rustls), in-memory remote for tests
    emusync-ffi/        C ABI, event queue, struct-size versioning
    emusyncd/           sidecar daemon + `emusync` CLI (inspect cards, export .psu, restore
                        versions, `s3 check`)
    emusync-server/     reference server (later)
  include/emusync.h     the C ABI contract (checked in; CI fails if cbindgen output drifts)
  bindings/cmake/       EmuSyncAndroid.cmake: libemusync.so from an Android CMake build (§21)
  bindings/cpp/         emusync.hpp: RAII wrappers, std::string_view, expected<>
  bindings/android/     Gradle build → AAR: libemusync.so as Prefab + Kotlin API (§26)
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
- **Backend conformance suite** run against every `Remote`. For S3: MinIO in CI on every
  change, plus nightly runs against AWS S3 and at least one S3-compatible service (such as R2).
  The suite covers conditional-write behaviour, the HEAD-before-PUT fallback, and an injected
  delayed-listing mode.
- **Card-mode tests:** both unit mode and whole-file mode for the same card fixtures, a switch
  between modes in the middle of a session, and mixed layouts (file card ↔ folder card ↔
  per-game card).
- **Device lab:** the Windows → macOS → Android scenario from §10.3, scripted, run on every
  release.

---

## 14. Milestones

| Milestone | Scope |
|---|---|
| **M0: Core + S3** | Data model, DAG reconciliation, SQLite state, outbox/inbox, **S3 backend** (AWS + MinIO), `raw` adapter, logical cards with **whole-file mode**, CLI. Already useful: `Mcd001.ps2` syncs between machines as a whole file. |
| **M1: PS2** | `ps2.card` + `ps2.card.folder` adapters with full round-trip and fuzz coverage, making unit mode the default for PS2 cards. `emusyncd` with a PCSX2 profile for Windows/macOS/Linux. |
| **M2: Embedded + mobile** | C ABI freeze (v1), C++ header, Android AAR with SAF VFS and keystore-backed S3 credentials. ARMSX2 fork and PCSX2 fork integrations. E2E encryption. |

| **M3: States + more backends** | Save states with compat keys, links, and the `resume` slot. STS temporary credentials. Further backends (WebDAV, Dropbox, Google Drive) on the same layout. Reference server with push, leases and handoff if the hosted question (§15) is answered yes. |
| **M4: More platforms** | PS1 (DuckStation, SwanStation/Beetle in RetroArch), GameCube (Dolphin), cartridge SRAM normalisation, PSP. RetroArch frontend integration. |

**Status (October 2026).** M0 and M1 are done. M2: **done** are the C ABI v1 freeze
(`include/emusync.h`, §20, with a C++ compile check of the header), the Android build of
`libemusync.so` with its CMake module (§21), the ARMSX2 integration (the fork in
`ilyalissoboi/ARMSX2` embeds `libemusync.so`; two-way sync verified on a real device, an AYN
Thor, against desktop PCSX2), end-to-end encryption (§22) with passphrase entry in ARMSX2's
settings, the desktop build of the shared library (§23), host secrets in the system's
credential store (§24), the PCSX2 desktop integration (the fork in `ilyalissoboi/pcsx2`; two-way
sync verified against ARMSX2), the GameCube adapters (§25, ahead of M4), the Android AAR
(§26), and the SAF VFS (§28). **Open:** the C++ RAII wrapper `bindings/cpp/emusync.hpp`. Ahead
of M4, also done: the Dolphin integration (desktop and Android, the fork in
`ilyalissoboi/dolphin`), PSP saves (§27) and the PPSSPP integration (`ilyalissoboi/ppsspp`,
desktop and Android, verified between Windows and an AYN Thor), and the PS1 adapter (§29).

**Roadmap (October 2026).** Memory cards come before save states (M3). In this order, with the
dependencies each item needs placed before it:

1. Desktop build of the shared library, with a CMake module and a package for MSBuild (§23).
2. Credential storage for hosts on desktop: the OS store (Credential Manager, Keychain, Secret
   Service) behind the C ABI, so emulators don't each write their own (§24).
3. **PCSX2 desktop integration** (a fork with ARMSX2's hooks and a Qt settings page), on Windows,
   macOS and Linux. Removes the sidecar's Windows limitation (uploads wait for the game to close).
4. GameCube adapters: `gc.card` (raw images) and `gc.gci` (GCI folders) (§25).
5. The Android AAR (§26): Kotlin API, Keystore-backed settings, lifecycle hooks, shared by the Android
   integrations below instead of each copying ARMSX2's.
6. **Dolphin integration**, desktop and Android (one codebase; Android keeps its data in
   app-specific storage, so no SAF).
7. PSP: the `psp.savedata` adapter, one unit per save folder, with folders of save folders in the core (§27).
8. The SAF VFS (`es_vfs`): PPSSPP's Android memory stick can be a user-picked `content://` folder
   (§28; the host implements the callbacks).
9. **PPSSPP integration**, desktop and Android.
10. PS1: the `ps1.card` adapter.
11. **DuckStation on desktop** through the sidecar (a `duckstation` profile). DuckStation is
    CC BY-NC-ND 4.0, which forbids distributing modified versions, so it can't embed the library.
12. **DuckStation on Android**: undecided. The app is closed source; options are a standalone
    emusync companion app syncing its card folder, another PS1 emulator, or none.
13. **Finishing uploads after the app is closed**, as a library feature: WorkManager through the
    AAR on Android (ARMSX2 moves to the AAR), and a hand-off to a helper process on desktop.
14. Everything else (leases, retention and garbage collection, per-game cards, other backends (Dropbox first, §30),
    metered networks, guided S3 setup, conflict and history UI on Android, …), to be
    prioritised later.

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
5. **Hosted service:** with S3 first, a hosted offering could be a managed bucket plus STS
   credential vending rather than a custom server. Is that in scope, or is it bring-your-own
   bucket only?
6. **S3 onboarding:** creating an IAM user and pasting keys is a hurdle for non-technical users.
   Should M0 ship a guided setup (CloudFormation template or `emusync s3 init`) that creates the
   bucket, policy and keys?
7. **S3-compatible providers:** which ones are tested and supported at launch (R2, B2, Wasabi,
   MinIO)?

## 16. Implementation notes (M0)

Where the M0 code differs from or pins down the design above:

1. **Cycle order.** A sync cycle runs pull → snapshot local changes → reconcile/apply → push
   (§6.1 lists push first). Pulling first lets a first sync adopt identical cloud content
   instead of publishing a duplicate. Local changes still reach the remote in the same cycle.
2. **Divergence is a tip set.** A local change becomes a version (parent = base) as soon as
   it is snapshotted, so every row of §6.3 reduces to "how many tips does the unit have".
   Tips with identical content are merged automatically, whatever the policy.
3. **`newest` writes a deterministic merge.** Its parents are all tips (sorted), its HLC is
   the highest tip HLC plus one, and its content is the winner's. Every device computes the
   same record and id, so concurrent automatic resolutions converge. The loser stays in
   history as a merge parent rather than as an open branch tip. A tip with history beats a
   first-sync root; between roots the older one wins. Merges may have more than two parents.
4. **First sync** also adopts an *older* version with identical content (a stale copy of the
   card), which then fast-forwards instead of raising a conflict.
5. **Version ids** are `b3:` + BLAKE3 of the record's canonical JSON (sorted keys, no
   whitespace, without `op` and `id`), verified on pull. Fields added by later versions are
   covered automatically.
6. **Blobs** carry a one-byte encoding header (0 = raw, 1 = zstd).
7. **Lost responses.** A segment's body is fixed before the first upload attempt. On `412` the
   existing object is fetched and compared, so a write whose response was lost is not
   mistaken for a cloned device id.
8. **Device names.** `device.json` is written create-only before a device's first segment.
   A rename (a different `EngineOptions::device_name` at `Engine::open`) is announced in the
   device's own log, with a `{"op":"device","name":"…","hlc":"…"}` record queued at the next
   push, even if nothing else is: the latest one in a log names that device, and its earlier
   versions too. Other devices get it with the segment, so a rename costs them no request,
   and it can't be lost to a `device.json` that isn't visible yet. Once the record is in a
   segment under the device's id, the device rewrites its `device.json` (a plain overwrite,
   keeping every field but `name`) for devices that join later and for earlier versions,
   which skip the record. The order matters for a restored backup given another name: its
   segment collides first, so it is reminted (§6.2) before it writes anything with the new
   name, and the original's `device.json` stays its own. Local meta keeps the name last
   logged (`logged_name:<id>`) and the name in `device.json` (`device_json_name:<id>`). A
   data directory from before them has only the `device_json:<id>` flag; it logs its name
   once and rewrites `device.json`. A device reads another's `device.json` when it doesn't
   know the name yet, and once for every device after upgrading to this version
   (`device_names_reread`), since it may have skipped `device` records before. In an
   encrypted repository the record is inside the sealed segment and the rewrite is sealed
   like every other object (§22).
9. **Not yet implemented:** compaction snapshots and garbage collection (new devices replay
   full logs), a last-seen time in `device.json`, sessions and leases
   (`Engine::set_busy` routes results to the inbox instead), the size+mtime fingerprint
   shortcut (cards are re-hashed each scan), and the FFI beyond version/status functions (now
   in §20).
10. **Platform details.** HTTP is `ureq` 2 with rustls/ring. On Windows the atomic replace is
    `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` (via `std::fs::rename`) rather than `ReplaceFileW`.
    Devices are discovered with one delimiter LIST of `devices/` per cycle. The conditional-write
    probe leaves a small `_probe/<uuid>` object.
11. **CLI.** S3 credentials come from `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` /
    `AWS_SESSION_TOKEN` and are never stored. `emusync sync` defaults to the `ask` policy
    (`--policy newest` for unattended use).

## 17. Implementation notes (M1: PS2 cards in unit mode)

1. **Units are root entries.** Each entry in the card's root directory is a unit, normally a
   save directory. Unit names are the raw entry names decoded as Latin-1 (lossless; plain
   ASCII for real saves). Titles come from the name (`BASLUS-20062GTA3` → `ps2:SLUS-20062`).
2. **Canonical form.** A tree of names, modes, attributes and file contents, with each
   directory's children sorted by name. Timestamps and on-card entry order go in the
   `ps2.layout` meta field, which is restored on apply but not hashed. The unused fields of
   directory entries are not kept.
3. **Rebuild, not patch.** Applying changes rewrites the whole allocatable area and the FAT
   (§5.1 described first-fit allocation). Entries are laid out contiguously in directory
   order: rebuilt cards are never fragmented, and two cards holding the same saves in the same
   order are byte-identical. Free clusters are erased to `0xFF`. The superblock, indirect FAT
   and backup blocks are copied unchanged, except that a superblock missing its derived fields
   (item 5) gets them filled in. Changed saves keep their position in the root directory; new
   ones are appended. Everything written matches what the PS2 BIOS writes. That was checked
   byte for byte against a card the BIOS formatted and saved to in PCSX2, with
   `EMUSYNC_REAL_PS2_CARD` and `crates/emusync-formats/tests/real_card.rs`; only cluster
   numbers differ. In particular:
   - a save directory's `.` entry has the directory's mode, its creation time twice, and points
     to the parent (first cluster, slot);
   - its `..` entry has the plain directory mode `0x8427`, cluster and slot 0, and the parent's
     creation time twice (only the root's `..` is hidden, `0xA426`);
   - the unused rest of a file's last cluster, an unused directory slot and the unused part of
     the indirect FAT are `0xFF`.

   The first M1 build got these wrong: it wrote `..` as `0xA426` with the directory's own
   times, `.` with the modification time, and zero padding. In PCSX2, games then found no saves
   on a card emusync had written.
4. **ECC** is recomputed for every page written (three bytes per 128-byte chunk, then four
   zero bytes) and is not checked on read: pages that were never written have none, and
   emulators don't check it either.
5. **Unformatted cards.** PCSX2 creates new card files with every byte `0xFF` and leaves
   formatting to the PS2 BIOS (the cards on the development machine were in this state). An
   image like that, of a standard size, is an empty card. The first apply formats it as the
   BIOS does: 8 MB gives 41 reserved clusters and 8,135 allocatable, with card flags `0x2B`.
   The superblock includes the fields the BIOS derives from the geometry at `0x154..0x184`:
   cluster size, FAT entries per cluster, clusters per erase block, -1, the allocatable-cluster
   limit (8,001 on 8 MB) and -1. The card is stamped with the first incoming save's creation
   time. This is what lets a fresh
   card on a new device receive every save from the cloud.
6. **Not rewritten.** Cards listing bad blocks, or whose root directory doesn't start at the
   first allocatable cluster, are read and uploaded but never written to. Apply reports them as
   unsupported.
7. **Deletions** become tombstone versions that delete the save on other devices and stay in
   history. One exception: if two or more saves disappear and the card is left empty, the card
   was most likely replaced or reformatted. The saves are restored from the cloud instead of
   being deleted everywhere, and the sync reports a warning.
8. **Apply** writes every change for a card in one rebuild. If that fails (card full, or a save
   the adapter rejects), it applies changes one at a time, deletions first. Saves that still
   don't fit wait in the inbox with a reason and apply automatically once there is room. Before
   the swap, every save on the rebuilt card is re-extracted and compared with what it should
   hold, both changed and untouched ones.
9. **Mode switching** (`es_card_set_mode`, `emusync card mode`) snapshots the card in its old
   mode, then publishes the new mode. Identical saves on different devices are adopted, not
   duplicated, so switching a card that was in sync creates no conflicts. Re-adding a card
   registered as `raw` by M0 switches its container to `ps2.card`. Switching back to
   whole-file mode usually raises one conflict per card: cards holding the same saves need not
   be byte-identical, so each device's image is a different version. Either side is fine to
   take.
10. **`card list` reads the card file**, so saves that haven't synced yet are listed too.
11. **Not yet:** per-game title-scoped containers (§2.2), the sidecar
    watcher (§6.4), in-game titles from `icon.sys` (Shift-JIS), and a cargo-fuzz target. The
    parsers are covered by randomized tests instead (corrupted cards and saves must be rejected
    without panicking; random apply sequences are checked against a model). The PS2 adapter
    has not been tested against cards written by a real PS2 BIOS or by PCSX2 with saves on
    them, because the only real cards available were unformatted.

## 18. Implementation notes (sidecar: `emusync watch`)

1. **Profiles are JSON** (`crates/emusyncd/profiles/`, built in with `include_str!`; more with
   `--profile file.json`). Each lists, per OS, process names (matched without regard to case),
   default card folders (`{home}`, `{documents}`, `{config}`, `{data}` placeholders, resolved
   with the `dirs` crate so redirected Documents folders work) and optional lock files, plus the
   card file extensions the emulator owns and, optionally, a folder card marker: a directory
   holding a file of that name is one of the emulator's folder cards, whatever its name (PCSX2:
   `_pcsx2_superblock`, so both `Mcd001` and `Mcd001.ps2` folders). PCSX2 ships built in. A profile must give the current
   OS at least one process name or lock file, or it is rejected: without one the emulator would
   never look busy. Process names are compared with both the reported name and the executable's
   file name, and a 15-byte name (Linux truncates `comm`) also matches a longer profile name it
   starts.
2. **Busy is per card.** Before every cycle, each container is marked busy (`Engine::set_busy`)
   if an emulator whose profile claims it (by extension or folder card marker) is running.
   Cards no profile claims are
   busy whenever any watched emulator runs. Busy cards are still snapshotted and uploaded where
   they can be read; only applies wait in the inbox. Process lists come from `sysinfo` every
   2 s, behind a `ProcessProbe` trait so tests use a fake.

   **Locked cards (Windows).** PCSX2 keeps a card file open without sharing while a game runs.
   For a folder card, it does this with the files of the saves the game loaded. Nobody else can
   read them meanwhile (`ERROR_SHARING_VIOLATION`; seen in PCSX2 testing with a folder card;
   that card files behave the same way is inferred from PCSX2's file API, not observed).
   - The engine reports such a card as blocked with `LOCKED` (`fsutil::is_locked`) and skips
     it for that cycle. The rest of the cycle, other cards and downloads included, goes on, as
     does an apply that finds its target locked; that change waits in the inbox.
   - The watcher's change check doesn't count a locked card as changed. It logs the lock once
     per episode (`waiting`, then `released`), not on every poll.
   - The card is synced by the first cycle after it is released: the change events when PCSX2
     closes the game, or the exit cycle.
   - So on Windows, saves upload when the game is closed rather than while it is played.
3. **Triggers.** File events (`notify`, non-recursive, on the folders holding cards) arm a 2 s
   debounce. When it expires, a cycle runs only if a card's content hash changed, so the
   watcher's own reads and applies don't loop. (Size and mtime are not enough: cards have a
   fixed size, and coarse mtimes can hide a second write.) A folder card is hashed as its tree
   image (§19.3), modification times included, and is also watched recursively, since PCSX2
   writes deep inside save directories. Events on the `.emusync-tmp`, `.emusync-old` and
   `.emusync-swap` siblings of a directory replace are ignored. A watch follows the directory,
   not the path (`ReadDirectoryChangesW`, inotify), so after a replace swaps the folder it
   would report the old one being deleted and miss PCSX2's next writes. The watch is renewed
   after every apply to a folder card, and when another program creates, removes or renames
   the folder. Emulator start (poll at once, cards now busy) and
   exit (apply the inbox) each run a cycle. A cycle due while writes are still settling waits
   for them, except on exit. Polls back off from 30 s to 15 min, doubling after a poll with no
   news or with the remote unreachable, and resetting after anything else.
4. **Discovery.** Card files in the profiles' folders, and directories in them holding the
   profile's folder card marker, are registered automatically (`--no-discover` turns this
   off). emusync's own scratch directories are never registered, though `<card>.emusync-tmp`
   holds a superblock while a replace runs. PCSX2 creates a folder card's directory before its
   superblock, and the card folder's flat watch doesn't see the superblock arrive. So a new
   folder in a card folder arms the debounce, and is checked for the marker at each process
   probe for a minute, in case the superblock comes after the debounce expires. A file that can't be registered, such as a second
   `Mcd001.ps2` in another folder, or an `Mcd001` folder card next to an `Mcd001.ps2` file,
   is skipped with one warning.
5. **Reporting.** `SyncReport::resolved` lists units the `newest` policy merged, so the log can
   say which saves diverged and where the losing version is kept.
6. **Shutdown.** Ctrl+C, SIGTERM/SIGHUP (`ctrlc` crate) or a `watch.stop` file in the data
   directory stop the loop between cycles, so a running cycle always finishes. Then
   `Daemon::shutdown` runs a final cycle unless an emulator is running (its card may be
   halfway through a write; its settled saves were already uploaded). A second Ctrl+C exits
   at once. The watcher keeps `watch.pid` in the data directory while it runs; a second
   watcher on the same directory refuses to start. The stop file exists because the
   windowless watcher on Windows has no console to signal.
7. **Starting at login (`emusync service`, `crates/emusyncd/src/service/`).** Per user and
   without admin rights: a Task Scheduler task with a logon trigger for the current user,
   `InteractiveToken`, `LeastPrivilege`, no time limit, allowed on battery, restart on failure
   (registered from UTF-16 XML with `schtasks /Create /XML`); a LaunchAgent with `RunAtLoad`
   and `KeepAlive.SuccessfulExit = false`; a systemd user unit with `Restart=on-failure`,
   `WantedBy=default.target`. Stopping waits 90 s for the final sync (`TimeoutStopSec`,
   `ExitTimeOut`). `stop`/`uninstall` use the stop file on every OS, so the watcher exits 0 and
   the service manager doesn't restart it. On Windows the task runs `emusyncw.exe`, built from
   the same library with `windows_subsystem = "windows"`, since a console program would open
   a console (or Windows Terminal) window at every login. The definitions are generated as
   text by pure functions and unit-tested on every OS.
8. **Credentials for the service** are never written into a definition. The watcher gets them
   from the environment the service manager provides (Windows tasks inherit the user's saved
   environment variables) or from `--env-file`, a `KEY=VALUE` file the user creates (a warning
   if it is readable by others on Unix). `service install` says which applies.
9. **Logging without a terminal.** When stderr isn't a terminal, the watcher logs to
   `logs/watch.log` in the data directory, rotated at 1 MiB with three old files (if a rename
   fails because the file is open elsewhere, it is truncated instead), and logs panics there.
10. **Not yet:** a tray app, a final sync at Windows logoff (the windowless process has no
    window to receive `WM_ENDSESSION`; the next start uploads what is left), and PCSX2
    portable installs (cards next to the executable; register them with `card add`).

## 19. Implementation notes (PCSX2 folder cards)

### 19.1 The on-disk layout

Read from PCSX2's `pcsx2/SIO/Memcard/MemoryCardFolder.cpp` and `MemoryCardFile.cpp` (master
at `161a6cd`, October 2026). **Confirmed** below means read in that source; **inferred** means
reasoned from it, not observed in a running PCSX2.

| Item | Status | Detail |
|---|---|---|
| Card root | confirmed | A directory. PCSX2 treats a directory as a folder card if it holds a `_pcsx2_superblock` file (`FileMcd_IsFolder`). |
| `_pcsx2_superblock` | confirmed | The card's first erase block (8,192 bytes, no ECC), read and written as is. A new folder card is a directory with an **empty** superblock file. The card counts as formatted only if byte `0x16` is `0x6F` (the `o` of "Format"). An unformatted card loads no saves, even if directories exist. |
| Saves | confirmed | One directory per root entry. Root-level files are skipped, and so is every name starting with `_pcsx2_`. With "auto-manage" filtering on, PCSX2 loads only `DATA-SYSTEM`, `BWNETCNF` and folders matching the running game's serial. That changes what the game sees, not what is on disk. |
| `_pcsx2_index` | confirmed | One per directory, YAML (rapidyaml). `$ROOT` (`%ROOT` before a rapidyaml fix) holds the directory's own `timeCreated`/`timeModified`. Each file has `order`, `timeCreated` and `timeModified`. Times are Unix seconds, converted to the card's date fields in UTC (`gmtime`, `_mkgmtime`/`timegm`). PCSX2 creates the file in flow style (`{$ROOT: {...}}`), and rewrites it with each flush. |
| Entry order | confirmed | Subdirectories don't get an `order`. They come first, in host enumeration order, then files sorted by `order`. Files missing from the index get negative positions in enumeration order, so they come first. PCSX2 drops a file whose `order` duplicates another file's (`try_emplace`). |
| Non-default entries | confirmed | `_pcsx2_meta_directory` (inside a directory) and `_pcsx2_meta/<file>` hold the raw 512-byte directory entry. They are written when the mode differs from the default (`0x8427` for directories, `0x8497` for files), the attribute is non-zero, or the name had to change on the host. When one exists, the name (if at least `0x60` bytes were read), mode, attribute and timestamps come from it. |
| Host names | confirmed | `\ % : \| " < >` become `_`, as do trailing dots and spaces. A name without metadata is copied into the 32-byte name field with `strlcpy`, so at most 31 bytes are kept. |
| Deletions | confirmed | A deleted save is renamed to `_pcsx2_deleted_<name>`, replacing any earlier one of that name. |
| Capacity | confirmed | Saves get the cluster count the BIOS reports: `alloc_end` rounded down to a thousand, minus one (7,999 clusters on 8 MB). PCSX2 adds saves in enumeration order and skips, with a warning, any save that doesn't fit. |
| While open | confirmed | PCSX2 builds the card image in memory when the card is opened. It writes changes back a couple of frames after the last write, and again on close. It keeps the files of loaded saves open, so outside changes to an open card are lost or overwritten. |
| Root order | inferred | Root save order is the host's enumeration order (`FindFiles`), so it isn't kept anywhere. emusync reads directories sorted by name. The root order recorded in `ps2.layout` is therefore not preserved on folder cards. Order inside a save is preserved. |
| YAML output | inferred | emusync writes block-style YAML, quoting keys that need it. Any YAML parser reads that, but it hasn't been loaded into a real PCSX2. |
| Non-UTF-8 names | inferred | PCSX2 hands raw name bytes to the host as a UTF-8 path. emusync writes names that aren't valid UTF-8 as Latin-1 and keeps the real bytes in a metadata file. |

### 19.2 Adapter

1. **Same units as `ps2.card`.** `ps2.card.folder` turns each save directory into the same
   `Node` tree and builds its canonical unit with the same `ps2::canonical` and `ps2::decode`.
   A folder card bound to `ps2:Mcd001` (from the directory name, `Mcd001` or `Mcd001.ps2`)
   and a `Mcd001.ps2` file on another device hold identical units, with no conversion step.
   Identical saves on both are adopted on first sync, not raised as conflicts. A folder card's
   platform is always `ps2`, whatever its extension.
2. **Reading mirrors `AddFolder`:** the same order rules, metadata files winning over the index,
   and the index winning over host modification times. Modes always get the "exists" bit,
   since the card build sets it anyway. If the index can't be parsed, it is ignored, as PCSX2
   does: order and times fall back. Some folders are invalid and never synced:
   - a metadata file whose mode lacks the directory or file bit;
   - an index giving two files the same `order` (PCSX2 would load only one of them);
   - a folder holding more than the card's capacity. PCSX2 skips a save exactly when all of
     them together don't fit, whatever the load order, so syncing such a folder would spread
     saves the emulator doesn't show. The user removes saves or picks a bigger card.
3. **Writing mirrors `FlushFileEntries`.** A save is written as its directory, an index listing
   every file's order and both timestamps, and metadata files only for non-default entries.
   A changed save replaces its whole directory. A deleted save is renamed to
   `_pcsx2_deleted_<name>`. Everything else in the folder is kept unchanged: stray files,
   earlier deleted saves, and saves the adapter doesn't touch. The first save written to an
   unformatted card writes the standard 8 MB superblock (the one `ps2.card` formats new card
   files with), stamped with that save's time.
4. **Refused:** saves PCSX2 would not load. That covers root-level files, names starting with
   `_pcsx2_`, and two entries whose host names would clash (after cleaning, compared without
   case, for Windows and macOS). These are reported as unsupported and wait in the inbox. A
   change that would push the saves past the BIOS capacity fails with `NO_SPACE`, as on a full
   card file.
5. **Whole-file mode** works too: the unit is the folder's tree image. It only syncs with other
   folder cards, because a `.ps2` image is not a valid folder card. Per-save mode is the one
   that interoperates.

### 19.3 Directory containers in the core

1. **Tree images.** Adapters stay pure. `ContainerFormat::kind()` says whether a container is
   a file or a directory. For a directory, the engine reads everything under it into a
   `formats::tree::Tree` (paths, contents and modification times) and serializes it
   (`ES-TREE1`). That image goes through the same scan, validate, apply and verify steps as a
   card file's bytes. Its hash, modification times included, is the "container changed during
   sync" check before an apply.
2. **Atomic directory replace** (`fsutil::replace_dir`), the directory form of §5 step 5:
   1. Write the new tree to `<card>.emusync-tmp`, fsyncing every file (and directory on Unix).
      File and directory modification times are restored from the image (directories after
      their children), so the result reads back as the same image. That matters in whole-file
      mode, where the image is the unit.
   2. Write the journal `<card>.emusync-swap` (temp file + rename).
   3. Rename `<card>` to `<card>.emusync-old`, then `<card>.emusync-tmp` to `<card>`.
   4. Remove `<card>.emusync-old`, then the journal.

   Without a journal, a leftover temp tree is incomplete and is deleted; the original was
   never touched. With a journal, the new tree is known to be complete, so
   `fsutil::recover_dir_replace` rolls forward from whichever rename the crash interrupted.
   It runs before every scan of a folder card that isn't busy. If a rename fails (on Windows,
   because a program holds a file in the folder open), the replace rolls back. The saves wait
   in the inbox with the error and are retried next cycle.
3. **Busy cards.** As for card files, nothing is applied to a container marked busy
   (`Engine::set_busy` / `es_session_begin`). For folder cards this matters more, because
   PCSX2 would overwrite outside changes on its next flush. Recovery is also skipped while a
   card is busy. Without a host integration (the CLI), the rename failing on open files is the
   only protection on Windows. On macOS and Linux the rename succeeds and PCSX2 can then write
   to the old directory, so hosts must report sessions.
4. **Side effects.** While a replace runs, PCSX2's memory card list can show
   `<card>.emusync-tmp` as an extra folder card, since it holds a `_pcsx2_superblock`. It
   disappears when the replace finishes.
5. **CLI.** `emusync card add <dir>` registers a folder card if the directory has a
   `_pcsx2_superblock`, and refuses any other directory. `emusync card inspect <dir>` lists its
   saves.
6. **Tests.** `crates/emusync-core/tests/folder_cards.rs` syncs a folder card against a card
   file over the in-memory remote. It covers saves in both directions, offline merges,
   deletions, adoption of identical saves, a busy card, and an interrupted replace rolled
   forward. `ps2_robustness.rs` runs random apply sequences on both adapters and checks that
   they hold the same units. It also checks that corrupted folder images and damaged index or
   metadata files are rejected without panicking.
7. **Not yet:** a real PCSX2 has not loaded a folder written by emusync, and emusync has not
   read a folder card with saves written by PCSX2. The tests build folders from the format
   as read in PCSX2's source. Folder cards larger than 8 MB are read with their own
   superblock's capacity, but an unformatted folder card is always formatted as 8 MB.

## 20. Implementation notes (C ABI v1)

`crates/emusync-ffi` implements `include/emusync.h` (ABI version 1). Where it differs from or
pins down §3.2, §6.4, §10 and §12:

1. **Threads (§3.2).** No async runtime: one worker thread per client owns the `Engine`, which
   is synchronous, and runs commands from a queue. `es_remote_add` runs the network half of its
   setup (`Engine::prepare_remote`: LIST, conditional-write probe, `repo.json`) on a short-lived
   helper thread, so a slow endpoint doesn't hold up the worker; the result is handed to the
   worker (`Engine::bind_remote`). An endpoint that is unreachable is attached anyway and set up
   at the next network cycle. Its URI is reserved meanwhile (in memory), so a second
   `es_remote_add` for another remote is refused, as for a bound data directory.
2. **What waits.** Blocking calls with their own deadline: `es_session_begin` (`timeout_ms`,
   default 3 s), `es_app_suspend` (`budget_ms`), `es_remote_add` (10 s), `es_container_flushed`
   (2 s for the local snapshot, then `ES_PENDING`) and `es_client_destroy`. Calls that need an
   answer from the worker (container add/remove, unit history and restore) wait up to 10 s for
   it to be free and return `ES_TIMEOUT`, having had no effect, if it is stuck in a network
   operation. Card modes and conflicts are read from a cache the worker refreshes after every
   command, so `es_card_get_mode`, `es_card_set_mode` (validation) and `es_conflict_get` never
   wait. `es_container_flushed` and `es_app_suspend` jump the queue.
3. **Session safety.** `es_session_begin` queues a pull-and-apply cycle (`Engine::pull_and_apply`,
   no push) and, whatever happened by the deadline, marks the mounted containers busy before it
   returns. The busy set is now a `BusyGate` the engine shares with other threads and holds
   across every container write, so once `es_session_begin` returns, a pull that is still
   running sends its results to the inbox instead of writing to a card the emulator has open
   (tested with a remote whose LIST stalls past the deadline). Busy marks are counted per
   session, so a session ending on the worker can't clear the mark of the next one on the same
   card. A container can be in only one open session (`ES_E_BUSY`), and can't be removed while
   a session is open or starting with it.
   A cycle also never *reads* a busy container (`EngineOptions::scan_busy = false`, which the
   sidecar leaves on): a pull that runs past the deadline would otherwise snapshot a card the
   emulator is writing. Busy containers are snapshotted only at the host's quiet points
   (`touched` after the debounce, `flushed`, session end); remote changes for them are still
   reconciled against the last snapshot and wait in the inbox.
4. **Session end.** Final snapshot of the mounted containers, clear busy, then one full cycle
   (pull, apply inbox items, push), rather than snapshot → push → clear → apply. One push
   instead of two; the result is the same. `played_ms` is not recorded yet.
5. **Struct versioning (§12).** Output structs carry `struct_size` too (`es_session_info`,
   `es_conflict_info`, `es_event`, `es_state_info`), set by the host's `ES_*_INIT` macro, so the
   library knows how much it may write. A struct smaller than the library's is `ES_E_ABI`; a
   larger one (a newer header) is accepted. `es_version_info` is embedded in `es_conflict_info`
   and therefore frozen. Version and state ids are 72-byte buffers (`b3:` + 64 hex digits).
   `es_conflict_info` gained `container` and `unit_id`, and `es_event` gained `unit_id`, so a
   host can pass them to `es_unit_history` / `es_unit_restore`. `tests/c_abi.rs` compiles a C
   program that prints every size and offset and compares them with the Rust definitions.
6. **Events.** Strings of a polled event live in the client, keyed by a token in
   `_internal`, until `es_event_release`: a double release is harmless and `es_client_destroy`
   frees any left. `ES_EV_APPLY_BLOCKED` is reported once per unit and cause until it clears (a
   full card would otherwise repeat every cycle); its status comes from a new
   `engine::BlockKind` on every `Blocked` entry rather than from the reason text.
   `ES_EV_UNIT_UPLOADED` uses the new `SyncReport::uploaded`. Not emitted in v1: `PROGRESS`,
   `LEASE_CHANGED`, `HANDOFF_REQUESTED`, `STATE_AVAILABLE`. The queue holds 4096 events; beyond
   that the oldest are dropped and an `ES_EV_ERROR` says how many.
7. **Logging.** Engine code logs through the `log` crate, which has one logger per process. The
   library installs a router that sends each record to the client whose thread produced it
   (worker and helper threads, and host threads inside an API call, register their client in a
   thread-local). A Rust host that installed its own logger first keeps it, and the callback
   then gets nothing. A read-write "alive" guard ensures no log, wake or credential callback
   runs after `es_client_destroy` returns, even when the worker outlives its deadline.
8. **Credentials.** `emusync_remote::CallbackCredentials` wraps the host's provider: it caches
   the answer, refetches temporary credentials 5 minutes before `expires_unix_ms`, and drops
   them when the remote answers 401/403, after which `S3Remote` retries the request once
   (`CredentialProvider::invalidate`). The `es_credentials` buffer is zeroed before the call
   and wiped after. Only `ES_CRED_S3` is accepted.
9. **Not implemented, refused with `ES_E_INVALID_ARG`:** `ES_POLICY_PREFER_LOCAL` / `PREFER_REMOTE` (if two devices each prefer their own copy,
   their automatic merges never converge; needs a design), per-game containers (`title_scope`),
   `ES_CF_NO_UPLOAD`, OAuth (`es_oauth_*`) and save states
   (`es_state_*`, M3). Leases don't exist yet, so `other_device_active` is always empty and
   `ES_CFG_DISABLE_LEASES` (like the metered-network flags) has no effect. There is no
   background timer: hosts call `es_sync_now` when they want a cycle; `es_remote_add` doesn't
   start one.
10. **One client per data directory** per process (`ES_E_BUSY`), since two engines on one
    SQLite state would race on its sequence numbers and HLC.
    **Moving a card** (added later, compatibly): a logical card has one container per device,
    and adding a second one for it fails, unless the descriptor has `ES_CF_REPLACE`. Then the
    old container is unregistered (its files are left alone) and the card continues at the new
    location, which is scanned like any new container. Hosts whose card changes place use it, such as
    a Dolphin slot switched between a raw card and a GCI folder (§25.4); the CLI doesn't, and
    asks for another name. `ES_E_BUSY` while any session is open, since the replaced container
    may be one of its own; the engine also refuses to move a card whose container is in use.
11. **Build.** `crate-type = ["rlib", "staticlib"]`; the shared library is built with
    `cargo rustc -p emusync-ffi --lib --crate-type cdylib` (CI does this on every OS). Listing
    `cdylib` in `Cargo.toml` would make it collide on Windows with the `emusync` CLI binary,
    both writing `target/*/emusync.pdb` (cargo#6313). The header is still written by hand, not
    by cbindgen; the layout test keeps the two in step. The Android build is in §21.
12. **Tests.** `src/tests.rs` drives the exported functions as a host would, over the
    in-memory remote (attached through a test-only hook; the C API has no memory remote):
    session begin applying before the emulator opens the card, a pull that misses the
    deadline, busy cards and inbox, conflicts, history and restore, the debounce, logging and
    the notifier, credentials, argument and `struct_size` checks. `tests/c_abi.rs` builds with
    the platform's C compiler (via the `cc` crate) a C smoke test (create → add container →
    session begin/end → poll events → destroy), a C++11 compile of the header with every
    `ES_*_INIT` macro, and the layout check, each linked against the static library, with
    warnings as errors.

## 21. Implementation notes (Android build)

`libemusync.so` for Android, built the way [the ARMSX2 research](research/armsx2.md) (§4, "Best
way to link a cargo-ndk Rust library") proposed. This settles its inferred items 2, 3, 5 and 9,
by building and inspecting the library, with these results:

1. **How it is built.** `cargo rustc -p emusync-ffi --lib --crate-type cdylib --target
   <triple>`, with the NDK's `clang` as linker (`CARGO_TARGET_<TRIPLE>_LINKER`) and as the cc
   crate's C compiler (`CC_<triple>`, for ring, the bundled SQLite, zstd and blake3), and
   `--target=<clang triple><api>` in both the C flags and a link argument. Plain `clang` rather
   than the NDK's `<triple><api>-clang` wrappers, because those are `.cmd` scripts on Windows. No
   cargo-ndk. API 26 (ARMSX2's `minSdk`), arm64-v8a and x86_64; armeabi-v7a and x86 are mapped
   but untested. Two front ends do this: `scripts/build-android.sh` and
   `bindings/cmake/EmuSyncAndroid.cmake`.
2. **`cdylib` stays out of `Cargo.toml`** (§20 item 11: the Windows `.pdb` collision), so the
   research's "add `cdylib` to `crate-type`" became `cargo rustc --crate-type cdylib`, which
   builds the same library. The static library and its C tests are unchanged.
3. **Link arguments go after `--`** on the `cargo rustc` command line, so they reach only the
   final link: unlike `RUSTFLAGS`, changing them does not rebuild the dependencies.
   - `-Wl,-soname,libemusync.so`. **Confirmed** (research item 3): without it rustc writes no
     `SONAME`, and a host linked against the library would record its build path. With it, a
     shared library and an executable built through the CMake module both have
     `DT_NEEDED libemusync.so`.
   - `-Wl,-z,max-page-size=16384`. Every `PT_LOAD` is aligned to 16 KB. NDK r30 already does
     this by default for arm64; the flag keeps it so on older NDKs and x86_64.
   - `-Wl,--exclude-libs,ALL`. **Needed, which the research did not expect** (item 2): rustc's
     version script hides every symbol except those it exports, but it exports every
     `#[no_mangle]` function of *every* crate, not only this one. On arm64, blake3's NEON C code
     calls back into Rust through one, `blake3_compress_in_place_portable`, which therefore
     ended up in the dynamic symbol table. `--exclude-libs,ALL` takes the symbols of all archive
     inputs out of it. Rust dependencies are linked as `.rlib` archives and emusync-ffi's own
     code as object files, so this leaves exactly the `es_*` functions.
4. **`-DZSTD_TRACE=0`** in the C flags. zstd otherwise references weak, undefined
   `ZSTD_trace_*` hooks, which a definition anywhere else in the process (the host's own zstd)
   would resolve. Without them the dynamic symbol table neither defines nor references any
   zstd, SQLite, ring or blake3 symbol.
5. **What it needs.** **Confirmed** (item 5): `DT_NEEDED` is `libc.so`, `libm.so` and
   `libdl.so` only. No `libc++_shared.so`, and not even `liblog.so`. A release arm64 build is
   7.2 MB, 5.5 MB stripped (AGP strips it when packaging).
6. **Exports.** **Confirmed** (item 2): the defined dynamic symbols are exactly the 31
   functions declared in `include/emusync.h`. `scripts/check-android-so.sh` checks this against
   the header, along with the soname, `DT_NEEDED`, the absence of bundled C library symbols and
   the 16 KB alignment.
7. **The CMake module** follows ARMSX2's librashader build, with two changes. Cargo runs from an
   `add_custom_target` that always runs, rather than a custom command with a fixed output, so
   edits to emusync's sources are picked up. A no-op build takes under a second, and the
   library is a `BYPRODUCT`, so Ninja relinks the host only when it changed. It also needs no
   `libc++_shared.so` staging. It defines `emusync::emusync` (IMPORTED SHARED, with the header
   directory), copies the library into `CMAKE_LIBRARY_OUTPUT_DIRECTORY` after each build, and
   picks the cargo profile from `CMAKE_BUILD_TYPE`. It takes the compiler from
   `CMAKE_C_COMPILER`, the ABI from `ANDROID_ABI` and the API level from
   `ANDROID_PLATFORM_LEVEL`. **Item 7:** with no cargo or rustc, or without the Rust target for
   the ABI (`rustc --print target-libdir` does not exist), it sets `EMUSYNC_ANDROID_FOUND=OFF`,
   and configuring still succeeds.
8. **CI** (`android` job, ubuntu-latest, NDK 29.0.14206865 as ARMSX2 pins it). It builds both
   ABIs with the script and with the module (through `bindings/cmake/android-test`, a shared
   library and the C smoke test linked against `emusync::emusync`), runs the checks on all four
   libraries, checks the consumers' `DT_NEEDED`, and configures for armeabi-v7a, which has no
   Rust target installed, to check that the module switches itself off. It uploads
   `libemusync.so` for both ABIs with `emusync.h` as the `libemusync-android` artifact. The
   `android-emulator` job runs the C smoke test (§20 item 12) against the x86_64 library on an
   API 30 emulator. The runners cannot emulate arm64, so the arm64 library is checked only
   statically.
9. **Not verified yet** (the rest of research item 9). Linking into ARMSX2's `emucore`
   with `-DLTO_PCSX2_CORE=ON`, AGP packaging in a real APK, and `System.loadLibrary` resolving
   it on an arm64 device all belong to the ARMSX2 integration itself.

## 22. Implementation notes (end-to-end encryption)

Optional encryption of a repository (§7), in `emusync-core` (`crypto.rs`, `keystore.rs` and the
engine), the C ABI (`es_repo_create_encrypted`, `es_repo_unlock`) and the CLI (`emusync remote
add --encrypt`, `remote unlock`, `remote lock`). Where it differs from or pins down §6.1 and §7:

1. **Chosen at creation.** A repository is encrypted or not for its whole life: `repo.json` is
   create-only, so it can't change later. A data directory therefore remembers the
   `repo_id` and encryption header it was bound with, and refuses the remote if `repo.json`
   later reads differently: someone with write access to the bucket could otherwise swap in a
   format-1 `repo.json` with the same id and get a bound device to upload plaintext. Asking
   for encryption on an existing unencrypted
   repository is refused (`Error::Incompatible`, `ES_E_INCOMPATIBLE`); switching means a new
   prefix and a new data directory on each device. Unencrypted repositories are unchanged in
   every byte: `repo.json` format 1 with `"encryption": null`, blobs at
   `blobs/<hh>/<content hash>`, plaintext log segments and `device.json`
   (`tests/encryption.rs` checks this).
2. **`repo.json` format 2.** An encrypted repository has
   `{"format": 2, "repo_id": "<uuid>", "encryption": {"scheme": "xchacha20poly1305-blake3",
   "kdf": {"alg": "argon2id", "m_kib": 65536, "t": 3, "p": 1, "salt": "<hex>"},
   "wrapped_key": "<hex>"}}`. The version bump is what stops an **older client**: every
   release before this one refuses any format but 1 when the remote is added ("unsupported
   repository format in repo.json", `ES_E_INVALID_ARG` through the C ABI), so it never writes
   plaintext into an encrypted repository. This was checked against the CLI built from `main`.
   A format above 2 is refused with "newer than this version of emusync supports; update
   emusync" (`ES_E_INCOMPATIBLE`).
3. **Keys.** A random 256-bit repository key is wrapped with XChaCha20-Poly1305 under an
   Argon2id key from the passphrase (64 MiB, 3 passes, 1 lane, 16-byte random salt; 88 ms
   on a desktop CPU, not yet measured on a phone), with `emusync repository key <repo_id>` as associated data so a
   wrapped key can't be moved to another repository. A wrong passphrase fails the tag check:
   `Error::WrongPassphrase` / `ES_E_PASSPHRASE`. Two subkeys are derived from the repository
   key with BLAKE3's key derivation mode: a data key and an address key. The KDF parameters a
   `repo.json` may ask for are bounded, because `repo.json` isn't authenticated: at most
   256 MiB, 12 passes and 4 lanes, and at most 4 times the work of the defaults (memory ×
   passes), so whoever can write the bucket can't make a phone run out of memory or spend
   long on an unlock. Changing the passphrase is not
   implemented: it needs a second wrapped-key object, since `repo.json` can't be rewritten.
4. **Objects.** Every object a device writes besides `repo.json` (blobs, log segments,
   `device.json`) is `0x01 ‖ 24-byte random nonce ‖ XChaCha20-Poly1305(data key, plaintext)`,
   with the object's key in the repository as associated data, so the provider can't pass one
   object off as another. A blob's plaintext is the §16 blob encoding (format byte, zstd); a
   segment's is the NDJSON of §6.1. Random nonces make a retry write different bytes, so the
   lost-response check of §16 item 7 decrypts the existing segment and compares plaintexts. A
   segment that doesn't decrypt is skipped with a warning, like a malformed record, and nothing
   in it is used; a plaintext segment planted in an encrypted repository is therefore ignored.
5. **Content addresses.** §7 says content addresses use keyed BLAKE3. What is keyed is what the
   provider sees: a blob lives at `blobs/<hh>/<BLAKE3, keyed with the address key, of the
   chunk's "b3:…" id>`. Content hashes and version ids stay plain BLAKE3 everywhere else: in
   the logs, which are encrypted, and in the local state. That keeps a data directory's
   history independent of its remote. Versions snapshotted before an encrypted remote was
   added upload as they are, rather than needing their whole DAG rehashed, and nothing
   changes for unencrypted repositories. The provider can't compute a blob's name without the
   address key, so it can't check for a known save, and dedup still works because every
   device derives the same names.
6. **Kept on each device.** The unwrapped key is stored in `<data_dir>/keys/<repo_id>.key`, so
   the passphrase is entered once per data directory. On Windows the key is protected with
   DPAPI (`CryptProtectData`, current user, the repository id as extra entropy), so a copied
   data directory is useless to another user or machine. Elsewhere it is a 0600 file in a 0700
   directory. That is the documented fallback on macOS and Linux desktops (anyone who can read
   the user's files can read the key, as they can the saves), and on Android and iOS, where the
   data directory is the app's private storage. The ABI has no hook for a host keystore
   (Android Keystore, iOS Keychain). Adding one means appending a field to `es_config`, which a
   host built against the current header would then pass short and get `ES_E_ABI`, so it waits
   for the next reason to change the ABI. A key file that can't be read (another Windows user,
   or another repository's file copied in) is ignored with a warning, and the repository is
   locked.
7. **Locked.** A data directory bound to an encrypted repository without its key (added
   without the passphrase, or after `remote lock` / `Engine::forget_key`) is *locked*. Every
   network cycle fails with `Error::Locked` / `ES_E_LOCKED` before it reads or writes
   anything, and so does fetching a version's blobs. Local snapshots continue and upload once
   it is unlocked. `emusync watch` and `emusync service install` refuse to start while locked.
8. **C ABI** (still ABI version 1). Two status codes are appended, `ES_E_LOCKED = -11` and
   `ES_E_PASSPHRASE = -12`. Older headers don't name them, and `es_status_str` describes them;
   no function or struct changed. `es_repo_create_encrypted` keeps the passphrase for the next
   `es_remote_add` (the only order that works, since `es_remote_add` creates `repo.json`),
   which creates the repository encrypted or unlocks an existing encrypted one. If that remote
   is unreachable, the passphrase waits with it for the deferred setup (§20 item 1). Without a
   passphrase, `es_remote_add` returns `ES_E_LOCKED` for an encrypted repository whose key the
   data directory doesn't have. Once the setup succeeds, the setup thread hands it to the
   worker ahead of queued work and returns the outcome of binding it, so a failure there (the
   key can't be stored, say) is returned as an error rather than `ES_OK`. `es_repo_unlock` runs
   on the worker and waits for it. Passphrases are held in `Zeroizing` buffers and never
   logged.
9. **CLI.** A passphrase is read from the terminal without echo (`rpassword`), or as the first
   line of stdin when stdin isn't a terminal. There is no flag or environment variable for it.
   A new repository asks twice. A wrong passphrase at `remote add` leaves the remote added and
   locked, with a hint to run `emusync remote unlock`.
10. **Tests.** `crates/emusync-core/tests/encryption.rs`, over the in-memory remote: two
    devices syncing both ways; every version readable by a third device from the remote alone;
    no plaintext, content hash or device name in any object or object name; the passphrase
    once per data directory; locked and wrong-passphrase paths; versions made before the
    remote was added; tampered and planted segments and swapped blobs; unencrypted
    repositories unchanged; format and header checks; and the default Argon2id parameters.
    Unit tests in `crypto.rs` (wrap and unwrap, hostile headers, sealing, keyed names) and
    `keystore.rs` (DPAPI on Windows, 0600 elsewhere). In `emusync-ffi`: two devices through the
    exported functions (create, locked, wrong passphrase, unlock, restart without the
    passphrase) and the argument and error cases; the C smoke test and the C++ header check
    use the new functions and codes. For the CLI, `s3_encrypted_repository_between_two_data_dirs`
    runs against moto in CI (the `s3-moto` job). Argon2 and BLAKE2 are built with
    `opt-level = 3` in the dev profile, so debug builds and tests don't spend seconds on each
    key derivation.

## 23. Implementation notes (desktop build)

The C ABI as a shared library for desktop hosts: `emusync.dll` + `emusync.dll.lib` (Windows,
`*-pc-windows-msvc`), `libemusync.dylib` (macOS, optionally universal), `libemusync.so` (Linux).

- **Shared, not static.** The static library is fine for Rust-free tests, but a host that links
  it gets emusync's SQLite, zstd and ring in its own symbol namespace, next to its own zstd
  (PCSX2, Dolphin and PPSSPP all have one). The shared library exports only the `es_*` functions:
  on Linux rustc's version script plus `--exclude-libs,ALL` (as on Android, §21), on macOS rustc's
  exported-symbols list, on Windows only what is marked for export. rustc's macOS list includes
  every `#[no_mangle]` function of every crate, and BLAKE3's NEON code calls back into Rust
  through one (`blake3_compress_in_place_portable`), so Apple targets build BLAKE3 with
  `no_neon`; Linux and Android keep NEON and hide the callback with `--exclude-libs`. `-DZSTD_TRACE=0` drops zstd's
  weak trace hooks, as on Android. `scripts/check-desktop-lib.sh` verifies the export list
  against `emusync.h` on Linux and macOS.
- **Loading.** Linux: soname `libemusync.so`, so the host's `DT_NEEDED` is the bare name; the host
  ships the library next to its executable with an `$ORIGIN` rpath. macOS: install name
  `@rpath/libemusync.dylib`; an app bundle puts it in `Contents/Frameworks` with an
  `@executable_path/../Frameworks` rpath. Windows: the DLL next to the executable. It depends
  only on system DLLs and the Visual C++ runtime (`VCRUNTIME140.dll` and the UCRT), which the
  emulators already ship.
- **`bindings/cmake/EmuSync.cmake`** derives the Rust target from the platform
  (`CMAKE_OSX_ARCHITECTURES`, the Visual Studio generator platform, or the processor;
  `EMUSYNC_RUST_TARGET` overrides it), builds each with `cargo rustc --crate-type cdylib`, joins
  two macOS architectures with `lipo`, and defines `emusync::emusync` (with `IMPORTED_IMPLIB` on
  Windows). Multi-config generators get the release library for every configuration.
  `emusync_deploy_runtime()` copies the library next to a target after each build and adds the
  rpath. On Android it hands over to `EmuSyncAndroid.cmake`, so one include serves projects that
  build both from one tree. MinGW is not supported (the GNU Windows target would need its own
  import library handling, and none of the target emulators build with it).
- **`scripts/build-desktop.sh`** stages the same library with the header as a package for builds
  that don't use CMake: PCSX2's Windows release builds use MSBuild. The import library is renamed
  to `emusync.lib`, the usual name in `AdditionalDependencies`.
- **Verified:** on Windows 11 with MSVC (Visual Studio 18, Ninja), the CMake test project builds
  `emusync.dll`, links the C smoke test and runs it from its build directory; the DLL exports the
  31 `es_*` functions and nothing else; a plain `cl` build against the staged package links and
  runs. CI repeats the CMake build and smoke test on Windows (Visual Studio generator), macOS
  (universal, arm64 runner) and Linux, and the export and loader checks on macOS and Linux.

## 24. Implementation notes (host secrets)

Desktop hosts keep S3 keys and an encryption passphrase between runs. §7 says platform bindings
keep them in the OS keystore; without help each emulator would write its own code for three
stores. `emusync_core::secrets` and the C functions `es_secret_set`, `es_secret_get` and
`es_secret_delete` do it once:

- **Stores.** Windows: Credential Manager, a generic credential of the current user. macOS: the
  user's Keychain, a generic password. Linux: the Secret Service over D-Bus (GNOME Keyring,
  KWallet). All three through the `keyring` crate, 3.x (4.x needs Rust 1.88, above this
  workspace's 1.80), with its pure-Rust D-Bus client on Linux (`zbus`), so the library gains no
  `libdbus` dependency at build or run time.
- **Naming.** A secret is `service` (the host, `PCSX2`) plus `name` (the setting,
  `s3-secret-access-key`): that is the service and account Keychain Access and Credential Manager
  show the user. Both are limited to 64 letters, digits, spaces, `.`, `_` and `-`, because the
  fallback below uses them as file names. Values are 1 to 1024 bytes: Credential Manager holds at
  most 2,560.
- **Fallback.** Only on Linux, where a Secret Service may not be running (a headless machine, the
  Steam Deck in game mode, a minimal window manager): if the store is unavailable, the secret
  goes to `<fallback_dir>/secrets/<service>/<name>`, mode 0600 in 0700 directories, the same
  protection repository keys get there (§22; the writer is shared, `fsutil::write_private`).
  A secret set or deleted while the store is away also marks the store's copy, which can't be
  changed then, as stale (`<fallback_dir>/secrets/.stale/<service>/<name>`; `.stale` can't be a
  service, which can't start with a dot). The first read that reaches the store again deletes
  that copy and answers from the file, so neither an old value nor a deleted secret comes back;
  storing again moves the secret into the store and deletes the file. On Windows and macOS an
  unreachable store (a keychain the user refused to unlock) is reported as `ES_E_IO`, not
  silently replaced by a file. A read, write or delete that can't reach the store and has no
  fallback is `ES_E_IO` too: an unreachable store is never reported as "no secret"
  (`ES_E_NOT_FOUND`). These paths are tested with an in-memory store that can be switched off.
- **No client.** The calls are free functions, so a host can read its keys before
  `es_client_create`, and settings screens can store them without a running sync. They run
  under the same panic guard as the rest of the ABI. They may block while the system asks the
  user to unlock the keychain or keyring.
- **Output buffer.** `es_secret_get` writes the value and a NUL into the caller's buffer and
  reports the size needed; a buffer that is too small gets `ES_E_NO_SPACE` and an empty string,
  and `out = NULL, cap = 0` asks for the size only. Values are held in `Zeroizing` buffers inside
  the library.
- **Android and iOS** hosts keep secrets in the Android Keystore or the iOS Keychain from their
  own code (ARMSX2 does, §21); the calls fail there with `ES_E_INVALID_ARG`, and the library
  doesn't link `keyring` for those targets.
- **Tests.** Argument checks run everywhere, including the C smoke test, and never reach a
  store. The round trips through the real store (core and C ABI) write to the machine's
  credential store, so they run only with `EMUSYNC_TEST_OS_SECRETS=1`; CI sets it, which covers
  Credential Manager on Windows and the Keychain on macOS, and the fallback file on Linux runners,
  which have no Secret Service. Not tested: a real Secret Service (GNOME Keyring, KWallet).

## 25. Implementation notes (GameCube cards)

Two adapters in `emusync_formats::gc`, verified against Dolphin's source (`GCMemcard`,
`GCMemcardDirectory`, `GCIFile`) and against real Dolphin cards:

- `gc.card`: a raw card image, Dolphin's `GC/MemoryCardA.USA.raw` (also the format of real
  cards dumped with GCMM). File container.
- `gc.gci`: a Dolphin GCI folder, `GC/USA/Card A/`, one `.gci` file per save. Directory
  container (§19.3), Dolphin's default card type.

Both have the same units in the same canonical form, so a raw card on one device and a GCI
folder on another sync with no conversion, as PCSX2's folder and file cards do (§19).

### 25.1 The card

Blocks are 8 KB. Block 0 is the header (serial data, format time, size in megabits, encoding,
checksum), blocks 1 and 2 are two copies of the directory (127 entries of 64 bytes, an update
counter, a checksum), blocks 3 and 4 are two copies of the block allocation table (a counter,
the free count, the last block allocated, the next-block map, a checksum). Saves start at block
5. Checksums are the 16-bit big-endian sum and inverse sum of the area, with `0xFFFF` stored as
0. Sizes are 4 to 128 megabits (59 to 2043 save blocks).

Reading follows Dolphin and the BIOS: the header's checksum and size must hold; of each pair
the copy with the higher counter whose checksum holds is active (the first on a tie); a card
with more than one bad copy, or a directory that doesn't agree with the allocation table
(a save's chain running off the card, looping, or sharing blocks; free counts that don't add
up), fails `validate`, so it is never synced (§5 step 1). `probe` is `Certain` for an image of
a valid size whose header checksum holds; `detect` tries it after the PS2 adapter, so a
`.raw` file that isn't a GameCube card (Dolphin's `SRAM.raw`) stays `raw`.

Writing (`apply`) writes the new directory and allocation table into the **inactive** copies
with the counter one higher and fresh checksums, the way the BIOS commits a change, then the
data blocks. Untouched saves keep their blocks. A put allocates from the block after the last
one allocated, wrapping, as Dolphin does, and fails with `NoSpace` when there are no free
blocks or entries.

### 25.2 Saves and their canonical form

A save is its 64-byte directory entry plus its data blocks. Its identity, as on the console,
is the game code, maker code and file name up to the first NUL; its unit name is
`<gamecode><maker>-<filename>` (`GM8E01-MetroidPrime A`, bytes mapped one to one to Latin-1 so
Shift JIS names round-trip), its title key `gc:<gamecode><maker>` (`gc:GM8E01`).

The canonical bytes are a magic (`ES-GCU01`), the entry and the data, with what differs
between two copies of the same save normalised away: the first block (`0xFFFF`), the
modification time and the copy counter (zero), and the unused bytes (`0xFF`). The time and the
copy counter go to the meta key `gc.dentry` (not hashed) and are restored on apply, so Dolphin
and the BIOS still show when a save was written.

**Copy protection.** Three saves embed the card's serial number, derived from the header, and
check it: Phantasy Star Online's `PSO_SYSTEM` and `PSO3_SYSTEM` (serial plus a CRC-32 over part
of the save) and F-Zero GX's `f_zero.dat` (serial plus a CRC-16 over the save). Dolphin rewrites
those fields when it imports one of them into a card and when it loads a GCI folder. The
canonical form zeroes them, so one version of the save hashes the same on every card; `gc.card`
writes the destination card's serial and checksums back (the same arithmetic as Dolphin),
and `gc.gci` leaves them zero for Dolphin to fill in.

### 25.3 GCI folders

Units are the `.gci` files in the folder's root whatever their name, in path order. A file
that isn't a valid save (too short, a block count that doesn't match its size, an empty entry)
or has the identity of an earlier file is skipped and left alone, as Dolphin does when it
builds its virtual card; other files (Dolphin's `MC_SYSTEM_AREA`) are kept. A replaced save
keeps its file; a new one gets the name Dolphin would give it, `<maker>-<gamecode>-<filename>.gci`
escaped with Dolphin's `EscapeFileName`, with ` (n)` added if that name is taken; a deleted one
loses its file. A file's mtime is the save's modification time.

`probe` is `Certain` for a folder with at least one valid `.gci` file. An empty folder can't
be told from any other: `add_container` registers an empty directory as `gc.gci` when it is
Dolphin's `Card A` or `Card B` folder, and the format can always be given.

### 25.4 In the core

- Platform `gc` for both formats.
- Card names: a raw card's stem, `MemoryCardA.USA`. A GCI folder `<region>/Card A` gets the same
  name, `MemoryCardA.<region>`, since Dolphin keeps the raw card for that slot and region at
  `GC/MemoryCardA.<region>.raw`: switching a slot between the two in Dolphin, or using a
  different one on each device, keeps one logical card.
- Dolphin keeps the card in memory while a game runs (both types) and writes it back (§25.5),
  so like PCSX2 it needs the host to say when the card is in use; that is the Dolphin
  integration (roadmap item 6), and a `dolphin` profile for the sidecar is left for later.
- A GCI folder at a custom path is named only for its region (`X/USA`, §25.5) and needs the
  card name given; the Dolphin integration knows the slot and passes it.
- A host that names its cards by slot and region (Dolphin's integration: `MemoryCardA.USA` for
  slot A's USA card, raw or GCI) moves the card with `ES_CF_REPLACE` when the slot's type or
  path changes (§20 item 10).
- **Missing cards are created for synced saves.** Dolphin creates a card or GCI folder the first
  time a game uses it, so on a new device the card for a slot and region often doesn't exist
  yet, and the game's first boot there would not see the saves made elsewhere. A format can
  give the image of a new, empty container (`ContainerFormat::empty`): `gc.card` a card as
  Dolphin makes one (2043 blocks, formatted now, Shift JIS for `*.JAP.raw`, a serial of its
  own), `gc.gci` an empty folder. A scan that finds such a container missing (in unit mode, not
  read-only) in a folder that exists reconciles it as empty, with the image hash `missing` rather
  than one of an image, and `apply` creates it only if saves arrive for it and it is still
  missing: one that appeared meanwhile is the usual "changed during sync". A container whose
  folder is gone too is more likely on a drive that isn't mounted than new, and stays missing
  rather than being created on the disk underneath (Dolphin creates `GC/` and `GC/<region>/` at
  startup, so its cards' folders exist). Nothing is created
  for a card nobody has saves for. A card file deleted after syncing is the same case: its
  local records are dropped and its saves come back from the cloud, so deleting the file
  deletes no save anywhere. Other formats keep reporting a missing container.

### 25.5 How Dolphin handles cards

Read in Dolphin's source (`MainSettings.cpp`, `GameCubePane.cpp`, `EXI.cpp`, `EXI_Channel.cpp`,
`EXI_DeviceMemoryCard.cpp`, `GCMemcardRaw.cpp`, `GCMemcardDirectory.cpp`; upstream master of
October 2026, the same in the `ilyalissoboi/dolphin` fork) and confirmed in the tests below.
The Dolphin integration and a sidecar profile have to work with all of it.

- **Which card a slot uses.** Each slot has a device type (`[Core] SlotA`/`SlotB`: 1 Memory
  Card, 8 GCI Folder, 255 Nothing) and, separately, a raw card path (`MemcardAPath`) and a GCI
  folder path (`GCIFolderAPath`), empty for the defaults `GC/MemoryCardA.<region>.raw` and
  `GC/<region>/Card A`. The card in use is the device type plus the path for it, resolved for
  the running game's region (`GetMemcardPath`, `GetGCIFolderPath`).
- **The region is always in the path.** A raw card path the user picks gets `.<region>` before
  the extension (`X/test.raw` opens `X/test.USA.raw`, a new blank card if that doesn't exist)
  and a custom GCI folder gets `/<region>` (`X` opens `X/USA`); a region already in the name
  (`.USA`, `.EUR`, `.JAP`, `.DEV`; `/USA`, `/EUR`, `/JPN`, `/DEV`) is replaced. The settings
  pane refuses a file or folder whose name has no region part. NTSC-K games use the Japanese
  name.
- **Paths outlive the device type.** Setting a slot to Nothing, or to the other card type,
  keeps its paths, and the settings pane refuses a card or folder "already used" by the other
  slot by comparing with that slot's stored path, whatever its device type. A path can be
  freed only by pointing that slot elsewhere (or by editing `Dolphin.ini` with Dolphin closed).
- **Settings are written late.** `Dolphin.ini` is saved when the settings window closes and
  when Dolphin exits, so while Dolphin runs the file can disagree with what it uses, and an
  edit made while it runs is overwritten.
- **Changes while a game runs swap the card.** Changing a slot's device or path during a game
  unplugs the card for a second and inserts the new one (`ChangeDevice`), as if swapped by hand.
- **Save states restore the card type.** A state stores each slot's device type; loading a
  state taken with another type (a raw card in a slot now set to a GCI folder) creates the
  state's type, at the path configured for it. Card contents are not in a state (except
  while a movie runs), but the game's memory is, so a game resumed from a state can find the
  card's save different from the one it remembers (Rogue Leader: "This Memory Card contains
  different … File Data", offering to overwrite).
- **Raw cards are written whole.** A raw card is read into memory when the slot is set up and
  written back whole within 15 seconds after a game writes to it (showing "Wrote to Memory Card A"),
  and again, unconditionally, when the card is removed: game stop, a slot change, Dolphin
  closing. A file can be rewritten with identical bytes; `SRAM.raw` is written at the same
  moment.
- **GCI folders are written per save.** Dolphin builds a virtual card from the folder: the
  running game's saves always, other games' saves only while the card keeps about 10% of its
  blocks and 14 of its 127 entries free. A changed save is written to its own file a second
  after the game's last write ("Wrote save contents to GCI Folder"), with the first-block field
  set to its place on the virtual card (`0x0005`, where emusync writes `0xFFFF`; the field is
  not part of the canonical form, §25.2). A new save gets Dolphin's file name, with a `0`
  inserted before `.gci` on a clash. A deleted save's file is renamed to `<name>.deleted`,
  which `gc.gci` leaves alone as it isn't a `.gci` file.
- **The Memory Card Manager** (Tools) opens only slots whose device is a raw Memory Card, so
  with GCI folders it starts empty; it works on files, not on the cards in use.

### 25.6 Tests

Unit tests build cards and saves from scratch: Dolphin's checksum of an empty directory, blank
cards of every size, round trips, untouched saves keeping their blocks, full cards, copy
protection (same hash on two cards, the serial written back), every corruption the reader
rejects, Dolphin's file-name escaping, and saves moving between a card and a folder.
`tests/gamecube.rs` syncs a raw card on one device with an empty GCI folder on another, both
ways, including a copy-protected save. Real Dolphin cards (a USA and a PAL card with saves of
four games) were checked locally, not committed: detection, listing, re-putting every save,
deleting one, copying all to a blank card and to a GCI folder, and merging each card's saves
into the other.

**In Dolphin** (October 2026, `ilyalissoboi/dolphin` 2606-fork-6 on Windows, Star Wars Rogue
Squadron II: Rogue Leader, USA), with copies of those cards:

- A raw card emusync built (the USA card's saves plus the PAL card's, merged) in slot A: the game
  loaded its save, saved after a mission, and Dolphin wrote the card. The Rogue Leader save
  got a new time and the directory counter went up; its blocks and the three other saves were
  untouched, and `gc.card` validates and reads the result.
- A GCI folder emusync wrote from the same saves, in slot A: the game loaded and saved, and
  Dolphin rewrote only `64-GSWE-RogueLeader.gci` (new time, new data, first block `0x0005`);
  `emusync card inspect` detects the folder as `gc.gci` and lists all four saves.
- The first attempts saved elsewhere, which is how the behaviour in §25.5 was found: the region
  inserted into the chosen file name opened a new blank card, a slot path kept by a slot set to
  Nothing blocked the card from the other slot, and loading a save state taken with a raw card
  replaced the GCI folder set for the slot.

## 26. Implementation notes (Android AAR)

`bindings/android` is a Gradle build whose `:emusync` module produces the AAR, roadmap item 5:
what an Android integration shares, so the next ones (Dolphin, PPSSPP) don't copy ARMSX2's.

### 26.1 The host's native code owns the client

§10.2 sketched a Kotlin API that creates the client and is the integration. The integrations
built since put the client in the emulator's C++ core: it has to call the session hooks around
its own card I/O (§6.4), PCSX2 and ARMSX2 share that code, and Dolphin will share one C++
integration between desktop and Android. So the AAR does not wrap the C ABI for Kotlin, and has no
JNI of its own:

- **For the native code:** `libemusync.so` for each ABI and `emusync.h`, as a Prefab package
  (26.2). A host's CMake build links it with `find_package(emusync REQUIRED CONFIG)` and
  `emusync::emusync`, with no Rust toolchain; the AAR's `jni/` folder puts the library in the
  APK.
- **For the Kotlin side:** what ARMSX2's `MemcardSyncSettings` did, made generic (26.3): the
  settings with their secrets in the Keystore, the data folder per storage, starting and stopping
  sync in the background, and telling the native code when the app leaves the screen. It drives
  the native side through `SyncBackend`, a small interface the host implements over its own JNI
  (start with a config, stop, running, sync now, status, backgrounded).

### 26.2 Building the AAR

- **The library:** a task runs `scripts/build-android.sh`, the build CI checks (§21), for each
  ABI (Gradle property `emusync.abis`, default `arm64-v8a,x86_64`; API 24; NDK 29.0.14206865
  like ARMSX2) into a generated `jniLibs` directory of the variant, emptied first so an ABI from
  an earlier build with other ABIs isn't packaged too. Cargo decides what to rebuild, so the task
  always runs. On Windows it uses Git's bash (`emusync.bash` overrides it): a plain
  `bash` there can be WSL's. Building the AAR therefore needs Rust with the Android targets;
  using it doesn't.
- **The Prefab package:** AGP's Prefab publishing only publishes libraries its own CMake build
  links, and cargo links this one. So a task added with AGP's artifact API
  (`SingleArtifact.AAR` transform) writes the package into the finished AAR:
  `prefab/prefab.json` (schema 2, the workspace version), `prefab/modules/emusync/module.json`,
  `include/emusync.h`, and `libs/android.<abi>/` with `abi.json` (API 24, NDK 29, STL `none`,
  shared) and the unstripped library. The copies in `jni/` are the ones AGP strips for APKs.
  Both pass `scripts/check-android-so.sh`.
- **Consumers in the same build** can't take the Prefab package from the project, since it only
  exists in the `.aar`, so `:prefab-test` depends on the `.aar` file. So does a host: it
  downloads the `.aar` of a release (26.5); a Maven publication is left for later. A file
  dependency doesn't bring the AAR's own dependency, `androidx.lifecycle:lifecycle-process`, so
  such a host adds it too.

### 26.3 The Kotlin API

Package `io.github.ilyalissoboi.emusync`, `minSdk` 24 (Android 7, Dolphin's minimum). The
library links only libc functions Android 7 has, but one: `getrandom` (Android 9), which is a
weak reference the `getrandom` crate checks, falling back to the system call:

- **`SyncSettings`:** on/off, storage address, access key ID and device name in
  SharedPreferences; the secret access key and the encryption passphrase sealed with AES-GCM
  under a non-exportable Android Keystore key, so a backup restored on another phone reads as
  "not set". `dataDir(remoteUri)` is `noBackupFilesDir/emusync-<first 8 bytes of SHA-256 of the
  address, hex>`, one folder per storage, never backed up (two installations sharing a device
  identity would mix their logs). The preferences name and key alias are parameters, and ARMSX2's
  (`memcard_sync`, `armsx2.memcardSync.secret`) keep its stored settings when it moves to the AAR.
  `config(appVersion)` gives the `SyncConfig` to start with, or null when sync is off or
  incomplete. `secretAccessKey()` and `passphrase()` read the secrets back for a settings
  screen's Show. `SyncConfig.toString()` hides the secret and the passphrase.
- **`SyncController`:** `start()` (blocking), `startAsync`, `startInBackground` and `stopAsync`
  run on one thread of the controller's, in order, each reading the settings when it runs; a
  settings screen that closes right after a change can't cancel its restart. `start` stops sync
  when the settings are off or incomplete, and turns a backend exception into the sentence it
  returns, a failed stop included (sync may still be running then). `attachToAppLifecycle()` follows `ProcessLifecycleOwner` (the app's first activity
  started, its last one stopped) and calls `SyncBackend.setAppBackgrounded` on the main thread,
  which must not block: what to do then depends on the emulator (ARMSX2 queues it behind the
  CPU thread's card flush). A host that follows its own activity calls `setAppBackgrounded`
  itself. A backend that reports `isAvailable = false` (a build without sync) is never called.

### 26.4 Tests

Instrumented, on CI's API 30 x86_64 emulator (`android-emulator` job, after the C smoke test):

- `SyncSettingsTest`, with the real Keystore: defaults, trimming (not of the passphrase), that
  no secret is in the preferences in the clear, removing values, a value whose key is gone (a
  restored backup) reading as not set and being enterable again, and the data folder per
  storage.
- `SyncControllerTest`, against a fake backend: incomplete settings stop, complete ones start
  with their config, errors come back as sentences, a build without sync is never called, async
  starts run in order and report on the main thread, and lifecycle calls reach the backend on
  the main thread.
- `:prefab-test`, an app that consumes the `.aar` as a host would: its JNI library links
  `emusync::emusync` through Prefab (CI checks its `DT_NEEDED` is the bare `libemusync.so`), the
  APK packages both libraries, and its test loads them and creates and destroys a client in the
  app's sandbox.

Not done here: a Maven publication, WorkManager for uploads after the app is closed (roadmap
item 13, when ARMSX2 moves to the AAR), and a Kotlin SAF implementation of `es_vfs` (§28.1).

### 26.5 Releases

A tag `vX.Y.Z` that matches the workspace version (`Cargo.toml`) runs `.github/workflows/release.yml`.
It builds the AAR and checks it as CI does: the libraries, the header, and the Prefab version.
Since 0.2.0 it also builds the desktop libraries with `scripts/build-desktop.sh` and checks
them as CI's desktop job does. Then it publishes a GitHub Release with:

- `emusync-X.Y.Z.aar`;
- `emusync-X.Y.Z-windows-x64.zip`: `bin/emusync.dll` with its `.pdb`, `lib/emusync.lib` (the
  import library), and `include/emusync.h`;
- `emusync-X.Y.Z-linux-x86_64.tar.gz` and `emusync-X.Y.Z-macos-universal.tar.gz`: `lib/`
  and `include/`;
- `emusync.h` and `SHA256SUMS`.

A host pins a version and its checksum, and downloads what it needs in its build. That way
neither its developers nor its CI need Rust: Dolphin's Android build takes the AAR, and
PPSSPP's Visual Studio build takes the Windows package. Tags are pushed by hand, after the
commit they name has passed CI.

## 27. Implementation notes (PSP saves)

Roadmap item 7: the `psp.savedata` adapter, and folders of save folders in the core.

### 27.1 The memory stick

As read in PPSSPP's source (`Core/Dialog/SavedataParam.cpp`):

- **Saves:** a save is the folder `PSP/SAVEDATA/<gameName><saveName>`, for example
  `ULUS10041S0`: the game's product code, then the name the game gives the save. It holds:
  - `PARAM.SFO`: the title, the save's title and detail, its directory name, and a file list
    with hashes;
  - the icons and the background (`ICON0.PNG`, `ICON1.PMF`, `PIC1.PNG`, `SND0.AT3`);
  - the game's data files.

  A secure save is encrypted with the game's key, not the device's, so a copied folder loads on
  another PSP or in PPSSPP. Some games also write folders there with their own file I/O.
- **Install data:** game data installed from the disc (PPSSPP's `PSPGamedataInstallDialog`) goes
  to the same folder, as `<gameName><dataName>`. Its `PARAM.SFO` has the same `CATEGORY "MS"` as
  a save, but no file list. It is often hundreds of MB.
- **Other files:** on Android, PPSSPP creates `SAVEDATA/.nomedia`.
- **Cached `PARAM.SFO`:** PPSSPP caches each save's `PARAM.SFO` (`LoadCachedSFO`). The PPSSPP
  integration (item 9) has to drop that cache when it replaces a save folder while the
  emulator runs, or replace save folders only between sessions.

### 27.2 The adapter (`psp.savedata`)

- **Units:** a unit is a save folder, named as the folder. A folder whose first nine characters
  are a product code (four capitals, five digits) has the title `psp:<code>`, for example
  `psp:ULUS10041`.
- **Canonical form:** the folder's tree image (§19.3), with paths relative to the folder and
  every modification time zero, so the same save on two devices hashes the same.
  - The times travel in the `psp.mtimes` meta, one line per entry (`<seconds> <path>`, the
    folder itself as `.`).
  - Apply restores them: PPSSPP and the PSP show a save's date from them.
- **Registration:**
  - A folder named `SAVEDATA`, in any case, is registered as `psp.savedata` by its name,
    without being read, even if it doesn't exist yet.
  - Its card is `psp:SAVEDATA` on every device.
  - `empty()` gives an empty folder, so a missing `SAVEDATA` is created when saves arrive, if
    its `PSP` folder exists (§25.4).
- **What isn't a unit:**
  - A folder over `MAX_SAVE_SIZE`, 32 MiB, is install data, not a save. Saves are rarely over
    2 MB.
  - Files directly in `SAVEDATA` aren't units either.

  Neither is read, synced, replaced or deleted.

### 27.3 Folders of save folders in the core (`ContainerKind::UnitDirs`)

A §19 directory container is read whole on every scan and replaced whole on every apply. A
memory stick can hold hundreds of saves and GBs of install data, so `SAVEDATA` is a third kind
of container, whose subdirectories are the units. `emusync-core::unitdirs` handles each save
folder on its own:

- **Scan.**
  - **Listing:** the folders are listed from file metadata alone: each folder's *signature*
    is a BLAKE3 hash of every entry's path, kind, size and modification time (in ns).
  - **Reads:** a folder is read and extracted only if its signature differs from the one
    recorded when it was last read (the `unit_signatures` table), or if it has none.
  - **Recent writes:** a folder with an entry modified in the last 3 s gets no recorded
    signature, so the next scan reads it again (git's rule for racy timestamps).
  - **Over the size limit:** a folder over the limit still counts as present, so a save that
    grows past the limit isn't deleted on other devices.
  - **The image hash:** the scan returns a hash of every folder's name and signature, in
    place of the image hash.
- **Apply.**
  - **The check:** the listing's hash must still be the one the scan returned.
  - **Each change, on its own:**
    1. Read that save folder.
    2. Run the adapter on an image holding only it.
    3. Check the result holds exactly the version, or nothing for a deletion.
    4. Keep a snapshot of the old save.
    5. Replace the folder with `fsutil::replace_dir`, or remove it with `fsutil::remove_dir`:
       a rename to `<name>.emusync-old`, then a delete.
    6. Record the local unit and its new signature.

    A change that fails holds back only itself.
- **Recovery.**
  - **At scan:** while the container isn't busy, each scratch sibling inside it
    (`<name>.emusync-tmp`, `-old` or `-swap`) is recovered as in §19.3.
  - **A leftover `-old`:** `recover_dir_replace` now also deletes an `-old` tree that has no
    journal. Only `remove_dir` leaves one, since `replace_dir` writes its journal before it
    renames anything.
- **What the core does without reading every save:**
  - It refuses whole-file mode for these containers.
  - `cards()` and `emusync card inspect` list the save folders.
- **Limit:** a save rewritten in place with every size and modification time unchanged isn't
  seen until it changes again.
  - The 3 s rule covers rewrites within one timestamp tick of the last read, even on FAT (2 s
    ticks).
  - It doesn't cover a game that writes the same size and then sets the old time back.
    rsync's quick check makes the same trade.

### 27.4 Tests

- **Adapter (`psp.rs`):** units are the folders, with titles; times are meta and come back on
  apply; put and delete; bad names and content are refused.
- **`unitdirs`:**
  - listing skips files, scratch folders and folders over the limit;
  - signatures follow changes;
  - folders are read, written and removed one by one;
  - an interrupted removal is finished.
- **Engine (`tests/psp.rs`):**
  - two memory sticks exchange new, changed and deleted saves, while 33 MB of install data and
    `.nomedia` stay untouched;
  - a missing `SAVEDATA` is created;
  - a folder with unchanged metadata isn't read again;
  - whole-file mode is refused.

Not done here: testing against PPSSPP itself, which comes with its integration (roadmap
item 9). Android's `content://` memory sticks go through a host's `es_vfs` (§28).

## 28. Implementation notes (host storage: `es_vfs`)

Roadmap item 8. On Android 11 and later, an app can reach shared storage only through folders
the user picks with the Storage Access Framework. PPSSPP keeps its memory stick in one of those,
as `content://` documents, which have no file paths. The engine now reaches every container
through a VFS, so a host can keep its containers in such storage.

### 28.1 Who implements what

The host implements the storage operations in C (`es_vfs`), and emusync never calls Java. PPSSPP
already has a native SAF layer (`Android_ListContentUri`, `Android_CreateFile`,
`Android_RenameFileTo`, file descriptors for documents) behind its `File::` and `Path` APIs, so
its integration (item 9) wraps those. A host without SAF code of its own would need a Kotlin
implementation; the AAR could ship one later, bridged through JNI.

### 28.2 The core: `emusync-core::vfs`

- **The trait:** `Vfs` has primitives:
  - reading: `stat`, `list` (entries with their metadata), `read` (a whole file);
  - writing: `write` (create or truncate, durable), `create_dir`, `rename` (within a
    directory, never over something), `remove` (a file, or a directory with everything in
    it), and `set_mtime` (best effort);
  - location arithmetic: `join`, `parent`, `name`.

  A location is a string only the VFS composes: a native path, or a URI.
- **Built on them:** the trait builds `read_tree`, `replace_dir`, `replace_file`, `recover` and
  `remove_dir` from the primitives.
  - Files now use the same journalled renames as directories (§19.3): write `<name>.emusync-tmp`,
    write the journal `<name>.emusync-swap`, rename `<name>` to `<name>.emusync-old`, rename the
    new one to `<name>`, then remove the old one and the journal.
  - SAF can't rename over a document, so the old "journalled copy" plan of §5, step 5 isn't
    needed.
  - `recover` rolls an interrupted replace forward or back, for files and directories alike.
- **`NativeVfs`, the default:** it keeps the code from before, overriding the built operations
  with `fsutil`. Files are still replaced with one atomic `rename`, with the same fsyncs and
  the same Windows lock detection.
- **`MemoryVfs`, for tests:** it holds everything in memory, can't set times and never renames
  over anything, as SAF behaves.
- **Where it is used:**
  - `EngineOptions::vfs` selects the VFS. The engine's container reads and writes, detection
    in `add_container`, `creatable`, whole-file metadata and `unitdirs` (§27) all go through
    it.
  - Only a native location is made absolute when a container is added.
  - The data directory stays native.
- **Locked files:** a VFS reports a file another program holds with `vfs::InUse`, which
  `fsutil::is_locked` recognises, so the card waits instead of failing.
- **A failed read:** when a host's storage fails to read a card file, only that card is held
  back, as for directories. It doesn't abort the cycle.

### 28.3 The C ABI

- **Reserved until now:** `es_config.vfs` was reserved, and refused with `ES_E_INVALID_ARG`, so
  no host could have used the old `es_vfs` layout. It is redefined here within ABI version 1.
  - The old layout was handle-based: `open`, `read_at`, `sync`, `replace`, `sibling`.
  - The new one is whole-file and name-based: `stat`, `list`, `read` (through a sink),
    `write`, `mkdir`, `rename`, `remove`, `set_mtime` (may be NULL), `join`, `parent` and
    `name`.
  - The C layout test checks the new structs.
- **Status codes:** the functions return `ES_OK`, `ES_E_NOT_FOUND`, `ES_E_BUSY` or `ES_E_IO`.
  - `ES_E_NOT_FOUND` matters, for example for a card that doesn't exist yet.
  - `ES_E_BUSY` becomes `InUse`.
- **Strings:** the string functions write into a caller's buffer and return the length needed,
  so emusync calls again with a larger buffer.
- **The host's side:** `es_client_create` copies the struct and checks that every function but
  `set_mtime` is set. The functions are called from the client's worker thread, one call at a
  time.
- **After destroy:** every call goes through the client's callback gate, as the log callback
  and the credential provider do. So none runs once `es_client_destroy` returns, even when its
  timeout left the worker finishing a sync in the background; the worker then gets errors.
  Destroy waits for a call in progress.
- **`HostVfs` (`emusync-ffi/src/vfs.rs`)** adapts the struct to the trait.

### 28.4 What changes on storage without times

On SAF, `set_mtime` is NULL, so a file written by emusync keeps the time it was written at.

- **Folder cards and GCI folders:** they read back with new times, which only changes the
  container's image hash, not its saves.
- **PSP saves:** they keep their `psp.mtimes` meta (§27.2). PPSSPP shows the date of the
  write, not of the save.
- **Signatures:** a save folder's signature (§27.3) is recorded from what the storage reports
  after the write, so it stays valid.

### 28.5 Tests

- **`vfs` unit tests:**
  - files and directories are replaced, and removed;
  - the protocol, interrupted after each step, recovers for both;
  - native paths, including times and scratch names.
- **`unitdirs`:** read, write and remove run on the native file system and on `MemoryVfs`.
- **Engine (`tests/vfs.rs`):** a device whose storage is a `MemoryVfs` syncs with a native one:
  - a PS2 card file both ways;
  - an empty Dolphin GCI folder, recognised by its place (`GC/USA/Card A`) through the VFS's
    `name` and `parent`, which receives a save;
  - a PSP `SAVEDATA`, created under `PSP` and synced both ways;
  - a card replace interrupted on the VFS, finished at the next scan.
- **FFI:** `es_client_create` checks the struct.
  - C callbacks written in Rust over a `MemoryVfs` back a client whose card exists only behind
    them: it receives a save, its own save reaches the PC, and nothing is left behind.
  - The C layout test covers `es_vfs` and `es_vfs_stat`.

Not done here: a host implementation. PPSSPP's comes with its integration (item 9).

## 29. Implementation notes (PS1 cards)

One adapter, `emusync_formats::ps1` (`ps1.card`), checked against the PS1 memory card layout
documented in psx-spx (nocash) and against what DuckStation reads and writes
(`memory_card_image.cpp`, read for interoperability only: DuckStation's licence forbids reuse).

### 29.1 The card

A card is 128 KB: 16 blocks of 8 KB, each 64 frames of 128 bytes. Block 0 is the card's own:

- **Header:** frame 0, `MC`. Frame 63 (the write test) is a copy of it.
- **Directory:** frames 1 to 15, one entry for each data block 1 to 15. An entry has a state,
  the save's size in bytes and its name (both only in the save's first block), and the next
  block of the save's chain (`0xFFFF` at the end).
- **States:** `0x51` first block of a save, `0x52` middle, `0x53` last. `0xA0` a block unused
  since formatting. `0xA1`, `0xA2`, `0xA3` a deleted save's blocks: the BIOS deletes a save
  by marking its entries and leaves its data, so it can be undeleted.
- **Broken-sector list:** frames 16 to 35, with replacement data in 36 to 55: a sector listed
  there is read from its replacement frame instead. Only a worn real card lists any (every
  emulated card checked lists none), and emusync doesn't read through the list, so a card that
  lists a sector isn't synced.
- **Checksums:** each directory frame ends with the XOR of its other 127 bytes. They are
  written correctly, but a wrong one alone doesn't fail `validate`: DuckStation doesn't check
  them either, and a card that works in the emulator should sync.

**Containers.** Every emulator that keeps a PS1 card as a file uses the raw 128 KB image:
DuckStation (`.mcd`), ePSXe and mednafen (`.mcr`), Beetle and SwanStation in RetroArch
(`.srm`), PCSX2 and ARMSX2. Two headered variants are read and written too, with their header
kept: DexDrive's `.gme` (3904 bytes of header, `123-456-STD`) and Connectix Virtual Game
Station's `.mem` (64 bytes, `VgsM`). A `.gme` header holds copies of the directory states and
links, which are updated, and a description for each block, cleared when its block changes.
PSP and PS3 `.VMP` cards are left for later: every write needs their signature recomputed,
and there was no real file to check it against.

**Validation** (a card that fails is never synced or written, §5 step 1):

- the container is one of the three sizes, with the `.gme` or `.mem` magic for those;
- the header starts with `MC` (an unformatted card fails);
- every directory state is one of the seven above;
- each save's chain is well formed: links stay on the card, reach only `0x52`/`0x53` blocks,
  the last block is `0x53`, no loop, no block in two chains;
- no in-use block outside a chain (a `0x52` whose first block is gone);
- every save has a name, and no two saves have the same one;
- a save's size field is its chain's size (whole blocks), as the BIOS and emulators write it;
  a canonical unit is held to the same;
- the broken-sector list is empty.

**Detection.** `probe` is `Certain` for a container whose header is `MC` with a correct
checksum, `Maybe` for one of the three sizes that isn't formatted. `detect` tries it after the
PS2 and GameCube adapters, by content, since RetroArch's PS1 cards are `.srm` files like
cartridge SRAM; an unformatted file is `ps1.card` only with a PS1 extension or inside a `.gme`
or `.mem` container, known by its magic.

**Writing** (`apply`), folded into one rewrite of the directory:

- a deleted save's entries are marked `0xA1`/`0xA2`/`0xA3`, its data left, as the BIOS does;
- a written save takes, in order: the blocks of the save it replaces, blocks unused since
  formatting, then whole deleted saves. A deleted save is overwritten only whole, the rest of
  its entries cleared, so nothing that can be undeleted runs into another save's blocks;
- untouched saves keep their blocks and entries;
- written entries are zeroed apart from their fields, with a fresh checksum;
- `NoSpace` when the card's 15 blocks don't fit the save.

### 29.2 Saves and their canonical form

A save is identified by its name, up to 20 characters, as on the console: the BIOS refuses a
second save with the same name. Its unit name is the name, bytes mapped one to one to
Latin-1. A save's name is the game's serial behind a region code (`BA` America, `BE` Europe,
`BI` Japan) and up to 8 characters of the game's own (`BASCUS-94163FF7-S01`). Its title key is
the serial: `ps1:SCUS-94163`. A PocketStation program has `P` for the serial's `-`, and its key
has the `-`.

The canonical bytes are a magic (`ES-P1U01`), the 21-byte name field, the size field, and the
data blocks in chain order. Those are what the game wrote: the first block starts with the
title frame (`SC`, icon flag, Shift JIS title, palette), then the icons and the data. Nothing
is moved to meta: a PS1 card keeps no times.

### 29.3 In the core

- Platform `ps1`, whatever the file's extension.
- Card names are file stems, as elsewhere (§2.1). Emulators name their cards differently
  (DuckStation's `shared_card_1.mcd`, ePSXe's `epsxe000.mcr`, RetroArch's `<game>.srm`), so
  matching two of them needs the card name given, which the sidecar profile (roadmap item 11)
  will do for DuckStation.
- **Missing cards are created** (as in §25.4): DuckStation formats a card in memory when its
  file doesn't exist and writes it on the first save, so `empty` gives a card formatted the way
  DuckStation does it. A `.mcd` or `.mcr` that doesn't exist yet is registered as `ps1.card`. A
  missing `.gme` or `.mem` stays missing.

### 29.4 Tests

- **Unit tests:**
  - a blank card matches DuckStation's format (header checksum `0x0E`, a free entry's `0xA0`,
    the write-test frame);
  - title keys, including PocketStation names;
  - round trips, replacing and deleting (the BIOS's deleted states, data kept);
  - untouched saves keep their blocks, and unused blocks are used before deleted saves, which
    go whole;
  - full cards, and a 21-character name without its NUL;
  - `.gme` and `.mem` keep their header, and a `.gme`'s directory copies and descriptions
    follow;
  - every corruption the reader refuses, and a wrong checksum alone accepted;
  - a deleted save the BIOS partly overwrote is free space.
- **`tests/ps1.rs`:**
  - DuckStation's `.mcd` and RetroArch's `.srm`, bound to one card, sync both ways, deletions
    included, and offline saves of different games merge;
  - a DexDrive `.gme` syncs with a raw card and keeps its header;
  - a missing `.mcr` is created for the saves synced to it.
- **Real cards,** checked locally and not committed: 41 card files from DuckStation (its
  `shared_card_1.mcd` and per-game cards), ePSXe, PCSX2, PCSX-R and RetroArch (Beetle's `.srm`
  and `.mcr`), 19 saves on 5 of them, the rest formatted and empty.
  - All were detected as `ps1.card` and validated.
  - Re-putting every save of a card left the file byte for byte the same, so emusync writes
    directory entries exactly as those emulators do.
  - Copying all saves to a blank card, deleting one, and merging each card into the next (or
    `NoSpace` when they didn't fit) kept every save.
  - `emusync card inspect` lists them with their title keys, PocketStation-style names
    (`BASLUSP00892…`) included.

## 30. Dropbox backend (design draft)

Status: design, not implemented. The second `Remote`, after S3 (§6.2), and the first that
signs in with an account instead of keys. With S3 the user has to create a bucket, a policy and
keys (§15, question 6). With Dropbox they press "Sign in with Dropbox" and approve. Why Dropbox
before Google Drive or OneDrive:

- **Its API fits the layout.** Files are addressed by path, so keys map onto it one to one.
  Uploads can be create-only on the server, so the "never overwrite" rule (§6.1) holds without
  the HEAD-before-PUT fallback. Folders are created implicitly.
- **Light registration for the developers.** One app in the Dropbox App Console, and a short
  review for production status (30.1). Google needs brand verification, with a privacy policy
  and a verified domain. Microsoft needs an Entra tenant.
- **Free for the user.** Dropbox Basic has 2 GB, far more than saves need. Its three-device limit
  covers Dropbox's own apps, not apps that use the API (Dropbox staff on its forum, not yet in
  its documentation).

### 30.1 One Dropbox app for every host

- **The app.** The emusync developers register one Dropbox app, `emusync`, with the
  **App folder** permission type: it sees only `/Apps/emusync/` in the user's Dropbox, which the
  user can browse. Scopes: `files.metadata.read`, `files.content.read`, `files.content.write`
  (Dropbox adds `account_info.read` to every app).
- **Its app key is in the library**, a constant in `emusync-remote`, not in each host. It is
  public: sign-in uses PKCE (30.4), so no client secret ships anywhere. Every host (the CLI,
  PCSX2, ARMSX2, Dolphin, PPSSPP) must use the same key, because a different app has a different
  app folder and so a different repository. A fork that registers its own app passes
  `?app_key=…` in the URI (30.2), and gets its own folder.
- **Production status.** A new app is in development status: up to 500 users can link it, but
  once 50 have, the developers have two weeks to be approved for production status, or no new
  user can link it until they are. The application asks how the app uses the API and for an
  icon, with no identity documents. It is reviewed only after 50 users have linked the app, so
  apply when the first release with Dropbox ships, and plan for the two-week window.
- **Redirect URIs** are registered in the App Console: a fixed loopback port for desktop and
  one custom scheme per Android host (30.4).
- **The App Console account** belongs to the project, not to one developer's personal Dropbox
  account: if it is lost, every user's sign-in stops working.

### 30.2 Repository mapping

```
dropbox:/saves                        → /Apps/emusync/saves/repo.json, …/blobs/…, …/devices/…
dropbox:/saves-encrypted              a second repository next to it
dropbox:/                             the app folder itself
dropbox:/saves?app_key=<key>          a fork's own Dropbox app
```

- **Keys** are paths under the repository folder, unchanged: `blobs/ab/abcd…` is
  `/saves/blobs/ab/abcd…` inside the app folder.
- **Case.** Dropbox paths are case-insensitive and keep the case they were created with. Every
  key the core writes is lower-case ASCII (hex hashes, UUIDs, fixed names), so no two keys fold
  together. The backend refuses a key with upper-case letters (`RemoteError::Other`), so a
  future key scheme can't collide silently. Listings use each entry's `name`, never
  `path_lower`.
- **Which account.** The URI doesn't name the Dropbox account. A data directory bound to one
  account's repository that is later signed in to another sees a different `repo.json` and is
  refused (the binding check of §20). Hosts keep one data directory per URI *and* account: the
  Android AAR hashes the account id into the data folder name with the URI (30.6).
- **`describe()`** returns the URI, so `emusync status` and `es_remote_add`'s binding see
  `dropbox:/saves`.

### 30.3 Operations

Hand-written over the existing `ureq` agent with rustls and `serde_json`, about the size of the
S3 backend. The official `dropbox-sdk` crate generates code for every route of the API and
would be most of the backend's compile time for five calls. RPC calls go to
`api.dropboxapi.com`, uploads and downloads to `content.dropboxapi.com`, with the arguments as
JSON in the `Dropbox-API-Arg` header (ASCII only, which keys are).

| `Remote` | Dropbox call |
|---|---|
| `put(key, data, if_none_match = true)` | `files/upload` with `mode: add`, `autorename: false`, `strict_conflict: true`, `mute: true`. A `path/conflict` error is `AlreadyExists`. |
| `put(key, data, false)` (a renamed device's `device.json`) | `files/upload` with `mode: overwrite`. Dropbox keeps the old revision in its own history. |
| `get(key)` | `files/download`. `path/not_found` is `Ok(None)`. |
| `head(key)` | `files/get_metadata`. True only for a file; `path/not_found` is false. |
| `list(prefix, start_after)` | `files/list_folder` on the prefix's folder (non-recursive, `limit: 2000`), then `files/list_folder/continue` while `has_more`. Entries come in no particular order: files are sorted by name and filtered past `start_after`. A missing folder is an empty list. |
| `list_dirs(prefix)` | The same listing, keeping folder entries. |
| Garbage collection (§7, later) | `files/delete_batch`. |

- **Create-only writes need `strict_conflict`.** With `mode: add` alone, uploading the same bytes
  as an existing file is not a conflict: nothing is written and the upload "succeeds". That
  would defeat the device-id clash check (§6.2): an unencrypted log segment written again with
  the same bytes is meant to come back `AlreadyExists`, so the engine compares it (`engine.rs`,
  the segment loop). It would also make `probe_conditional_writes`, which writes the same bytes
  twice, report no conditional writes. `strict_conflict: true` makes identical contents a
  conflict too, so the probe finds conditional writes and the engine never falls back to
  HEAD-before-PUT.
- **Sizes.** Chunks are at most 256 KB and segments are small, far under the 150 MB a single
  `files/upload` takes, so upload sessions are needed only for batching (30.7). Empty files are
  allowed (the conformance suite writes one).
- **Consistency.** Dropbox lists a file once its upload has returned. The protocol doesn't rely
  on that anyway (§6.2).

**Errors.** Dropbox answers endpoint errors with HTTP 409 and an `error_summary` such as
`path/not_found/..`:

| Response | Handled as |
|---|---|
| 409 `path/not_found` | `None`, false or an empty list, by operation |
| 409 `path/conflict` on a create-only upload | `PutOutcome::AlreadyExists` |
| 409 `path/insufficient_space` | `RemoteError::Other("the Dropbox account is full")`: the outbox keeps the changes, and the next cycle tries again |
| 401 `expired_access_token` | refresh the token (30.5), retry once |
| 401 otherwise, or a refresh answered `invalid_grant` (the user disconnected the app) | `RemoteError::Auth`, so the host gets `ES_EV_AUTH_REQUIRED` and asks the user to sign in again |
| 429 (including `too_many_write_operations`) and 503 | wait for `Retry-After` (at most 60 s) and retry, at most 3 times, then `RemoteError::Network` |
| other 5xx, timeouts, connection errors | `RemoteError::Network` after the S3 backend's retries |
| 400 | `RemoteError::Http`: a bug in the request, never retried |

**Cost of a cycle.** Like S3: one listing of `devices/`, one listing of each other device's
`log/`, one download per new segment, one upload per new blob plus the segment. A request takes
roughly 0.2 to 0.5 s, more than S3. A card's first upload (a full PS2 card is a few dozen saves)
takes 10 to 20 s, once; a saved game later is 2 to 4 requests.

### 30.4 Sign-in

OAuth 2 authorization code flow with PKCE (`code_challenge_method=S256`) and
`token_access_type=offline`. That returns a long-lived refresh token and an access token that
expires after about 4 hours. Neither the token exchange nor a refresh needs a client secret.

- **Desktop** (the CLI, PCSX2, Dolphin, PPSSPP): the browser is sent to
  `http://127.0.0.1:<port>/dropbox`, with the port fixed when the app is registered. Dropbox
  matches redirect URIs exactly, port included, so a random port isn't possible. A small
  listener receives the redirect and answers with "Signed in, you can close this tab". If the
  port is taken, or there is no browser (SSH, the Steam Deck in game mode), the flow runs with
  no redirect URI: Dropbox shows the code and the user pastes it.
- **Android:** a Custom Tab, with the redirect to a custom scheme. Dropbox allows a non-HTTPS
  scheme with PKCE. The scheme is per host app, `<applicationId>.emusync://dropbox`, because two
  emulators that both claimed one shared scheme would make Android ask which app should open the
  redirect. Each host's scheme is registered in the App Console, so a new Android host asks the
  developers to add it. A fork that can't wait uses the paste-the-code flow or its own app key.
- **What sign-in returns:** the refresh token, which the host keeps (30.6), and the account's
  id and email address, for the settings screen ("Signed in as alice@example.com") and the data
  folder name.

### 30.5 Credentials and the C ABI

The library still never shows UI or stores secrets (§7). The host stores the refresh token, as
it stores S3 keys today, and supplies it through the credential provider. Additions to ABI v1,
all additive:

- **`ES_CRED_OAUTH_REFRESH`**, a new `es_credential_kind`: the host puts the refresh token in
  `bearer_token`. The library exchanges it for an access token, keeps that in memory only, and
  exchanges again 5 minutes before expiry or after a 401, the same schedule the provider follows
  for STS credentials. Most hosts use this kind: they store one string.
- **`ES_CRED_BEARER`** (reserved since v1) becomes live: an access token in `bearer_token` with
  its `expires_unix_ms`, for a host that runs OAuth itself. The library asks for a new one 5
  minutes before expiry or after a 401.
- **In `emusync-remote`,** `CredentialProvider` stops being S3-specific: it returns a
  `Credentials` enum (`S3`, `Bearer`, `OAuthRefresh`), and each backend refuses the kinds it
  can't use, as `creds.rs` already refuses non-S3 kinds. The Dropbox backend wraps the provider
  in a token cache that does the exchange.
- **The OAuth helpers.** The `es_oauth_begin` and `es_oauth_complete` reserved in v1 keep the
  PKCE verifier inside a client, which Android loses if it kills the app while the browser is
  open, and have no way to hand back the refresh token. They stay as they are (always
  `ES_E_INVALID_ARG`), and three functions that need no client replace them:

```c
/* Starts a sign-in to provider ("dropbox") with redirect_uri ("" for paste-the-code). Writes the
   URL to open, and `pending`: an opaque string (PKCE verifier, state, redirect URI) the host
   keeps until es_oauth_finish, across a process restart if it must. No network. */
ES_API es_status es_oauth_start(const char* provider, const char* redirect_uri,
                                char* out_url, size_t url_cap, char* out_pending, size_t pending_cap);

/* Desktop: listens on the loopback address and port of pending's redirect URI until the browser
   arrives or timeout_ms passes, and writes the redirect it received. ES_E_IO if the port is
   taken (then start again with redirect_uri ""), ES_TIMEOUT. */
ES_API es_status es_oauth_wait_redirect(const char* pending, uint32_t timeout_ms,
                                        char* out_redirect, size_t cap);

/* Exchanges the redirect (or the code the user pasted) for tokens; blocks up to 10 s. Writes the
   refresh token for the host to store, and the account ("dbid:…\talice@example.com").
   ES_E_AUTH if the user declined or the state doesn't match, ES_OFFLINE. */
ES_API es_status es_oauth_finish(const char* provider, const char* pending, const char* redirect_or_code,
                                 char* out_token, size_t token_cap, char* out_account, size_t account_cap);
```

- **Sign-out** deletes the stored token and calls `auth/token/revoke`. A token the user revoked
  from Dropbox's settings makes the next refresh fail with `invalid_grant`, which reaches the
  host as `ES_EV_AUTH_REQUIRED` (30.3).

### 30.6 Hosts

**CLI (`emusyncd`):**

```sh
emusync remote add dropbox                 # dropbox:/saves; opens the browser
emusync remote add dropbox:/saves-encrypted --encrypt
emusync remote add dropbox --no-browser    # prints the URL, asks for the code
emusync remote login                       # sign in again, e.g. after disconnecting the app
emusync remote logout                      # revoke the token and delete it from this device
```

- The refresh token goes into the system's credential store (§24): service `emusync`, name
  `dropbox-refresh-token`, with the usual fallback file on Linux without a Secret Service. That
  is what `emusync watch` running as a service reads. `EMUSYNC_DROPBOX_TOKEN` overrides it for
  scripts and `watch --env-file`, as the `AWS_*` variables do for S3.
- `emusync status` shows the account: `remote: dropbox:/saves (alice@example.com)`.

**Desktop emulators** (the PCSX2, Dolphin and PPSSPP forks): a "Sign in with Dropbox" button on
the sync settings page that calls the three `es_oauth_*` functions on a background thread and
opens the URL with the toolkit (`QDesktopServices::openUrl` in Qt). The token is kept with
`es_secret_set(<host>, "dropbox-refresh-token", …)`. The credential provider returns it as
`ES_CRED_OAUTH_REFRESH`.

**Android (the AAR, §26):**

- `SyncSettings` gains a storage kind (S3 or Dropbox). For Dropbox it keeps the refresh token
  sealed with the Keystore key, as it keeps the S3 secret, and the account's email address in
  plain preferences. `isComplete` asks for a token instead of keys.
- `DropboxSignIn`, new in the AAR: builds the PKCE request, opens a Custom Tab
  (`androidx.browser`), receives the redirect in an activity the AAR's manifest declares with
  the `${applicationId}.emusync` scheme, and exchanges the code. It is written in Kotlin
  because the AAR has no JNI of its own (§26.1). Calling the C functions would make every host
  add JNI wrappers, for about a hundred lines of Kotlin. The verifier is saved in preferences
  while the tab is open, so a process restart doesn't lose the sign-in.
- `SyncConfig` gains `refreshToken` (hidden in `toString`), and S3's fields become optional.
  The host's JNI credential provider returns `ES_CRED_OAUTH_REFRESH`.
- The data folder is named from the URI and the account id (30.2).

### 30.7 Later

- **Batched uploads.** `too_many_write_operations` comes from concurrent writes to one Dropbox
  namespace, which every device of a user shares. Uploading each blob into an upload session and
  committing them with one `files/upload_session/finish_batch_v2` (up to 1000 files) takes the
  namespace lock once instead of once per file. Needs `Remote::put_many`, with a default that
  loops over `put`, called by the push for a cycle's blobs.
- **One listing per cycle.** A recursive `files/list_folder` of `devices/` returns a cursor;
  later, `files/list_folder/continue` returns only what changed since. The backend could refresh
  an in-memory index of `devices/` in `list_dirs("devices/")`, which starts every pull, and
  answer that cycle's `list` calls from it. That makes a pull's listing one request, however
  many devices there are.
- **Change notifications.** `files/list_folder/longpoll` waits, up to 8 minutes, until something
  changes under a cursor. `emusync watch`, and an Android app on screen, could pull when another
  device pushes instead of polling (§6.1's back-off timer). Needs a `Remote::wait_for_change`
  that backends without it don't implement.

### 30.8 Tests

- **Unit tests:** request building (headers, `Dropbox-API-Arg` escaping), the error table,
  listing pagination with sorting and `start_after`, the token cache (refresh before expiry,
  after a 401, a single exchange when threads ask at once), `Retry-After`.
- **A fake Dropbox:** a small HTTP server in the test crate that implements the six endpoints
  with `strict_conflict`, case-folding paths and injected 429s, 409s and expired tokens. The
  conformance suite (§13) runs against it in CI on every change.
- **Real Dropbox, nightly:** the conformance suite against a test account, its refresh token a
  CI secret. The suite works in its own random namespace, which is deleted afterwards.
- **Hosts:** CLI sign-in in both flows, by hand. On Android, an instrumented test of the
  redirect activity and the token storage, and two-way sync between desktop PCSX2 and ARMSX2 on
  the AYN Thor, as for S3.

### 30.9 Open questions

1. **App name.** Dropbox app names are global, so `emusync` may be taken. The name is also the
   folder users see (`/Apps/<name>`).
2. **Encryption by default.** §7 says encryption is on by default for third-party storage. A
   passphrase is the one thing a non-technical user can lose, and Dropbox already encrypts at
   rest. Should a Dropbox repository be encrypted by default, or only offered?
3. **The visible folder.** Users with the Dropbox desktop app get `/Apps/emusync` synced to
   their computers: many small files, harmless but noticeable. The docs could suggest excluding
   it with selective sync. Deleting files in it loses history, and possibly saves; a README file
   at the app folder's root, outside every repository folder, could say not to edit it.
4. **Interoperating with a folder backend.** A desktop could sync `~/Dropbox/Apps/emusync/saves`
   through Dropbox's own app with a future `folder://` backend, while a phone uses the API. The
   layout allows it, if the folder backend tolerates files that are still arriving.

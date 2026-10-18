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
| PS1 | `.mcd`/`.mcr`/`.srm` raw 128 KB; `.gme`/`.vgs`/`.vmp` headered | Save block chain (1–15 blocks) | Serial in the block name |
| GameCube | `.raw` card image; Dolphin GCI folder | GCI file | Game code + maker code |
| Cartridge systems | `.srm`, `.sav`, `.eep`, `.fla` (RetroArch, mGBA, …) | The whole file, normalised | ROM hash (No-Intro SHA-1/CRC) |
| PSP | `SAVEDATA/<dir>` | Directory | Directory prefix (`ULUS10041`) |
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
  the GCI `gamecode+filename`.
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

- `ps1.card`: raw 128 KB images plus headered variants. Units are block chains. Apply rebuilds
  the directory frames and link fields.
- `gc.card` / `gc.gci`: Dolphin raw images and GCI folders (v2).
- `sram`: normalises cartridge save files between emulators (v2, with a quirk table for each
  emulator).
- `dir`: any directory tree as one unit (PSP `SAVEDATA/*`, Citra/Azahar and others).
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
    device.json                   name, platform, emulator, last-seen
    log/<seq:016x>.ndjson         append-only log segments; each segment is immutable once written
    compact/<seq:016x>.json       optional snapshot of everything this device has published up to seq
```

Log records (one JSON object per line):

```json
{"op":"card","card":"ps2:Mcd001","mode":"units","hlc":"2026-10-12T19:40:00.000Z/0000"}
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
| `PutObject` with `If-None-Match: *` | Every write: blobs, log segments, compaction snapshots, `device.json`. Writes are create-only. |
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
The protocol tolerates that, because every write is an append: an object that isn't visible
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

The ARMSX2 native core shares the PCSX2 hooks above. The Kotlin layer adds:

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
8. **Not yet implemented:** compaction snapshots and garbage collection (new devices replay
   full logs), `device.json` last-seen updates (it is written once), sessions and leases
   (`Engine::set_busy` routes results to the inbox instead), the size+mtime fingerprint
   shortcut (cards are re-hashed each scan), and the FFI beyond version/status functions.
9. **Platform details.** HTTP is `ureq` 2 with rustls/ring. On Windows the atomic replace is
   `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` (via `std::fs::rename`) rather than `ReplaceFileW`.
   Devices are discovered with one delimiter LIST of `devices/` per cycle. The conditional-write
   probe leaves a small `_probe/<uuid>` object.
10. **CLI.** S3 credentials come from `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` /
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
   and backup blocks are copied unchanged. Changed saves keep their position in the root
   directory; new ones are appended.
4. **ECC** is recomputed for every page written (three bytes per 128-byte chunk, then four
   zero bytes) and is not checked on read: pages that were never written have none, and
   emulators don't check it either.
5. **Unformatted cards.** PCSX2 creates new card files with every byte `0xFF` and leaves
   formatting to the PS2 BIOS (the cards on the development machine were in this state). An
   image like that, of a standard size, is an empty card. The first apply formats it with
   PCSX2's folder-card layout (8 MB: 41 reserved clusters, 8,135 allocatable; card flags
   `0x2B`), stamped with the first incoming save's creation time. This is what lets a fresh
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
   busy whenever any watched emulator runs. Busy cards are still snapshotted and uploaded; only
   applies wait in the inbox. Process lists come from `sysinfo` every 2 s, behind a
   `ProcessProbe` trait so tests use a fake.
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

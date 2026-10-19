# ARMSX2 integration research

Research for milestone M2 ([DESIGN.md §10.2](../DESIGN.md), open questions 2 and 3 in §15): how
emusync would be linked into the ARMSX2 Android app, and where its calls from
[`include/emusync.h`](../../include/emusync.h) would go.

**Method.** I cloned the official repository and read the source. I did not build it, run it, or
test it on a device. Each finding is tagged:

- **confirmed**: read in the source. The file and line are cited, and every link is a permalink
  at the commit below.
- **inferred**: follows from the source but was not observed directly. Each of these says what
  would settle it.

I compared against upstream PCSX2 by diffing against the merge base
(`36907f4b70d149b05c2700acae687bedb720a7af`, 2026-09-27) with a local PCSX2 clone, not by
running anything.

## 0. The repository

| | |
|---|---|
| Official repository | <https://github.com/ARMSX2/ARMSX2> |
| Commit read | [`e7235739936c5ad8d09af20bf0fd382624caff8b`](https://github.com/ARMSX2/ARMSX2/commit/e7235739936c5ad8d09af20bf0fd382624caff8b), `master`, 2026-10-18, "Merge pull request #852 from loki666/ci-release-linux-appimage" |
| Release at the time | [2.8.4](https://github.com/ARMSX2/ARMSX2/releases/tag/2.8.4) (2026-10-17), plus nightlies (`nightly-20261018`) |
| License | GPL-3.0 (`COPYING.GPLv3`; source files carry `SPDX-License-Identifier: GPL-3.0+`) |

**Why this is the project's own repository, not a mirror** (**confirmed** via the GitHub API and
the project website):

- It lives in the `ARMSX2` organisation. The organisation's `blog` field and the repository's
  `homepage` are both <https://armsx2.net>, and the "Source Code" button on armsx2.net links to
  `github.com/ARMSX2`.
- It is a GitHub fork of `PCSX2/pcsx2` (created 2025-08-11, 2.2k stars, pushed to the day before
  I read it) with 4,372 commits ahead and 66 behind upstream. It is a diverged working fork, not
  a read-only copy.
- A web search for "ARMSX2 github" returns `kitakatashima/ARMSX2` first. That repository is a
  fork *of* `ARMSX2/ARMSX2` (parent `ARMSX2/ARMSX2`), so it is not the official source. Sibling
  repositories in the organisation (`ARMSX3`, `ARMSX1`, `ARMSX2-NATIVE-TOOLS`) are other projects.

Pin `ARMSX2/ARMSX2` in any integration notes.

**What the repository is.** The README calls it a "Native ARM64 JIT Fork of PCSX2" that targets
Android, iOS, macOS, Windows and Linux from one shared core. The Android app is in
`platforms/android`. An iOS app is in `platforms/ios`, and there are also Qt, SDL and libretro
front ends. One core with five front ends is useful for emusync: the memory-card hooks in
section 7 belong in the shared core and would serve all of them.

Paths below are relative to the repository root, and every link is a permalink at that commit.

---

## 1. Memory card storage on Android

### Summary

| Question | Answer |
|---|---|
| Default location | `<DataRoot>/memcards/`, where DataRoot defaults to `Context.getExternalFilesDir(null)`, i.e. `/storage/emulated/0/Android/data/<applicationId>/files/memcards/`. The stable `applicationId` is `com.armsx2`; nightlies use a different one. |
| User-selectable folder? | Yes, but the selection is the **data root** (memory cards, save states, settings together), not a per-card folder. |
| Storage Access Framework? | **No, not for memory cards.** The native core is handed a plain POSIX path, and the card code does ordinary `fopen`/`fwrite`. SAF is used for ROMs, BIOS import and card import/export only. |
| File cards (`.ps2`) | Yes. |
| Folder cards | Yes. |
| Per-game cards | Yes, as an app-level slot-1 swap to `<serial>.ps2`, off by default. |
| Default card names | See below. The Kotlin defaults are lowercase `mcd001.ps2` and `mcd002.ps2`; the core's own default is `Mcd001.ps2`. |

### Details

**Data root choices** (**confirmed**).

The setup wizard offers up to three data-root choices
([`OnboardingScreen.kt:399-430`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/src/main/java/com/armsx2/ui/onboarding/OnboardingScreen.kt#L399-L430),
[`OnboardingViewModel.kt:68-110`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/src/main/java/com/armsx2/ui/onboarding/OnboardingViewModel.kt#L68-L110)):

1. **Internal / app-private**: `getExternalFilesDir(null)`. The default.
2. **SD card**: the app-specific directory on a secondary volume (`sdCardDataDir`,
   [`MainActivityRuntime.kt:338`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/src/main/java/com/armsx2/runtime/MainActivityRuntime.kt#L338)).
3. **Custom folder**: `BuildConfig.STORAGE_ALL_FILES` builds only (the `github` flavour). It
   needs `MANAGE_EXTERNAL_STORAGE`, which only the sideloaded APK declares
   ([`src/github/AndroidManifest.xml`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/src/github/AndroidManifest.xml);
   [`app/build.gradle.kts:253-285`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/build.gradle.kts#L253-L285)). The
   Play build (`play` flavour) deliberately omits it.

The chosen value is stored as a POSIX path. A legacy preference could still hold a SAF tree URI
(`content://com.android.externalstorage.documents/tree/primary%3A…`), which
`resolveTreeUriToPosix` ([line 369](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/src/main/java/com/armsx2/runtime/MainActivityRuntime.kt#L369))
converts to `/storage/emulated/0/<rel>` or `/storage/<volumeId>/<rel>`.
`resolveDataRoot` ([line 1985](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/src/main/java/com/armsx2/runtime/MainActivityRuntime.kt#L1985))
falls back to app-private storage if such a path is not writable. The result goes to native code
as `EmuFolders::DataRoot` in `NativeApp.initialize`
([`native-lib.cpp:288-300`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/src/main/cpp/native-lib.cpp#L288-L300)).

**Memory card folder** (**confirmed**).

- The core builds each card path as `EmuFolders::MemoryCards` + the slot's filename
  ([`Pcsx2Config.cpp:2328`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/Pcsx2Config.cpp#L2328), `FullpathToMcd`). By default
  `EmuFolders::MemoryCards` is `<DataRoot>/memcards`.
- The Kotlin side uses the same location: `cardsDir()` is `File(assetCopyRoot(ctx), "memcards")`
  ([`MemoryCardBackup.kt:137`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/src/main/java/com/armsx2/MemoryCardBackup.kt#L137),
  [`MemoryCardViewModel.kt:363`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/src/main/java/com/armsx2/ui/memorycards/MemoryCardViewModel.kt#L363)).
- Backups go in a sibling `memcard-backups/` folder, deliberately outside `memcards/`.

**Why SAF is not used for cards** (**confirmed**). Three pieces of evidence:

- The only `content://` support in the native file layer opens read-only. `NativeApp.openContentUri`
  calls `openFileDescriptor(uri, "r")`
  ([`NativeApp.java:1181-1193`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/src/main/java/kr/co/iefriends/pcsx2/NativeApp.java#L1181-L1193)),
  and `FileSystem::OpenCFile` routes `content://` through it
  ([`common/FileSystem.cpp:1003-1030`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/common/FileSystem.cpp#L1003-L1030)). It exists for
  disc images. A memory card cannot be opened for writing this way.
- `HostVFS` ([`common/HostVFS.h`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/common/HostVFS.h)) is a pluggable file system that could
  carry SAF. Only the libretro core installs one (`pcsx2-libretro/Main.cpp:1636`). The Android
  app never calls `HostVFS::Install`.
- `resolveTreeUriToPosix`'s own comment says "emucore's POSIX FileSystem APIs require the
  resolved path to be writable without broad shared-storage privileges"
  ([line 282](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/src/main/java/com/armsx2/runtime/MainActivityRuntime.kt#L282)).

**The answer to DESIGN §15 question 2.** ARMSX2 on Android keeps cards in app-specific storage
by default, or in a raw POSIX folder if the user picked one. It does **not** use SAF tree URIs
for them. The `es_vfs` callback path in `emusync.h` is therefore **not needed for the ARMSX2 M2
integration**, which can pass native paths as `location`. It would matter only for the libretro
build, or if ARMSX2 later moves cards to SAF.

**One catch for the custom-folder case** (**confirmed that the problem exists in the core,
inferred that it affects emusync**). The core works around Android's FUSE-emulated shared
storage, where a plain libc `fopen` that *creates* a file, and a plain `mkdir`, can be denied
even for an all-files-access app:

- `FileSystem::CreateDirectoryViaJava` and `CreateFileViaJava`
  ([`native-lib.cpp:3660`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/src/main/cpp/native-lib.cpp#L3660),
  [`3700`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/src/main/cpp/native-lib.cpp#L3700)) exist for this reason.
- `NativeApp.java:1195-1200` says "Fallback directory creation for native
  FileSystem::CreateDirectoryPath … raw libc mkdir() can be denied".
- `MemoryCardFolder.cpp:57-68` carries a comment about the same denial.

emusync's journalled replace writes a sibling temp file and renames it (`es_vfs.sibling` in the
header). If it runs on such a folder, creating that temp file with `open(O_CREAT)` may fail. It
would not fail in the default `Android/data/<pkg>/files` location, which the app owns. Test on a
device with a custom data folder before M2 sign-off. If it fails, emusync needs an in-place
write fallback or an `es_vfs` whose `open`/`mkdir` use the Java File API.

**Default card names** (**confirmed**).

- Core default: `FileMcd_GetDefaultName` returns `Mcd%03u.ps2`, so `Mcd001.ps2` and
  `Mcd002.ps2`, with `Mcd-Multitap%u-Slot%02u.ps2` for multitap slots
  ([`MemoryCardFile.cpp:265-270`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/SIO/Memcard/MemoryCardFile.cpp#L265-L270)). Used when
  no setting exists (`Pcsx2Config.cpp:2195`).
- Android Kotlin default: `memoryCardSlot1Filename = "mcd001.ps2"` and
  `memoryCardSlot2Filename = "mcd002.ps2"`, **lowercase**
  ([`Settings.kt:664-672`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/src/main/java/com/armsx2/config/Settings.kt#L664-L672)),
  written to `[MemoryCards] Slot1_Filename` and `Slot2_Filename` whenever the config is saved
  (`Settings.kt:1088-1091`). The per-game logic compares against `"mcd001.ps2"` with
  `ignoreCase = true` ([`MainActivityRuntime.kt:1005`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/src/main/java/com/armsx2/runtime/MainActivityRuntime.kt#L1005)),
  which suggests both spellings occur.
- **Inferred:** a fresh Android install ends up with lowercase `mcd001.ps2`, because the Kotlin
  layer pushes its defaults to the core at boot. Look at a freshly installed device's
  `memcards/` folder to settle it.

**Consequence for emusync.** Android's file system is case-sensitive, so `mcd001.ps2` and
`Mcd001.ps2` are different files. If emusync takes the logical card name from the file stem
(the header default when `card` is NULL), a phone would sync a card named `mcd001` and the
desktop a card named `Mcd001`, which would never meet. **Set `es_container_desc.card` explicitly**
to `"Mcd001"` or `"Mcd002"` from the slot number, not from the filename. DESIGN §2.1 and the
`card` comment in the header already anticipate "stems differ between emulators"; this is the
concrete case. It also affects the DESIGN §10.2 sketch, which says "Mcd001.ps2 / Mcd002.ps2 bind
to logical cards Mcd001 / Mcd002". On Android that binding needs the explicit `card`.

**File, folder and per-game cards** (**confirmed**).

- **File cards.** The card list accepts `.ps2`, `.mcr`, `.mcd`, `.bin`, `.mc2` and
  `.conquestcard` ([`MemoryCardFile.cpp:994-997`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/SIO/Memcard/MemoryCardFile.cpp#L994-L997)).
  `FileMcd_CreateNewCard` can make 8, 16, 32 and 64 MB cards and PS1 cards. `FileMcd_SetType`
  treats a path as a **folder card** if it is a directory
  ([`MemoryCardFile.cpp:646-665`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/SIO/Memcard/MemoryCardFile.cpp#L646-L665)).
- **Folder cards.** The Android card manager lists directories as folder cards, creates them
  with a `_pcsx2_superblock` marker, and imports them from a folder or zip
  ([`MemoryCardViewModel.kt:31-175`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/src/main/java/com/armsx2/ui/memorycards/MemoryCardViewModel.kt#L31-L175)).
- **"Per-game" has two meanings here.**
  1. *Per-game assignment.* The user pins any named card to a slot for one game, stored as a
     per-game settings override (`assignToGame`, `MemoryCardViewModel.kt:216`). The Mcd001/Mcd002
     slot defaults still apply to other games.
  2. *Serial-named card.* With the preference `memcard.perGame` on (default off) and slot 1 still
     on the factory-default card, the app sets slot 1 to `<serial>.ps2`, for example
     `SLUS-20062.ps2`. The core creates and formats it at boot
     ([`MainActivityRuntime.kt:998-1013`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/src/main/java/com/armsx2/runtime/MainActivityRuntime.kt#L998-L1013)).
     This is the NetherSX2-style layout.
  - The core also has GameDB `memcardFilters` for folder-card save filtering
    (`VMManager.cpp:1411`, `memcardFiltersAsString()`).
  - **Mapping to emusync:** a serial-named card is a per-game container. Register it with
    `title_scope = <serial>` and `card = "Mcd001"`, the slot's shared logical card, as the header
    comment says. This answers DESIGN §15 question 4 for ARMSX2: it uses `<serial>.ps2`, in the
    same folder as the other cards.

---

## 2. How cards are opened and written

### Is it PCSX2's `FileMemoryCard` and `FolderMemoryCard` unchanged?

**Confirmed: nearly, with a small number of additive changes.** I diffed
`pcsx2/SIO/Memcard/*` against the merge base
(`36907f4b70d149b05c2700acae687bedb720a7af`). The real ARMSX2 changes are:

| Change | Where | Relevance to emusync |
|---|---|---|
| `FileMemoryCard::Flush()` → `std::fflush` on every open file card. Exposed as `FileMcd_Flush()` and `FolderMemoryCardAggregator::Flush()`. | [`MemoryCardFile.cpp:371-386`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/SIO/Memcard/MemoryCardFile.cpp#L371-L386), [`689-695`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/SIO/Memcard/MemoryCardFile.cpp#L689-L695), [`MemoryCardFolder.cpp:1078-1091`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/SIO/Memcard/MemoryCardFolder.cpp#L1078-L1091), [`2331`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/SIO/Memcard/MemoryCardFolder.cpp#L2331) | **Useful.** This is a ready-made "bank everything now" point for `es_container_flushed`. It exists because Android can kill a backgrounded process with no further callback. |
| `std::fflush(mcfp)` after every successful sector write. | [`MemoryCardFile.cpp:~547`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/SIO/Memcard/MemoryCardFile.cpp#L544-L554) | The file on disk is up to date after each write. emusync can read the file at any `touched` and see current data. |
| A failed read-back now `return 0` instead of merging into stale buffer content. | `FileMemoryCard::Save`, [`~503-516`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/SIO/Memcard/MemoryCardFile.cpp#L503-L516) | None. |
| `m_overwrite[]` replaces `m_ispsx[]`. It also treats Soul Calibur II's `.conquestcard` as overwrite-in-place with no checksum stamp. | `IsConquestCard`, [`MemoryCardFile.cpp:~156-166`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/SIO/Memcard/MemoryCardFile.cpp#L156-L166) | A new card type. emusync should treat it as `raw` or reject it, not as `ps2.card`. |
| `MemcardBackup::OnCardsReopening()` is called from `FileMcd_Reopen` between closing and reopening the cards. | [`MemoryCardFile.cpp:704-714`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/SIO/Memcard/MemoryCardFile.cpp#L704-L714) | **Precedent and a good seam.** See section 7. |
| `IOP_SIDE_ASSERT()` in `FileMcd_Reopen`, `FileMcd_CancelEject`, `FileMcd_Swap`. | same file | Cards are touched from the **IOP thread** when the IOP runs on its own thread (ARMSX2's `IopThread`). emusync's `touched` and `flushed` are documented thread-safe, which is what is needed. |
| `FolderMemoryCard::SaveYAMLToFile` null-check for FUSE. | [`MemoryCardFolder.cpp:57-68`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/SIO/Memcard/MemoryCardFolder.cpp#L57-L68) | Same FUSE caveat as in section 1. |
| An arcade-dongle terminator tweak. | `MemoryCardProtocol.cpp` | None. |

ARMSX2 is also **behind** upstream in places, since it is 66 commits behind. The ECC-parity
fix, `FileMcd_InsertECC` and reserved-cluster rejection that appear in a current PCSX2 clone are
upstream commits from 2026-10-07 that ARMSX2 does not yet have. Treat differences in raw spare
bytes or ECC between cards written by ARMSX2 and by current PCSX2 as expected. This reinforces
the design rule that the `ps2.card` adapter works on logical content and recomputes ECC itself
(DESIGN §5.1).

New in ARMSX2: [`pcsx2/SIO/Memcard/MemcardBackup.{h,cpp}`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/SIO/Memcard/MemcardBackup.h)
and the Kotlin twin
[`MemoryCardBackup.kt`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/src/main/java/com/armsx2/MemoryCardBackup.kt).
Both snapshot the cards into zips before a boot (details in section 7).

### How card paths reach the native layer

**Confirmed.** Kotlin does not pass card paths to the core. It passes *settings*, and the core
derives the paths:

1. `NativeApp.setSetting("MemoryCards", "Slot1_Filename", "string", "<name>")` plus
   `commitSettings()` (`MemoryCardViewModel.assign`, [line 189](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/src/main/java/com/armsx2/ui/memorycards/MemoryCardViewModel.kt#L189)),
   or the saved config pushed at boot.
2. The core reads `Slot%u_Enable` and `Slot%u_Filename` in `Pcsx2Config::LoadSaveMemcards`
   ([`Pcsx2Config.cpp:2296-2310`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/Pcsx2Config.cpp#L2296-L2310)).
3. `EmuConfig.FullpathToMcd(slot)` is `EmuFolders::MemoryCards` + the filename. `FileMcd_SetType`
   picks File or Folder by whether the path is a directory.
4. `FileMemoryCard::Open` / `FolderMemoryCard::Open` open that POSIX path.

So the emusync glue gets its `location` from `EmuConfig.FullpathToMcd(slot)` after
`FileMcd_SetType()`, a native path with no URIs involved.

---

## 3. Lifecycle hooks

### Native (the PCSX2 equivalents)

All **confirmed**. The VM manager is upstream's, with the same structure:

| Upstream concept | ARMSX2 location |
|---|---|
| `VMManager::Initialize` | [`pcsx2/VMManager.cpp:1965`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/VMManager.cpp#L1965). `Host::OnVMStarting()` fires at line 1987. |
| Where the serial becomes known | `VMManager::UpdateDiscDetails(bool booting)` ([`1339`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/VMManager.cpp#L1339)), called from `Initialize` at [`2134`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/VMManager.cpp#L2134). It sets `s_disc_serial` (`cdvdGetDiscInfo` at ~1370; the BIOS serial when there is no disc; the ELF title for an ELF boot; the game ID for arcade), then calls `ReportGameChangeToHost()` ([1482](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/VMManager.cpp#L1482)) and **`FileMcd_Reopen(...)` at [1491](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/VMManager.cpp#L1491)**. |
| Card open | `FileMcd_Reopen(serial)` → `FileMcd_EmuClose()`, `FileMcd_SetType()`, `sioSetGameSerial()`, `MemcardBackup::OnCardsReopening()`, `FileMcd_EmuOpen()` ([`MemoryCardFile.cpp:704-714`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/SIO/Memcard/MemoryCardFile.cpp#L704-L714)). `FileMcd_EmuOpen` ([668](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/SIO/Memcard/MemoryCardFile.cpp#L668)) calls `impl.Open()`, `implFolder.SetFiltering(true)`, `implFolder.Open()`. |
| Boot failure cleanup | `ScopedGuard close_memcards(&FileMcd_EmuClose)` at [`VMManager.cpp:2136`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/VMManager.cpp#L2136). |
| Card writes | `FileMcd_Save(port, slot, src, adr, size)` ([`MemoryCardFile.cpp:834`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/SIO/Memcard/MemoryCardFile.cpp#L834)) dispatches to `FileMemoryCard::Save` ([482](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/SIO/Memcard/MemoryCardFile.cpp#L482)) or `FolderMemoryCardAggregator::Save` ([2364](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/SIO/Memcard/MemoryCardFolder.cpp#L2364)). This is the single choke point for every guest write. |
| Folder-card flush | `FolderMemoryCard::NextFrame` (1070) counts down; `Flush()` (1093) writes. The countdown advances only on vsync, so a paused VM never reaches it, which is why ARMSX2 added `FlushNow`. |
| Flush on demand | `FileMcd_Flush()` ([689](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/SIO/Memcard/MemoryCardFile.cpp#L689)). New in ARMSX2, called only from the Android and iOS front ends. |
| Close | `FileMcd_EmuClose()` ([680](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/SIO/Memcard/MemoryCardFile.cpp#L680)) calls `implFolder.Close()` then `impl.Close()`. Called from `VMManager::Shutdown` at [`VMManager.cpp:2411`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/VMManager.cpp#L2411). |
| `VMManager::Shutdown` | [`2340`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/VMManager.cpp#L2340). `SaveSessionTime(s_disc_serial)` at 2376 is the existing play-time bookkeeping; `Host::OnVMDestroyed()` at 2444. |
| Mid-game reopen | `UpdateEmuFolders` ([1166](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/VMManager.cpp#L1166)) when the memory-card folder changes, `CheckForMemoryCardConfigChanges` ([3931](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/VMManager.cpp#L3931)) when slot settings change, and `UpdateDiscDetails(false)` ([3151, 3161, 3181](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/VMManager.cpp#L3151)) on disc swap. All of these close and reopen the cards under a running VM. |

**The game serial is known before the cards open: yes** (**confirmed**). `UpdateDiscDetails`
resolves the serial first, and `FileMcd_Reopen` runs a few lines later in the same function.
That function is also the first place the cards open, because `Initialize` itself does not call
`FileMcd_EmuOpen`. There is no earlier "open" to race.

**What "serial" means** (**confirmed**, from `VMManager.cpp:1355-1388`). It is the disc serial
for a disc boot. For a BIOS-only boot it is `BiosSerial`, so a sync-session hook should skip
those or treat them as titleless. For an ELF boot it is the ELF's file title. For an arcade game
it is the `.acgame` game ID.

### Android thread and call structure

**Confirmed.**

- A game starts in Kotlin on a dedicated single-thread executor (`eDispatcher`, not the UI
  thread). It calls the blocking `NativeApp.runVMThread()` for the whole session
  ([`MainActivityRuntime.kt:854`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/src/main/java/com/armsx2/runtime/MainActivityRuntime.kt#L854),
  [`native-lib.cpp:3741`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/src/main/cpp/native-lib.cpp#L3741)). That
  function calls `VMManager::Initialize`, runs the Execute/Paused loop, then
  `VMManager::Shutdown(false)`.
- Just before `runVMThread`, Kotlin calls `MemoryCardBackup.snapshotActiveCards(...)` with the
  game's `settingsKey` as the serial (`@@ANDROID_MCDBAK@@`,
  [line 842](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/src/main/java/com/armsx2/runtime/MainActivityRuntime.kt#L842)).
  Kotlin therefore knows the serial even earlier than the core does, from the game library.
  Prefer the core's value, which comes from the disc itself.
- The native CPU thread is the `runVMThread` thread (`s_cpu_thread_id`). The IOP,
  GS and VU threads are separate.

### Activity and Service lifecycle

**Confirmed** from `AndroidManifest.xml` and `MainActivityRuntime.kt`.

- The live activity is `com.armsx2.Main`, which is `MainActivityRuntime`
  ([`Main.kt`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/src/main/java/com/armsx2/Main.kt)).
  `kr.co.iefriends.pcsx2.MainActivity.java` is a legacy class that survives as `.MainActivity`
  only via an `activity-alias` pointing at `.Main`
  ([`AndroidManifest.xml:129-140`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/src/main/AndroidManifest.xml#L129-L140)).
  Its `onPause`/`onDestroy` are not used.
- **There is no Service for emulation.** The only services are in the `:discord` process, and
  the manifest removes `FOREGROUND_SERVICE*` permissions. The VM runs on a thread in the main
  process, so Android may kill it freely when the app is in the background.
- `onPause` (5954): saves state of the UI, then, if a game is running, `InGameOverlay.open()`
  (which pauses the VM and shows the pause menu), then `NativeApp.flushShaderCache()`.
- `onStart` / `onStop` (6000 / 6006): `NativeApp.setAppBackgrounded(false/true)`, which lets
  native code pause the audio device for real while out of sight
  ([`native-lib.cpp:4023`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/src/main/cpp/native-lib.cpp#L4023)).
- `onDestroy` (6051): if not a configuration change, calls `NativeApp.shutdown()` and then
  **`Process.killProcess(myPid())`**.
- `NativeApp.pause()` ([`native-lib.cpp:3930`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/src/main/cpp/native-lib.cpp#L3930)):
  queues a CPU-thread task that calls `VMManager::SetPaused(true)`, `cdvdSaveNVRAM()`, then
  **`FileMcd_Flush()`** and, for arcade, `ACSRAM::WriteFile()`. The comment explains the
  rationale: Android can reclaim the process while backgrounded with no further callback. The
  already-paused branch (3965-3985) flushes too. `NativeApp.resume()` (3995) unpauses.
- `NativeApp.shutdown()` ([`native-lib.cpp:4119`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/src/main/cpp/native-lib.cpp#L4119)):
  latches a stop request, requests VM shutdown on the CPU thread, then **waits up to 5 s** for
  `VMState::Shutdown`. The Kotlin `stop()` runs it on a `VMStop` executor, optionally after an
  autosave state (`autoSaveOnExit`, [line 1462](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/src/main/java/com/armsx2/runtime/MainActivityRuntime.kt#L1462)).

**What this means for emusync.** From leaving the app to being killed, the app has only the
`onPause`/`onStop` window; after that the process can die at any time, and on `onDestroy` it is
killed explicitly. This matches the header's `es_app_suspend` ("everything reaches the on-disk
outbox first, then a best-effort push"). `NativeApp.shutdown` and the 2 s thread join in
`onDestroy` put a hard ceiling on any blocking `es_session_end`. See section 7.

---

## 4. Build

### Layout (**confirmed**)

- **Gradle.** `platforms/android/` is a standalone Gradle project with one module, `:app`. The
  Android Gradle Plugin is 9.2.1, Gradle 9.4.1, Kotlin 2.4.0 with Compose, `compileSdk = 37`,
  `targetSdk = 37`, `minSdk = 26`
  ([`libs.versions.toml`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/gradle/libs.versions.toml),
  [`gradle-wrapper.properties`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/gradle/wrapper/gradle-wrapper.properties),
  [`app/build.gradle.kts:119-126`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/build.gradle.kts#L119-L126)).
- **Native.** `externalNativeBuild { cmake { path = src/main/cpp/CMakeLists.txt; version = "3.31.6" } }`
  ([`build.gradle.kts:306-311`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/build.gradle.kts#L306-L311)).
  That file does `add_subdirectory` of the repository's top-level `common/`, `pcsx2/` and
  `3rdparty/libretro`, then adds `3rdparty/adrenotools`, `miniupnpc`, `librashader` and the
  `flurry` and `savers` libraries
  ([`cpp/CMakeLists.txt`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/src/main/cpp/CMakeLists.txt)). A stale comment
  at `build.gradle.kts:302` says the native build is disabled; the `cmake` block below it is
  live.
- **NDK and ABIs.** NDK is pinned to `29.0.14206865` (`armsx2.ndkVersion`, line 26). **Only
  `arm64-v8a`** (`abiFilters`, line 135). `ANDROID_STL=c++_static`.
- **Library names.** One shared library, `emucore_4k` by default
  (`ARMSX2_EMUCORE_LIBRARY_NAME`, `CMakeLists.txt:311-317`), loaded by
  `System.loadLibrary` in `NativeApp.java:26-35`. Release APKs ship **two builds of the core**,
  4 KiB- and 16 KiB-page, in one universal APK
  ([`README.md`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/README.md),
  [`tools/build-universal-page-apk.sh`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/tools/build-universal-page-apk.sh)).
  The 16 KiB page size is a compile-time define, `OVERRIDE_HOST_PAGE_SIZE`, and all ELFs are
  linked with `-z max-page-size=16384`
  ([`cmake/BuildParameters.cmake:183-200`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/cmake/BuildParameters.cmake#L183-L200)).
  Only one core is loaded per process.
- **JNI.** All JNI functions are in `native-lib.cpp` (6,925 lines) as
  `Java_kr_co_iefriends_pcsx2_NativeApp_*`, declared in `NativeApp.java` (about 200 `native`
  methods).
- **Gradle `packaging`.** `useLegacyPackaging = true` so extracted `.so` files exist on disk
  ([line 327](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/build.gradle.kts#L327)).

### Third-party native dependencies as precedent

**Confirmed.** Most are built as static libraries by CMake and linked into `emucore`. The first
two are the relevant precedents:

| Dependency | How | Notes |
|---|---|---|
| **librashader (Rust)** | Fetched at configure time from a pinned commit, then `cargo build --release --target aarch64-linux-android` runs from a CMake `add_custom_command`. The output is a **cdylib**, imported as `add_library(librashader SHARED IMPORTED)`, copied to `CMAKE_LIBRARY_OUTPUT_DIRECTORY` for AGP to package. | [`3rdparty/librashader/CMakeLists.txt`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/src/main/cpp/3rdparty/librashader/CMakeLists.txt). **Optional**: no `cargo` means `ARMSX2_HAVE_LIBRASHADER=OFF` and the feature compiles out. This is a direct template. |
| **Discord bridge** | A separate shared library that `emucore` never links, kept apart for license reasons and run in the `:discord` process. | `CMakeLists.txt:319-340`. Shows the maintainers' habit of isolating things with different license terms. |
| adrenotools | In-tree subdirectory, `useLegacyPackaging` for its hook `.so` files. | |
| cubeb, oboe, SDL3, shaderc, vixl, libzip, **zstd**, miniupnpc, freetype, libpng, … | Static, via `add_subdirectory` or `find_package`. | `Zstd::Zstd` is already linked into `emucore` (`CMakeLists.txt:351`). |

### Best way to link a cargo-ndk Rust library

I recommend an **optional `cdylib`**, `libemusync.so`, built by a CMake custom command exactly
the way librashader is built, and linked into `emucore` as an `IMPORTED` shared target. Each
point below is **inferred** unless it cites the code.

1. **Rust target and toolchain.** `aarch64-linux-android` only, API level 26, the NDK's own
   clang as linker. librashader's CMake passes the right `CARGO_TARGET_*_LINKER`, `CC_*` and
   `AR_*` environment, so `cargo-ndk` is not required. `cargo-ndk` would work in CI as an
   alternative; either way the inputs are the same.
2. **Why `cdylib` rather than `staticlib`.** `emusync-ffi` currently builds `rlib` and
   `staticlib` ([`crates/emusync-ffi/Cargo.toml`](../../crates/emusync-ffi/Cargo.toml), crate name
   `emusync`). Linking `libemusync.a` into `emucore` has two problems:
   - **Duplicate C symbols.** emusync's dependencies include the `zstd` crate (`zstd-sys`
     compiles libzstd with its normal `ZSTD_*` names), `rusqlite` with `bundled` (SQLite) and
     `ring`. `emucore` already links its own zstd (`Zstd::Zstd`, `find_package(Zstd 1.5.5 REQUIRED)` in `cmake/SearchForStuff.cmake`; the repository carries `3rdparty/zstd`).
     Two copies of `ZSTD_*` in one link give duplicate-symbol errors, or worse, silently mixed
     versions.
   - **A huge C++ LTO link.** release builds use `-DLTO_PCSX2_CORE=ON`. librashader's CMake comment
     ([`3rdparty/librashader/CMakeLists.txt:117`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/src/main/cpp/3rdparty/librashader/CMakeLists.txt#L117))
     records the choice of cdylib specifically to keep the Rust objects out of `emucore`'s LTO
     link.
   A `cdylib` exports only the `#[no_mangle] extern "C"` symbols (`rustc` passes a version script
   that keeps everything else local), so the C dependencies stay private. Add `cdylib` to
   `crate-type` in the `emusync-ffi` crate. That change is not part of this research.
3. **Soname.** `rustc` does not emit a `SONAME` for an Android cdylib. Without one the linker
   records the build-tree path as `DT_NEEDED` and the library fails to load at runtime.
   librashader works around this with
   `RUSTFLAGS="-C link-arg=-Wl,-soname,liblibrashader_capi.so"`. Do the same with
   `-Wl,-soname,libemusync.so`, plus `-Wl,-z,max-page-size=16384`.
4. **Packaging.** AGP only packages `.so` files that CMake itself emits into the per-ABI output
   directory, so copy the cargo output there in a `POST_BUILD` step, as librashader does.
   Otherwise the core links and then dies at runtime with `UnsatisfiedLinkError`.
5. **No `libc++_shared.so` needed.** librashader needs it only because it pulls in C++
   dependencies (glslang, spirv-cross), which is why its CMake stages `libc++_shared.so`. emusync
   has only C dependencies (SQLite, zstd, ring), so it should link against plain libc, libm, libdl
   and liblog.
6. **Both core variants share one copy.** The 4K and 16K cores are separate `emucore` builds. A
   `libemusync.so` in the APK is shared by whichever core loads, and it needs no page-size
   define.
7. **Optional like librashader.** If `cargo` or the target is missing, leave
   `ARMSX2_HAVE_EMUSYNC=OFF` and compile the feature out, so a contributor without a Rust
   toolchain can still build. Define `ARMSX2_HAS_EMUSYNC` on **`PCSX2_FLAGS`**, not `PCSX2`.
   `CMakeLists.txt:383-396` explains why: release builds compile many sources into a separate
   `PCSX2_LTO` target that only sees `PCSX2_FLAGS`, so a define on `PCSX2` compiles cleanly and
   silently ships the feature stubbed out. `VMManager.cpp` and the `SIO/Memcard` files are core
   sources, so this applies to the call sites in section 7.
8. **Is a C++ wrapper needed?** No. `emusync.h` already has `extern "C"` guards, so
   `native-lib.cpp` and the core can call `es_*` directly. A thin ARMSX2-side glue file (for
   example `pcsx2/EmuSync/EmuSyncHost.{h,cpp}`) is still worth having, to keep `#ifdef
   ARMSX2_HAS_EMUSYNC` out of `VMManager.cpp` and to hold the container registry. The planned
   `bindings/cpp/emusync.hpp` (DESIGN §12) is optional convenience.
9. **A spike to settle the open points.** The soname handling, the symbol-hiding behaviour and
   the absence of `libc++_shared` are all inferred. A half-day spike would settle them: build
   `libemusync.so` with the librashader recipe, link it into `emucore`, and check
   `llvm-readelf -d` for `DT_NEEDED` and `llvm-nm -D` for exported symbols.

**Other build notes.**

- `ring` and `rusqlite`'s bundled SQLite compile C, so cargo needs `CC_aarch64_linux_android`
  pointing at the NDK clang. librashader's CMake already sets it.
- ARMSX2 builds in debug and release configurations; match `--release` to the CMake build type.
- Kotlin access: the design's Android AAR (DESIGN §9, §12 `bindings/android`) would add a second
  JNI layer. For ARMSX2 the simpler route is a few `NativeApp` methods in `native-lib.cpp`,
  because the C++ core owns the one `es_client` and the hooks. A second client created by an AAR
  would fight the first over the same SQLite database and leases. If a standalone AAR is still
  wanted for other apps, the AAR and `emucore` should both link the same `libemusync.so`.

---

## 5. Save states

### Version

**Confirmed.** ARMSX2 keeps upstream's value:

```cpp
// pcsx2/SaveState.h:29
static const u32 g_SaveVersion = (0x9A59 << 16) | 0x0000;
```

This is identical at the merge base (2026-09-27) and in current PCSX2 master. The file's own
notice ("When updating g_SaveVersion … [SAVEVERSION+]") is unchanged. Upstream bumped it on
2025-11-03; the ARMSX2 history of `pcsx2/SaveState.h` shows no fork-specific bump.

**Consequence.** `compat_key = "pcsx2:sv=<g_SaveVersion>"` and `"armsx2:sv=<g_SaveVersion>"`
carry the same number today. Because ARMSX2 merges upstream periodically and was 66 commits
behind when I read it, a future upstream `[SAVEVERSION+]` bump will leave the two on different
versions until ARMSX2 catches up. The exact-match rule in DESIGN §8 handles that case by itself.

### Fork-specific state changes

All **confirmed** by diffing `SaveState.{h,cpp}` against the merge base (347 changed lines in
the `.cpp`):

1. **Legacy loader for AetherSX2/NetherSX2-era states.** `SaveStateLegacy::IsSupportedVersion`
   accepts major versions `0x9A2C` and `0x9A34`
   ([`SaveStateLegacy.cpp:63`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/SaveStateLegacy.cpp#L63)). Load-only; ARMSX2 always
   writes `0x9A59`. This matters for users migrating from NetherSX2, not for sync.
2. **FPU block kept in upstream layout on purpose.** The EE FPR slots are now 64-bit
   (`FPRreg` union with `double d; u64 UD`) with a relocation mode, but states go through
   `fpuRegistersWire`, a 264-byte struct. Its comment: "The FPU block as savestates carry it.
   The format is shared with upstream and does not move"
   ([`R5900.h:175-184`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/R5900.h#L175-L184), `SaveState.cpp` `fpuRegsToWire`).
3. **Recompiler-private state is not serialized.** The arm64 EE call-return stack, COP2 constants
   and so on live in `cpuRegistersPack` but are explicitly not in states ([`R5900.h:284-325`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/R5900.h#L284-L325)).
4. **On load**: clears `VU0.VIWrites` / `VU1.VIWrites` (the "recent VI writes behind the VU
   branch hazard are not in the state") and calls `IopThread::ResyncClocks()`
   ([`SaveState.cpp:~89-97`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/SaveState.cpp#L89-L97)). That is behaviour on load, not
   format.
5. **Extra zip entries only for arcade games**: `ACRAM.bin`, `ACSRAM.bin`, `jvs_state.bin`,
   written when `Arcade::IsActive()` (`IsInUse()`).
6. **Libretro in-memory zip** (`SaveState_ZipToBuffer`, `UnzipFromBuffer`).
7. **Autosave slot.** `VMManager::GetSaveStateFileName` has an extra
   `<serial> (<CRC>).autosave.p2s` name beside upstream's `.resume.p2s` and `.NN.p2s`
   ([`VMManager.cpp:2575-2586`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/VMManager.cpp#L2575-L2586)). Files live in
   `EmuFolders::Savestates`. The user-facing messages say "ARMSX2".

### What is not verified

**Inferred:** that a state saved by x86-64 PCSX2 loads and runs in ARMSX2, and the reverse. The
source suggests it is intended ("The format is shared with upstream"). But the CPU cores, the
64-bit FPU slots and the IOP thread are all ARMSX2 changes, and I did not load a state in both
directions. I also did not diff every `DoState` function (`R3000A`, `GS`, `SPU2`, `Sio*`), only
the top-level container, the EE FPU block and the version.

**Recommendation for the `compat_key` idea.**

- Start with **separate keys** per emulator: `armsx2:sv=9A590000` and `pcsx2:sv=9A590000`.
  Nothing is lost, because the library only offers exact matches.
- After a round-trip test (save in one, load in the other, with a few games including ones that
  use the VU and GS paths), publish a shared key such as `ps2-core:sv=9A590000`, set by the
  emulator, not guessed by the library.
- Whichever key is used, the SaveVersion and the zip layout are not the whole story. Make the key
  include a per-fork "state revision" field (`armsx2:sv=9A590000:r1`) so a future ARMSX2 change
  that alters the state without bumping `g_SaveVersion` can opt out of sharing.

---

## 6. License and contribution

- **License.** ARMSX2 is **GPL-3.0** (`COPYING.GPLv3`; GitHub reports `GPL-3.0`). Source files
  use `SPDX-License-Identifier: GPL-3.0+` (69 hits under `pcsx2/SIO` and the Android Kotlin;
  new ARMSX2 files carry "ARMSX2 Contributors"). **Confirmed.**
- **Compatibility with emusync.** The workspace is `MIT OR Apache-2.0`
  ([`Cargo.toml`](../../Cargo.toml)). Both are compatible with GPL-3.0 (Apache-2.0 is GPLv3
  compatible, though not GPLv2). The combined ARMSX2 + emusync binary is distributable under
  GPL-3.0, with no relicensing. **Inferred** (my reading of the licenses, not legal advice).
  One thing to check: emusync's transitive dependencies (`ring`, `rustls`, `webpki-roots`,
  `rusqlite`/SQLite, `zstd`) should be run through `cargo deny` or `cargo about`, because
  ARMSX2 ships a third-party licence page
  ([`assets/licenses/android.html`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/src/main/assets/licenses/android.html),
  which lists librashader) that would need emusync and its dependencies added.
- **Dynamic linking is already how the project handles licence boundaries.** The Discord bridge
  is a separate library, kept off the `emucore` link line so a proprietary SDK is never combined
  with GPL code (`CMakeLists.txt:319-340`). emusync does not need that separation (its licence is
  compatible), but a `cdylib` fits the same pattern, and the optional-build pattern (librashader)
  fits the way they add features.
- **CLA / contribution.** I found no `CONTRIBUTING` file and no CLA or DCO requirement in the
  repository. The only contribution scaffolding is a PR template
  ([`.github/PULL_REQUEST_TEMPLATE.md`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/.github/PULL_REQUEST_TEMPLATE.md)) asking for a
  description, rationale, testing steps and an optional AI-assistance note, and issue templates.
  The README says nothing about contributions. **Confirmed that no CLA exists in the
  repository; inferred that none is required**, because a CLA could be handled in a channel I
  cannot see (their Discord). The upstream PCSX2 project also has no CLA, but that is a separate
  project's policy.
- **Likely stance on an optional network feature.** The Android manifest requests `INTERNET`, and
  the app already contains an updater, online icon downloads, a texture-pack installer and
  WebRTC remote play, so network features are not new. The Play build strips features Play
  policy disallows (`play` vs `github` flavours). A cloud-sync feature that uploads user data to
  a user-supplied S3 bucket needs a data-safety declaration for Play; gating it behind a flavour
  or a runtime opt-in would be consistent with how LSFG and the updater are handled.
  **Inferred.**
- **Process suggestion.** Open an issue or a draft PR first. The maintainers have shown they
  take optional, compile-out features (librashader, LSFG, Discord) and use AI assistance openly,
  so a small, gated, well-tested PR is likely welcome. **Inferred;** I have not spoken to them.

---

## 7. Integration sketch

### Principles that follow from the above

- **One `es_client` per process, owned by native code.** Created in the C++ core, with a few
  `NativeApp` JNI methods so Kotlin can configure remotes, credentials and show events.
- **`data_dir` is a private internal directory, not the data root.** The DB, staging and
  snapshots (DESIGN §4.1) should not sit on FUSE-emulated shared storage, and must be excluded
  from Android Auto Backup (see the next point). Use
  `Context.getNoBackupFilesDir()/emusync` and pass it to `NativeApp`. **Inferred**, but
  `AndroidManifest.xml:56` sets `allowBackup="true"` with both backup-rule files left as empty
  samples ([`backup_rules.xml`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/src/main/res/xml/backup_rules.xml)), so
  anything in `filesDir` is backed up by default. That is exactly the "app backup restored onto a
  second device (cloned device ID)" case in DESIGN §11. `noBackupFilesDir` avoids it, and
  explicit `<exclude>` rules would be belt and braces.
- **Container paths are native paths.** No `es_vfs` (section 1). Set `format` from
  `EmuConfig.Mcd[slot].Type` (`"ps2.card"` or `"ps2.card.folder"`), `location` from
  `EmuConfig.FullpathToMcd(slot)`, `card` to `"Mcd001"` / `"Mcd002"` by slot, and for a
  serial-named `<serial>.ps2` also `title_scope = <serial>`.
- **`es_container_add` is idempotent**, so register in the same hook as `es_session_begin`,
  every boot, after `FileMcd_SetType()` has decided File or Folder.
- **Skip multitap slots (3-8) and BIOS-only boots in the first version.**

### Call sites

| `emusync.h` call | Where | Notes |
|---|---|---|
| `es_client_create`, `es_remote_add` | New JNI `NativeApp.emusyncInit(...)`, called from Kotlin once the data root is pinned, i.e. after `kickoffEmucoreInit` ([`MainActivityRuntime.kt:2216`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/src/main/java/com/armsx2/runtime/MainActivityRuntime.kt#L2216)) next to `NativeApp.initialize` ([`native-lib.cpp:288`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/src/main/cpp/native-lib.cpp#L288)). | Credentials come from Android Keystore through the `es_credential_provider` callback into JNI. `emulator_id = "armsx2"`; `emulator_version` from `BuildConfig.VERSION_NAME`. |
| `es_container_add` + **`es_session_begin`** | **`VMManager::UpdateDiscDetails`, immediately before `FileMcd_Reopen(...)` at [`VMManager.cpp:1491`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/VMManager.cpp#L1491)**, guarded by `booting` (equivalently `s_state == VMState::Initializing`). | The serial is set, `FileMcd_SetType()` has not yet run for this boot (it runs inside `FileMcd_Reopen`), so determine File/Folder with `FileSystem::DirectoryExists(EmuConfig.FullpathToMcd(slot))`. The cards are closed here: `Initialize` has not opened them, and nothing has called `FileMcd_EmuOpen`. That is the "MUST be called before the emulator opens mounted containers" condition. |
| `es_container_touched` | `FileMcd_Save` ([`MemoryCardFile.cpp:834`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/SIO/Memcard/MemoryCardFile.cpp#L834)) after a successful return, using the slot→container-id table. | One call covers file and folder cards and every guest write. It runs on the IOP thread when `IopThread` is enabled; `touched` is "cheap" and thread-safe. For folder cards the bytes reach disk only at `FolderMemoryCard::Flush` (1093), so call `touched` there too. |
| `es_container_flushed` | (a) `FileMcd_Flush()` ([`689`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/SIO/Memcard/MemoryCardFile.cpp#L689)), for each open slot, after the two `Flush()` calls. (b) `FileMcd_EmuClose()` ([`680`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/SIO/Memcard/MemoryCardFile.cpp#L680)), after `impl.Close()`. (c) The end of `FolderMemoryCard::Flush`. | (a) is hit by `NativeApp.pause()` on every background; (b) by every shutdown, disc change and card change. `flushed` "blocks only for the local extract (ms)", which is acceptable on the CPU thread while the VM is paused or stopping. |
| **`es_session_end`** | `VMManager::Shutdown`, **after `FileMcd_EmuClose()` at [`VMManager.cpp:2411`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/pcsx2/VMManager.cpp#L2411)**, before `Host::OnVMDestroyed()` (2444). | The header says `es_session_end` also "applies inbox items to now-closed containers", so the cards must already be closed. `played_ms`: measure with a monotonic timer started after `es_session_begin` (the existing `SaveSessionTime` at 2376 is per-serial and persisted, not a session length). **Do not wait for the network here**, see the next section. |
| `es_app_suspend` | Native: in `NativeApp.setAppBackgrounded(true)` ([`native-lib.cpp:4023`](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/src/main/cpp/native-lib.cpp#L4023)), reached from `onStop` ([line 6006](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/src/main/java/com/armsx2/runtime/MainActivityRuntime.kt#L6006)). Queue it on the CPU thread **behind** the existing flush task so it runs after `FileMcd_Flush()`. With no VM running, call it directly. | `onPause` is too early: it also fires for split-screen and picture-in-picture, where the app stays visible and running (the existing comment at `MainActivityRuntime.kt:5996-5999` makes the same point for audio). The CPU-thread queue is FIFO, so a task queued from `setAppBackgrounded` runs after the pause task that `onPause` queued. |
| `es_app_resume`, `es_sync_now` | `onStart` ([6000](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/src/main/java/com/armsx2/runtime/MainActivityRuntime.kt#L6000)) via JNI. Also `es_sync_now` when the library screen opens or after a game exits. | A warm sync before the user taps a game makes the boot-time `es_session_begin` fast. This matters because that call blocks the emulation thread during boot. |
| `es_event_poll` / `es_event_set_notifier` | JNI `NativeApp.emusyncPollEvents()` called from a Compose `LaunchedEffect`, with the `wake` notifier posting to the main handler. | Needed for `ES_EV_CONFLICT`, `APPLY_BLOCKED`, `AUTH_REQUIRED`. The in-game overlay already uses Compose `PadModal`s, so conflict UI belongs there. The app bans window dialogs (`checkNoWindowModals` in `build.gradle.kts:369`) so a conflict UI must use `PadModal`. |
| `es_client_destroy` | `NativeApp.shutdown()`'s caller in `onDestroy` ([6051](https://github.com/ARMSX2/ARMSX2/blob/e7235739936c5ad8d09af20bf0fd382624caff8b/platforms/android/app/src/main/java/com/armsx2/runtime/MainActivityRuntime.kt#L6051)), just before `killProcess`, with a short timeout. | The process is killed immediately afterward, so only the outbox flush matters. |
| `es_state_*` | M3. `SaveStateToSlot` / `SaveStateFileName` (`VMManager.cpp:2575`) and `NativeApp.saveStateToSlot` (`native-lib.cpp:4195`), `Autosave::OnShutdown` (2370). | The resume state is `<serial> (<CRC>).resume.p2s`; ARMSX2 also has `.autosave.p2s`. |

### Ordering with the existing backup code

ARMSX2 already snapshots cards before a boot, twice: Kotlin does it just before `runVMThread`,
and native `MemcardBackup::OnCardsReopening` does it inside `FileMcd_Reopen` once the cards are
closed (rolling zips, pruned to the newest 3 plus 4 daily). With the hook above, the order for
one boot is:

1. Kotlin `snapshotActiveCards` (the card as it was before this boot).
2. `es_session_begin` (emusync applies remote versions to the closed cards).
3. `MemcardBackup::OnCardsReopening` (the card after sync, which is the card the game will see).
4. `FileMcd_EmuOpen`.

Step 1 gives a recovery point from before emusync touched anything, which is a useful extra
safety net for the "never lose a save" goal. Mention it in release notes.

### Mid-game reopen (open question)

`FileMcd_Reopen` also runs under a live VM (settings change, card swap, disc change; see
section 3). For those, do **not** call `es_session_begin` (it blocks up to 3 s on a thread that
is running the game). Suggested rule: begin only when `booting`, and end-then-begin a new
session only when the **serial** changes (a different game started from the BIOS browser). That
is a product decision to confirm with the maintainers. On a pure card swap, `es_container_flushed`
on `EmuClose` and a re-registration of the containers should be enough.

### A header gap this research found

`es_session_end(c, id, played_ms)` has no timeout and the header says it pushes to the remote.
On Android the budget is tight: `NativeApp.shutdown()` waits at most 5 s for the VM, then
`onDestroy` joins the emulation thread for 2 s and **kills the process**. A slow or offline S3
endpoint could hold `es_session_end` past that, and the process dies mid-push. DESIGN §11
already says the outbox makes this safe ("process killed mid-upload"), so the data is not at
risk, but the lease release and the push would be delayed to the next launch.

Two options for the ABI: give `es_session_end` a `timeout_ms` like `es_session_begin` and
`es_app_suspend` have, or document that the call returns after the local outbox write and
continues the push on a library thread. I would take the first. This is a note for the sibling
session working on the C ABI.

### Open questions for the maintainers

1. Will they accept an optional, compile-out sync feature (`ARMSX2_ENABLE_EMUSYNC`), and in
   which flavours?
2. Which spelling do they want as the default card name, `mcd001.ps2` or `Mcd001.ps2`? It is
   harmless to sync either with an explicit `card`, but a mismatch with desktop users' files
   will confuse users who copy cards by hand.
3. Are mid-game card reopens (section 3) something they would rather have a "sync pending" flag
   for than a blocking call?
4. Does the 66-commit gap to upstream (and the 2026-10-07 memory-card fixes) mean they plan to
   merge upstream soon? That affects whether card ECC bytes will match desktop.

### Things I did not do

- No build, no run, no device. Everything about linking and FUSE behaviour is read from source or
  inferred.
- I did not read the iOS port (`platforms/ios`) beyond noticing that it calls the same
  `FileMcd_Flush()`. The core hooks in section 7 would serve it too.
- I did not diff every state-serialization function against upstream, only the container and
  the EE FPU block (section 5).
- I did not contact the ARMSX2 maintainers.

/*
 * emusync.h — C ABI for the emusync save-sync library (ABI version 1).
 *
 * Design: docs/DESIGN.md (§3.2 threading, §6.4 sync hooks, §10 integration walkthrough, §12 ABI
 * stability, §20 implementation notes). This header is the integration contract for emulators.
 *
 * Linking: build the static library with `cargo build -p emusync-ffi --release`
 * (libemusync.a / emusync.lib) and link it together with the system libraries Rust's standard
 * library needs (`cargo rustc -p emusync-ffi --lib -- --print native-static-libs` lists them).
 * A shared library is built with `cargo rustc -p emusync-ffi --lib --release --crate-type cdylib`;
 * define EMUSYNC_SHARED when compiling against it.
 *
 * Conventions
 *  - All strings are UTF-8, NUL-terminated. Input strings are copied; the caller keeps ownership.
 *    A string that is not valid UTF-8 is rejected with ES_E_INVALID_ARG.
 *  - Every struct the host passes in, and every struct the library fills, begins with
 *    `struct_size`. Initialise it with the matching ES_*_INIT macro: a struct smaller than this
 *    library's version of it is rejected with ES_E_ABI, so newer library versions can append
 *    fields without breaking older hosts. (es_version_info, embedded in es_conflict_info, is
 *    the exception: it is fixed for ABI version 1.)
 *  - All functions are thread-safe, except that es_client_destroy must not race with other calls
 *    on the same client. NULL pointers where a value is required return ES_E_INVALID_ARG.
 *  - Only functions documented as "blocks" wait, each with a deadline; everything else queues
 *    work for the library's worker thread and returns at once. A few calls need an answer from
 *    the worker ("waits for the worker"): they wait up to 10 s for it to finish what it is doing
 *    (normally nothing; at worst a network operation) and return ES_TIMEOUT, having had no
 *    effect, if it doesn't.
 *  - The library never calls into the host from its own threads except for: the log callback,
 *    the event `wake` notifier, and the credential provider. Callbacks must not throw or unwind,
 *    and must not call back into emusync. None runs after es_client_destroy returns.
 *  - No Rust panic crosses the ABI: an internal error returns ES_E_INTERNAL.
 */
#ifndef EMUSYNC_H
#define EMUSYNC_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#if defined(_WIN32) && defined(EMUSYNC_SHARED)
#  ifdef EMUSYNC_BUILDING
#    define ES_API __declspec(dllexport)
#  else
#    define ES_API __declspec(dllimport)
#  endif
#elif defined(__GNUC__) && defined(EMUSYNC_SHARED)
#  define ES_API __attribute__((visibility("default")))
#else
#  define ES_API
#endif

#define ES_ABI_VERSION 1u

/* ------------------------------------------------------------------------------------------ */
/* Basic types                                                                                */
/* ------------------------------------------------------------------------------------------ */

typedef struct es_client es_client;      /* opaque */
typedef uint64_t es_container_id;
typedef uint64_t es_session_id;
typedef uint64_t es_conflict_id;

typedef enum es_status {
    ES_OK              = 0,
    ES_PENDING         = 1,   /* accepted; completes in the background (watch events) */
    ES_TIMEOUT         = 2,   /* blocking call hit its deadline; work continues in background */
    ES_OFFLINE         = 3,   /* remote unreachable; local operation succeeded */
    ES_CONFLICT        = 4,   /* one or more units need a decision (see ES_EV_CONFLICT) */

    ES_E_INVALID_ARG   = -1,  /* also: a feature this version doesn't implement (see below) */
    ES_E_IO            = -2,
    ES_E_FORMAT        = -3,  /* container failed validation; it was NOT modified */
    ES_E_NO_SPACE      = -4,  /* target container (e.g. memory card) is full */
    ES_E_BUSY          = -5,  /* container is in use by an active session */
    ES_E_AUTH          = -6,  /* remote credentials missing/expired (see ES_EV_AUTH_REQUIRED) */
    ES_E_QUOTA         = -7,
    ES_E_INCOMPATIBLE  = -8,  /* e.g. a card mode this build has no adapter for */
    ES_E_NOT_FOUND     = -9,
    ES_E_ABI           = -10, /* struct_size / ABI version mismatch */
    ES_E_LOCKED        = -11, /* the repository is encrypted and this device has no key yet:
                                 es_repo_unlock with the passphrase */
    ES_E_PASSPHRASE    = -12, /* the passphrase does not unlock the repository */
    ES_E_INTERNAL      = -100
} es_status;

ES_API uint32_t    es_abi_version(void);               /* ES_ABI_VERSION of the library */
ES_API const char* es_version_string(void);            /* e.g. "0.1.0 (abi 1)"; static */
ES_API const char* es_status_str(es_status s);         /* static string; safe for any value */

/* ------------------------------------------------------------------------------------------ */
/* Logging                                                                                    */
/* ------------------------------------------------------------------------------------------ */

typedef enum es_log_level { ES_LOG_ERROR, ES_LOG_WARN, ES_LOG_INFO, ES_LOG_DEBUG, ES_LOG_TRACE } es_log_level;

/* Called from the library's threads, and from the host's own thread during an API call, for
   records at or above the configured level. `target` is the module (e.g. "emusync_core::engine").
   Both strings are valid only during the call. Must not call back into emusync. */
typedef void (*es_log_fn)(void* user, es_log_level level, const char* target, const char* msg);

/* ------------------------------------------------------------------------------------------ */
/* Optional VFS (e.g. Android SAF content:// URIs). Reserved: ABI version 1 accepts only NULL  */
/* (native paths) in es_config.vfs and returns ES_E_INVALID_ARG otherwise.                    */
/* ------------------------------------------------------------------------------------------ */

typedef struct es_vfs_stat {
    uint64_t size;
    int64_t  mtime_unix_ms;
    uint8_t  is_dir;
} es_vfs_stat;

typedef enum es_vfs_open_mode { ES_VFS_READ = 1, ES_VFS_WRITE_TRUNCATE = 2 } es_vfs_open_mode;

typedef struct es_vfs {
    uint32_t struct_size;
    void*    user;
    int  (*open)(void* user, const char* uri, es_vfs_open_mode mode, void** out_handle);
    int  (*read_at)(void* user, void* handle, uint64_t offset, void* buf, size_t len, size_t* out_read);
    int  (*write)(void* user, void* handle, const void* buf, size_t len);
    int  (*sync)(void* user, void* handle);                          /* fsync equivalent */
    int  (*close)(void* user, void* handle);
    int  (*stat)(void* user, const char* uri, es_vfs_stat* out);
    /* Replace `dst` with `src` atomically if possible. If the platform cannot (SAF), return
       ES_E_IO and the library falls back to its journalled-copy protocol. May be NULL. */
    int  (*replace)(void* user, const char* src_uri, const char* dst_uri);
    int  (*remove)(void* user, const char* uri);
    int  (*mkdir)(void* user, const char* uri);
    /* Enumerate children of a directory; `cb` is called once per entry name. */
    int  (*list)(void* user, const char* dir_uri,
                 void (*cb)(void* cb_user, const char* name, const es_vfs_stat* st), void* cb_user);
    /* Produce a sibling URI for temp files (e.g. "<dir>/<name>.emusync-tmp"). */
    int  (*sibling)(void* user, const char* uri, const char* suffix, char* out, size_t out_cap);
} es_vfs;

/* ------------------------------------------------------------------------------------------ */
/* Client                                                                                     */
/* ------------------------------------------------------------------------------------------ */

typedef enum es_conflict_policy {
    ES_POLICY_ASK = 0,          /* raise ES_EV_CONFLICT and wait for es_conflict_resolve */
    ES_POLICY_NEWEST,           /* higher HLC wins; loser kept as a recoverable branch */
    ES_POLICY_PREFER_LOCAL,     /* reserved: ES_E_INVALID_ARG in ABI version 1 */
    ES_POLICY_PREFER_REMOTE     /* reserved: ES_E_INVALID_ARG in ABI version 1 */
} es_conflict_policy;

/* Accepted; no effect in ABI version 1 (there are no save states or leases yet). */
enum es_config_flags {
    ES_CFG_SYNC_STATES_ON_METERED = 1u << 0,   /* default off */
    ES_CFG_NO_SAVES_ON_METERED    = 1u << 1,   /* default: saves do sync on metered */
    ES_CFG_DISABLE_LEASES         = 1u << 2
};

typedef struct es_config {
    uint32_t           struct_size;
    const char*        data_dir;          /* required: private dir for DB, staging, snapshots (native
                                             path). One client per data_dir per process (else
                                             ES_E_BUSY). */
    const char*        device_name;       /* shown to other devices; NULL = the name stored in
                                             data_dir, else the OS host name */
    const char*        emulator_id;       /* e.g. "pcsx2", "armsx2", "emusyncd"; NULL = "emusync" */
    const char*        emulator_version;
    const es_vfs*      vfs;               /* must be NULL in ABI version 1 (native file system) */
    es_conflict_policy conflict_policy;
    uint32_t           flags;             /* es_config_flags */
    uint64_t           snapshot_budget_bytes; /* local history store; 0 = default (100 MiB) */
    es_log_fn          log;               /* NULL = no logging */
    void*              log_user;
    es_log_level       log_level;
} es_config;

#define ES_CONFIG_INIT { sizeof(es_config), NULL, NULL, NULL, NULL, NULL, ES_POLICY_ASK, 0u, 0u, NULL, NULL, ES_LOG_INFO }

/* Opens (or initialises) the local state in data_dir and starts the worker thread. Blocks for
   the local open only. On failure *out is NULL. */
ES_API es_status es_client_create(const es_config* cfg, es_client** out);
/* Blocks up to `timeout_ms` while the worker finishes queued work, snapshots every container to
   the outbox (on disk) and pushes it (best effort). If the deadline passes, the worker finishes
   in the background; either way no callback runs after this returns, and event strings not yet
   released become invalid. NULL is a no-op. */
ES_API void      es_client_destroy(es_client* c, uint32_t timeout_ms);

/* ------------------------------------------------------------------------------------------ */
/* Remotes                                                                                    */
/* ------------------------------------------------------------------------------------------ */

typedef enum es_credential_kind {
    ES_CRED_NONE = 0,           /* reserved (unsigned requests): rejected by S3 remotes */
    ES_CRED_S3,                 /* access key + secret, optionally STS session token + expiry */
    ES_CRED_BEARER              /* reserved (OAuth bearer token, later backends) */
} es_credential_kind;

/* Filled by the host's credential provider. Fixed buffers so no ownership crosses the ABI.
   The library copies the values and wipes the struct after use; it never persists them. */
typedef struct es_credentials {
    uint32_t           struct_size;
    es_credential_kind kind;
    char               access_key_id[128];
    char               secret_access_key[128];
    char               session_token[4096];    /* STS temporary credentials only; else "" */
    int64_t            expires_unix_ms;        /* 0 = static keys; else refreshed 5 min before */
    char               bearer_token[4096];     /* ES_CRED_BEARER only */
} es_credentials;

/* Supplies credentials for `remote_uri`. Called from a library thread: when the remote is first
   used, 5 minutes before temporary credentials expire, and after the remote rejects them (HTTP
   401/403, once per request). Fill `out` (its struct_size is pre-set and every buffer zeroed by
   the library) and return 0, or return ES_E_AUTH if the user must enter or renew credentials:
   the operation then fails with ES_E_AUTH and the library emits ES_EV_AUTH_REQUIRED. `user`
   must stay valid until es_client_destroy. */
typedef struct es_credential_provider {
    void* user;
    int (*get)(void* user, const char* remote_uri, es_credentials* out);
} es_credential_provider;

/* remote_uri, v1 (S3 and S3-compatible):
     "s3://my-saves/emusync/v1?region=eu-west-1"
     "s3://my-saves/emusync/v1?endpoint=https://<account>.r2.cloudflarestorage.com&region=auto"
     "s3://saves/emusync/v1?endpoint=http://nas.local:9000&path_style=1"
   Reserved for later backends: "webdav://…", "dropbox:…", "gdrive:…", "onedrive:…",
   "folder://…", "https://…" (reference server). Returns ES_E_INVALID_ARG for unsupported schemes,
   and for a remote other than the one this data_dir is already bound to.
   Blocks up to 10 s while it validates access with one LIST, probes conditional-write support
   and creates the repository if it is new (unencrypted, unless es_repo_create_encrypted came
   first). Returns ES_OK, ES_E_AUTH (credentials rejected; the remote is not added), or
   ES_OFFLINE if the endpoint is unreachable or slower than that: the remote is still added
   (another remote is refused from then on), and its setup completes in the background or on
   the next sync. For an encrypted repository: ES_E_LOCKED if this data_dir has no key for it
   yet (the remote is added; call es_repo_unlock), and with a passphrase from
   es_repo_create_encrypted, ES_E_PASSPHRASE if it is wrong, or ES_E_INCOMPATIBLE if the
   repository exists and is not encrypted (in both cases the remote is not added).
   ES_E_INCOMPATIBLE also for a repository made by a newer emusync.
   Does not start a sync (see es_sync_now). The provider struct is copied. */
ES_API es_status es_remote_add(es_client* c, const char* remote_uri, const es_credential_provider* creds);

/* End-to-end encryption (docs/DESIGN.md §7, §22). Optional, chosen when a repository is created:
   blobs, log segments and device.json are encrypted with a random repository key, which
   repo.json holds wrapped with a key derived from the passphrase (Argon2id, 64 MiB, 3 passes:
   under 0.1 s on a desktop CPU; the host should still not call it on its UI thread). The passphrase is needed once per data_dir: the unwrapped key is kept in
   data_dir/keys/ (DPAPI-protected on Windows; elsewhere a file only the app can read, which on
   Android and iOS is the app's private storage). The library never logs a passphrase, and keeps
   it in memory only until it is used (wiped afterwards). Neither can be undone: a lost
   passphrase cannot be recovered, and an encrypted repository cannot be made unencrypted.

   es_repo_create_encrypted: call it *before* es_remote_add. The next es_remote_add then creates
   a new repository encrypted with this passphrase, or unlocks an existing encrypted one with it
   (see es_remote_add for its results). Applies to that one es_remote_add only. Returns ES_OK, or
   ES_E_INVALID_ARG for a NULL or empty passphrase. Does not block.

   es_repo_unlock: for an encrypted repository es_remote_add reported as ES_E_LOCKED (a device
   joining an existing repository). Derives the key, checks it against repo.json and keeps it in
   data_dir; no network unless the remote's setup hadn't completed yet. Waits for the worker and
   blocks while the key is derived. Returns ES_OK (then call es_sync_now), ES_E_PASSPHRASE,
   ES_E_INVALID_ARG (NULL or empty passphrase, no remote, or the repository is not encrypted),
   ES_OFFLINE (the remote's setup still can't reach it) or ES_TIMEOUT.

   While a repository is locked, sync cycles fail with ES_E_LOCKED (ES_EV_ERROR and
   ES_EV_SYNC_FINISHED carry it); local snapshots (touched/flushed) still work and are uploaded
   once it is unlocked. Older libraries (before encryption) refuse an encrypted repository at
   es_remote_add with ES_E_INVALID_ARG ("unsupported repository format"). */
ES_API es_status es_repo_create_encrypted(es_client* c, const char* passphrase);
ES_API es_status es_repo_unlock(es_client* c, const char* passphrase);

/* OAuth PKCE helpers for later backends (not used by S3). begin writes an authorization URL
   for the host to open. Return ES_E_INVALID_ARG until such a backend exists (begin writes "" to
   out_url if cap > 0). */
ES_API es_status es_oauth_begin(es_client* c, const char* provider, const char* redirect_uri, char* out_url, size_t cap);
ES_API es_status es_oauth_complete(es_client* c, const char* provider, const char* redirect_with_code);

/* ------------------------------------------------------------------------------------------ */
/* Containers (memory cards, SRAM files, save folders)                                         */
/* ------------------------------------------------------------------------------------------ */

/* A container is the local file/folder holding a logical card (DESIGN §2). The default layout
   is one image file per card (PCSX2/ARMSX2 Mcd001.ps2, Mcd002.ps2), registered as shared
   containers with `card` left NULL. Logical cards with the same name are the same card on
   every device. */

enum es_container_flags {
    ES_CF_READ_ONLY = 1u << 0,  /* upload from, never write to */
    ES_CF_NO_UPLOAD = 1u << 1   /* receive only. Reserved: ES_E_INVALID_ARG in ABI version 1 */
};

typedef struct es_container_desc {
    uint32_t    struct_size;
    const char* format;       /* "ps2.card", "ps2.card.folder", "raw"; NULL = auto-detect (a PS2
                                 card image or folder card, else "raw": whole-file only).
                                 Reserved: "ps1.card", "gc.card", "gc.gci", "sram", "dir". */
    const char* location;     /* native path */
    const char* platform;     /* "ps2", "ps1", "gc", ... ; NULL = inferred from format/extension */
    const char* card;         /* logical card name, e.g. "Mcd001". NULL = the file or folder stem.
                                 Set it when stems differ between emulators. */
    const char* title_scope;  /* must be NULL in ABI version 1 (shared containers only): a
                                 per-game container returns ES_E_INVALID_ARG */
    uint32_t    flags;        /* es_container_flags */
} es_container_desc;

#define ES_CONTAINER_DESC_INIT { sizeof(es_container_desc), NULL, NULL, NULL, NULL, NULL, 0u }

/* Idempotent: re-adding the same location returns the existing id. Waits for the worker, and
   reads the container once to detect its format. ES_E_INVALID_ARG for an unknown format, a
   directory that is not a folder card, or a logical card that already has a container on this
   device. Publishes the logical card to the repository (in unit mode where an adapter exists,
   else whole-file) unless it already exists, in which case the repository's mode is adopted. */
ES_API es_status es_container_add(es_client* c, const es_container_desc* d, es_container_id* out);
/* Unregisters a container (the file is left alone). Waits for the worker. ES_E_BUSY while a
   session has it mounted (or is starting with it), ES_E_NOT_FOUND for an unknown id. */
ES_API es_status es_container_remove(es_client* c, es_container_id id);

/* Logical card sync mode, shared by every device (DESIGN §2.1). */
typedef enum es_card_mode {
    ES_CARD_UNITS = 0,          /* per-save sync inside the card (default when an adapter exists) */
    ES_CARD_WHOLE_FILE          /* card image is one opaque unit: byte-identical mirrors */
} es_card_mode;

/* card_id is "<platform>:<card>", e.g. "ps2:Mcd001", or the card name alone if unambiguous.
   Neither call waits. get_mode: ES_E_NOT_FOUND for a card the repository doesn't know.
   set_mode takes a final snapshot in the old mode, publishes the change on the next push, and
   returns ES_PENDING (ES_OK if the card is already in that mode); ES_E_BUSY while a session has
   the card open; ES_E_INVALID_ARG for ES_CARD_UNITS on a format without an adapter. */
ES_API es_status es_card_get_mode(es_client* c, const char* card_id, es_card_mode* out);
ES_API es_status es_card_set_mode(es_client* c, const char* card_id, es_card_mode mode);

/* Emulator wrote to the container. Cheap; (re)arms a 2 s debounce, then a quiescent snapshot to
   the outbox, then a push. */
ES_API void      es_container_touched(es_client* c, es_container_id id);
/* Emulator flushed/closed the container: snapshot now, ahead of other queued work, then push in
   the background. Blocks up to 2 s for the local extract: ES_OK once it is taken, ES_PENDING if
   the worker was busy (it is taken as soon as the worker is free), ES_E_NOT_FOUND. */
ES_API es_status es_container_flushed(es_client* c, es_container_id id);

/* ------------------------------------------------------------------------------------------ */
/* Game sessions                                                                              */
/* ------------------------------------------------------------------------------------------ */

typedef struct es_title {
    uint32_t    struct_size;
    const char* platform;      /* "ps2"; required */
    const char* serial;        /* "SLUS-20062" for disc systems; NULL otherwise */
    const char* content_hash;  /* "sha1:..." / "crc32:..." for cartridge systems; NULL otherwise */
    const char* display_name;  /* optional, for UI on other devices */
} es_title;

#define ES_TITLE_INIT { sizeof(es_title), NULL, NULL, NULL, NULL }

enum es_session_flags {
    ES_SB_QUERY_RESUME_STATE = 1u << 0   /* fill es_session_info.resume_state_* (M3: always "") */
};

typedef struct es_session_begin_args {
    uint32_t               struct_size;
    const es_title*        title;          /* serial or content_hash is required */
    const es_container_id* mounted;        /* containers in slots for this session, slot order */
    size_t                 mounted_count;
    uint32_t               timeout_ms;     /* max time to wait for remote; 0 = default (3000) */
    uint32_t               flags;          /* es_session_flags */
    const char*            state_compat_key; /* required with ES_SB_QUERY_RESUME_STATE */
} es_session_begin_args;

#define ES_SESSION_BEGIN_INIT { sizeof(es_session_begin_args), NULL, NULL, 0u, 0u, 0u, NULL }

typedef struct es_session_info {
    uint32_t      struct_size;
    es_session_id id;
    uint32_t      units_applied;          /* remote versions written into mounted containers */
    uint32_t      units_blocked;          /* could not apply (e.g. card full); see events */
    uint32_t      conflicts;              /* unresolved conflicts for this title (and for
                                             whole-file cards that are mounted) */
    uint8_t       remote_reached;         /* 0 if offline/timeout: playing on local data */
    char          other_device_active[64];/* device holding a live lease, or "" (no leases yet:
                                             always "" in ABI version 1) */
    char          resume_state_id[72];    /* newer compatible "resume" state, or "" (M3) */
    int64_t       resume_state_unix_ms;
} es_session_info;

#define ES_SESSION_INFO_INIT { sizeof(es_session_info), 0u, 0u, 0u, 0u, 0u, {0}, {0}, 0 }

/* Blocks up to args->timeout_ms. MUST be called before the emulator opens mounted containers.
   Marks the mounted containers busy, pulls, and applies remote changes to them (and to any other
   container not in use). Returns ES_OK, ES_OFFLINE, ES_TIMEOUT or ES_CONFLICT; all of these mean
   "safe to start": once it returns, the library never writes to the mounted containers until
   es_session_end, even if the pull is still running (its results wait in the inbox). The return
   may exceed the deadline by the one container write that was in progress at it.
   ES_E_BUSY if a mounted container is in another open session; ES_E_NOT_FOUND for an unknown
   container. */
ES_API es_status es_session_begin(es_client* c, const es_session_begin_args* args, es_session_info* out);
/* Queues the final snapshot of the mounted containers, then clears their busy mark and runs a
   full cycle (pull, apply queued inbox items to the now-closed containers, push). Returns
   ES_PENDING; ES_E_NOT_FOUND for an unknown session. `played_ms` is not recorded yet. */
ES_API es_status es_session_end(es_client* c, es_session_id id, uint64_t played_ms);

/* Mobile lifecycle. suspend blocks up to budget_ms, ahead of other queued work: every container
   is snapshotted to the on-disk outbox first, then a best-effort push. Returns ES_OK (pushed),
   ES_OFFLINE, ES_TIMEOUT (budget used up; the work continues), or an error. Platform bindings
   also schedule a deferred background job. resume queues a full cycle (ES_PENDING). */
ES_API es_status es_app_suspend(es_client* c, uint32_t budget_ms);
ES_API es_status es_app_resume(es_client* c);

/* Full background cycle (pull, snapshot, apply to containers not in a session, push). Containers
   in a session are not read by any cycle: they are snapshotted only at es_container_touched (after
   the debounce), es_container_flushed and es_session_end, and remote changes for them wait in the
   inbox. Returns
   ES_PENDING; calls made while one is queued are merged into it. The library has no timer of its
   own in ABI version 1: call this when the host wants a sync (e.g. periodically, or from UI). */
ES_API es_status es_sync_now(es_client* c);

/* ------------------------------------------------------------------------------------------ */
/* Save states (milestone M3: every function returns ES_E_INVALID_ARG in ABI version 1)        */
/* ------------------------------------------------------------------------------------------ */

typedef struct es_state_desc {
    uint32_t        struct_size;
    const es_title* title;
    const char*     path;             /* state file; copied into staging before return */
    const char*     compat_key;       /* e.g. "pcsx2:sv=<g_SaveVersion>" — exact-match only */
    const char*     slot;             /* "resume", "1".."10", or host-defined */
    const char*     screenshot_path;  /* optional PNG */
} es_state_desc;

#define ES_STATE_DESC_INIT { sizeof(es_state_desc), NULL, NULL, NULL, NULL, NULL }

typedef struct es_state_info {
    uint32_t struct_size;
    char     id[72];
    char     device_name[64];
    int64_t  created_unix_ms;
    uint64_t size;
    uint8_t  stale_vs_saves;   /* 1 = local saves are newer than this state's linked versions */
} es_state_info;

#define ES_STATE_INFO_INIT { sizeof(es_state_info), {0}, {0}, 0, 0u, 0u }

ES_API es_status es_state_publish(es_client* c, const es_state_desc* d);  /* ES_PENDING */
ES_API es_status es_state_query_latest(es_client* c, const es_title* t, const char* compat_key,
                                       const char* slot, es_state_info* out); /* ES_E_NOT_FOUND if none */
/* Blocks up to timeout_ms; writes atomically to dest_path. */
ES_API es_status es_state_fetch(es_client* c, const char* state_id, const char* dest_path, uint32_t timeout_ms);

/* ------------------------------------------------------------------------------------------ */
/* Conflicts and history                                                                      */
/* ------------------------------------------------------------------------------------------ */

/* Fixed layout for ABI version 1 (embedded in es_conflict_info). Strings are truncated to fit. */
typedef struct es_version_info {
    char     version_id[72];    /* "b3:<64 hex>" */
    char     device_name[64];   /* device that created the version */
    int64_t  modified_unix_ms;  /* when it was created (hybrid logical clock) */
    uint64_t playtime_ms;       /* cumulative for the title at that device, if reported (0 in v1) */
    uint64_t size;
    uint8_t  is_tombstone;
} es_version_info;

typedef struct es_conflict_info {
    uint32_t        struct_size;
    es_container_id container;         /* the local container, or 0 */
    char            unit_id[128];      /* e.g. "ps2:Mcd001:BASLUS-20062GTA3"; for es_unit_* */
    char            unit_name[64];     /* e.g. "BASLUS-20062GTA3" ("@card" in whole-file mode) */
    char            unit_title[128];   /* in-game title from metadata, if any (else "") */
    char            title_key[64];     /* e.g. "ps2:SLUS-20062", or "" */
    es_version_info local;             /* this device's version (all zero if none) */
    es_version_info remote[4];         /* divergent remote tips (usually 1) */
    uint32_t        remote_count;
} es_conflict_info;

#define ES_CONFLICT_INFO_INIT { sizeof(es_conflict_info), 0u, {0}, {0}, {0}, {0}, \
    { {0}, {0}, 0, 0u, 0u, 0u }, { { {0}, {0}, 0, 0u, 0u, 0u } }, 0u }

typedef enum es_resolution {
    ES_RESOLVE_KEEP_LOCAL = 0,
    ES_RESOLVE_TAKE_REMOTE             /* use remote[remote_index] */
} es_resolution;

/* Neither call waits. get: ES_E_NOT_FOUND once the conflict is resolved. resolve records the
   decision as a merge version and runs a full cycle to apply and upload it: ES_PENDING. */
ES_API es_status es_conflict_get(es_client* c, es_conflict_id id, es_conflict_info* out);
ES_API es_status es_conflict_resolve(es_client* c, es_conflict_id id, es_resolution r, uint32_t remote_index);

/* History browsing and restore. unit_id is a unit id ("ps2:Mcd001:BASLUS-20062GTA3"), a save's
   name if it is on one card only, or for a whole-file card the card ("ps2:Mcd001"). Both wait for
   the worker. history calls `cb` on the calling thread, newest version first, before it returns;
   `v` is valid during the call. restore creates a new version whose content equals `version_id`
   (a full id or a unique prefix), then runs a full cycle to apply and upload it: ES_PENDING. */
typedef void (*es_version_cb)(void* user, const es_version_info* v);
ES_API es_status es_unit_history(es_client* c, const char* unit_id, es_version_cb cb, void* user);
ES_API es_status es_unit_restore(es_client* c, const char* unit_id, const char* version_id);

/* ------------------------------------------------------------------------------------------ */
/* Events (pull model)                                                                        */
/* ------------------------------------------------------------------------------------------ */

typedef enum es_event_type {
    ES_EV_SYNC_STARTED = 1,      /* a cycle that may use the network began */
    ES_EV_SYNC_FINISHED,         /* status: ES_OK / ES_OFFLINE / error (message says why) */
    ES_EV_PROGRESS,              /* bytes_done / bytes_total. Reserved: not emitted in v1 */
    ES_EV_UNIT_UPLOADED,         /* a version of unit_id reached the remote */
    ES_EV_UNIT_APPLIED,          /* remote version written into local `container` */
    ES_EV_APPLY_BLOCKED,         /* a container couldn't be synced, or a unit applied to it.
                                    status: ES_E_NO_SPACE (card full) / ES_E_BUSY (in a session,
                                    or locked by another program) / ES_E_FORMAT (failed
                                    validation) / ES_E_NOT_FOUND (container file missing) /
                                    ES_OFFLINE (content not downloadable yet) / ES_E_IO /
                                    ES_E_INCOMPATIBLE. Reported once until the cause changes;
                                    the unit waits in the inbox and is retried every cycle. */
    ES_EV_CARD_MODE_CHANGED,     /* another device switched a card's mode; message = card_id */
    ES_EV_CONFLICT,              /* conflict_id set; call es_conflict_get */
    ES_EV_LEASE_CHANGED,         /* another device started/stopped a title. Reserved (v1: never) */
    ES_EV_HANDOFF_REQUESTED,     /* another device wants this title. Reserved (v1: never) */
    ES_EV_STATE_AVAILABLE,       /* newer compatible state published elsewhere. Reserved (M3) */
    ES_EV_AUTH_REQUIRED,         /* message = remote uri; the credential provider said ES_E_AUTH
                                    or the remote rejected its credentials */
    ES_EV_ERROR                  /* status + message */
} es_event_type;

typedef struct es_event {
    uint32_t        struct_size;
    es_event_type   type;
    es_status       status;
    es_container_id container;     /* 0 if none */
    es_conflict_id  conflict_id;   /* ES_EV_CONFLICT */
    uint64_t        bytes_done;
    uint64_t        bytes_total;
    const char*     title_key;     /* e.g. "ps2:SLUS-20062"; may be NULL */
    const char*     unit_id;       /* e.g. "ps2:Mcd001:BASLUS-20062GTA3"; may be NULL */
    const char*     unit_name;     /* e.g. "BASLUS-20062GTA3"; may be NULL */
    const char*     device_name;   /* may be NULL */
    const char*     message;       /* human-readable, may be NULL */
    void*           _internal;
} es_event;

#define ES_EVENT_INIT { sizeof(es_event), (es_event_type)0, ES_OK, 0u, 0u, 0u, 0u, NULL, NULL, NULL, NULL, NULL, NULL }

/* Non-blocking. Returns 1 and fills `out` if an event was dequeued, 0 if the queue is empty, or a
   negative es_status (ES_E_ABI if out->struct_size is too small). String pointers stay valid until
   es_event_release(out); releasing twice is harmless. Polling into a struct that still holds an
   unreleased event leaks it until es_client_destroy. If more than 4096 events pile up, the oldest
   are dropped and an ES_EV_ERROR says how many. */
ES_API int  es_event_poll(es_client* c, es_event* out);
ES_API void es_event_release(es_client* c, es_event* ev);

/* Optional: `wake` is invoked from a library thread when the queue goes from empty to non-empty,
   so the host can schedule es_event_poll on its own thread. Must not call into emusync from inside
   `wake`. Pass NULL to remove it. */
ES_API void es_event_set_notifier(es_client* c, void (*wake)(void* user), void* user);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* EMUSYNC_H */

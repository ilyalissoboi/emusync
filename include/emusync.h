/*
 * emusync.h — C ABI for the emusync save-sync library.
 *
 * Design: docs/DESIGN.md. This header is the integration contract for emulators.
 *
 * Conventions
 *  - All strings are UTF-8, NUL-terminated. Input strings are copied; the caller keeps ownership.
 *  - Every input struct begins with `struct_size`; initialise with the matching ES_*_INIT macro
 *    so newer library versions can append fields without breaking older hosts.
 *  - All functions are thread-safe. Only functions documented as "blocks" may wait; everything
 *    else queues work and returns immediately.
 *  - The library never calls into the host from its own threads except for: the log callback,
 *    the event `wake` notifier, the token provider, and VFS callbacks. Each documents its thread.
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

    ES_E_INVALID_ARG   = -1,
    ES_E_IO            = -2,
    ES_E_FORMAT        = -3,  /* container failed validation; it was NOT modified */
    ES_E_NO_SPACE      = -4,  /* target container (e.g. memory card) is full */
    ES_E_BUSY          = -5,  /* container is in use by an active session */
    ES_E_AUTH          = -6,  /* remote credentials missing/expired (see ES_EV_AUTH_REQUIRED) */
    ES_E_QUOTA         = -7,
    ES_E_INCOMPATIBLE  = -8,  /* e.g. save state compat_key mismatch */
    ES_E_NOT_FOUND     = -9,
    ES_E_ABI           = -10, /* struct_size / ABI version mismatch */
    ES_E_INTERNAL      = -100
} es_status;

ES_API uint32_t    es_abi_version(void);
ES_API const char* es_version_string(void);          /* e.g. "0.1.0 (rust 1.8x, abi 1)" */
ES_API const char* es_status_str(es_status s);       /* static string */

/* ------------------------------------------------------------------------------------------ */
/* Logging                                                                                    */
/* ------------------------------------------------------------------------------------------ */

typedef enum es_log_level { ES_LOG_ERROR, ES_LOG_WARN, ES_LOG_INFO, ES_LOG_DEBUG, ES_LOG_TRACE } es_log_level;

/* Called from any library thread. Must not call back into emusync. */
typedef void (*es_log_fn)(void* user, es_log_level level, const char* target, const char* msg);

/* ------------------------------------------------------------------------------------------ */
/* Optional VFS (e.g. Android SAF content:// URIs). If not supplied, native paths are used.   */
/* Callbacks are invoked from the library's I/O pool; they must be thread-safe.                */
/* Return 0 on success, negative es_status on failure.                                        */
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
    ES_POLICY_PREFER_LOCAL,
    ES_POLICY_PREFER_REMOTE
} es_conflict_policy;

enum es_config_flags {
    ES_CFG_SYNC_STATES_ON_METERED = 1u << 0,   /* default off */
    ES_CFG_NO_SAVES_ON_METERED    = 1u << 1,   /* default: saves do sync on metered */
    ES_CFG_DISABLE_LEASES         = 1u << 2
};

typedef struct es_config {
    uint32_t           struct_size;
    const char*        data_dir;          /* required: private dir for DB, staging, snapshots (native path) */
    const char*        device_name;       /* shown to other devices; NULL = OS hostname / model */
    const char*        emulator_id;       /* e.g. "pcsx2", "armsx2", "emusyncd" */
    const char*        emulator_version;
    const es_vfs*      vfs;               /* NULL = native file system */
    es_conflict_policy conflict_policy;
    uint32_t           flags;             /* es_config_flags */
    uint64_t           snapshot_budget_bytes; /* local history store; 0 = default (100 MiB) */
    es_log_fn          log;
    void*              log_user;
    es_log_level       log_level;
} es_config;

#define ES_CONFIG_INIT { sizeof(es_config), NULL, NULL, NULL, NULL, NULL, ES_POLICY_ASK, 0u, 0u, NULL, NULL, ES_LOG_INFO }

ES_API es_status es_client_create(const es_config* cfg, es_client** out);
/* Blocks up to `timeout_ms` to flush the outbox to disk (always) and to the remote (best effort). */
ES_API void      es_client_destroy(es_client* c, uint32_t timeout_ms);

/* ------------------------------------------------------------------------------------------ */
/* Remotes                                                                                    */
/* ------------------------------------------------------------------------------------------ */

/* Supplies a bearer/OAuth token for `remote_uri`. Called from a library thread. Write a
   NUL-terminated token into `out` and return 0, or return ES_E_AUTH if the user must sign in
   (the library then emits ES_EV_AUTH_REQUIRED). */
typedef struct es_token_provider {
    void* user;
    int (*get_token)(void* user, const char* remote_uri, char* out, size_t out_cap);
} es_token_provider;

/* remote_uri examples:
     "folder:///Users/me/Syncthing/emusync"   "webdav://nas.local/dav/emusync"
     "s3://bucket/prefix?region=eu-west-1"    "dropbox:/Apps/emusync"
     "gdrive:appDataFolder"                   "onedrive:/emusync"
     "https://sync.example.com"               (reference server)
   `token_provider` may be NULL for backends that need none. */
ES_API es_status es_remote_add(es_client* c, const char* remote_uri, const es_token_provider* token_provider);

/* E2E encryption. Create: initialises a new encrypted repo. Unlock: derives the key on a new device.
   Blocks (Argon2id, ~0.5 s). The derived key is stored in the OS keystore by platform bindings. */
ES_API es_status es_repo_create_encrypted(es_client* c, const char* passphrase);
ES_API es_status es_repo_unlock(es_client* c, const char* passphrase);

/* OAuth PKCE helpers (optional). begin writes an authorization URL for the host to open. */
ES_API es_status es_oauth_begin(es_client* c, const char* provider, const char* redirect_uri, char* out_url, size_t cap);
ES_API es_status es_oauth_complete(es_client* c, const char* provider, const char* redirect_with_code);

/* ------------------------------------------------------------------------------------------ */
/* Containers (memory cards, SRAM files, save folders)                                         */
/* ------------------------------------------------------------------------------------------ */

enum es_container_flags {
    ES_CF_DEFAULT_PLACEMENT = 1u << 0,  /* new units for this platform land here (DESIGN §5.4) */
    ES_CF_MIRROR_ALL        = 1u << 1,  /* receive every unit of the platform during idle sync */
    ES_CF_READ_ONLY         = 1u << 2,  /* upload from, never write to */
    ES_CF_NO_UPLOAD         = 1u << 3   /* receive only */
};

typedef struct es_container_desc {
    uint32_t    struct_size;
    const char* format;       /* "ps2.card", "ps2.card.folder", "ps1.card", "gc.card", "gc.gci",
                                 "sram", "dir", "raw"; NULL = auto-detect */
    const char* location;     /* native path or VFS URI */
    const char* platform;     /* "ps2", "ps1", "gc", ... ; NULL = inferred from format */
    const char* title_scope;  /* NULL = shared card; else title key for a per-game container */
    uint32_t    flags;        /* es_container_flags */
} es_container_desc;

#define ES_CONTAINER_DESC_INIT { sizeof(es_container_desc), NULL, NULL, NULL, NULL, 0u }

/* Idempotent: re-adding the same location returns the existing id. Does not block on I/O. */
ES_API es_status es_container_add(es_client* c, const es_container_desc* d, es_container_id* out);
ES_API es_status es_container_remove(es_client* c, es_container_id id);

/* Emulator wrote to the container. Cheap; arms a debounce timer, then a quiescent snapshot. */
ES_API void      es_container_touched(es_client* c, es_container_id id);
/* Emulator flushed/closed the container: snapshot now. Blocks only for the local extract (ms). */
ES_API es_status es_container_flushed(es_client* c, es_container_id id);

/* ------------------------------------------------------------------------------------------ */
/* Game sessions                                                                              */
/* ------------------------------------------------------------------------------------------ */

typedef struct es_title {
    uint32_t    struct_size;
    const char* platform;      /* "ps2" */
    const char* serial;        /* "SLUS-20062" for disc systems; NULL otherwise */
    const char* content_hash;  /* "sha1:..." / "crc32:..." for cartridge systems; NULL otherwise */
    const char* display_name;  /* optional, for UI on other devices */
} es_title;

#define ES_TITLE_INIT { sizeof(es_title), NULL, NULL, NULL, NULL }

enum es_session_flags {
    ES_SB_QUERY_RESUME_STATE = 1u << 0   /* fill es_session_info.resume_state_* */
};

typedef struct es_session_begin_args {
    uint32_t               struct_size;
    const es_title*        title;
    const es_container_id* mounted;        /* containers in slots for this session, slot order */
    size_t                 mounted_count;
    uint32_t               timeout_ms;     /* max time to wait for remote; 0 = default (3000) */
    uint32_t               flags;          /* es_session_flags */
    const char*            state_compat_key; /* required with ES_SB_QUERY_RESUME_STATE */
} es_session_begin_args;

#define ES_SESSION_BEGIN_INIT { sizeof(es_session_begin_args), NULL, NULL, 0u, 0u, 0u, NULL }

typedef struct es_session_info {
    es_session_id id;
    uint32_t      units_applied;          /* remote versions written into mounted containers */
    uint32_t      units_blocked;          /* could not apply (e.g. card full); see events */
    uint32_t      conflicts;              /* unresolved conflicts for this title */
    uint8_t       remote_reached;         /* 0 if offline/timeout: playing on local data */
    char          other_device_active[64];/* device holding a live lease, or "" */
    char          resume_state_id[48];    /* newer compatible "resume" state, or "" */
    int64_t       resume_state_unix_ms;
} es_session_info;

/* Blocks up to args->timeout_ms. MUST be called before the emulator opens mounted containers.
   Returns ES_OK, ES_OFFLINE, ES_TIMEOUT or ES_CONFLICT; all of these mean "safe to start". */
ES_API es_status es_session_begin(es_client* c, const es_session_begin_args* args, es_session_info* out);
/* Final snapshot + push + lease release; then applies inbox items to now-closed containers. */
ES_API es_status es_session_end(es_client* c, es_session_id id, uint64_t played_ms);

/* Mobile lifecycle. suspend blocks up to budget_ms: everything reaches the on-disk outbox first,
   then a best-effort push. Platform bindings also schedule a deferred background job. */
ES_API es_status es_app_suspend(es_client* c, uint32_t budget_ms);
ES_API es_status es_app_resume(es_client* c);

/* Full background cycle for containers not in an active session. Returns ES_PENDING. */
ES_API es_status es_sync_now(es_client* c);

/* ------------------------------------------------------------------------------------------ */
/* Save states                                                                                */
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
    char     id[48];
    char     device_name[64];
    int64_t  created_unix_ms;
    uint64_t size;
    uint8_t  stale_vs_saves;   /* 1 = local saves are newer than this state's linked versions */
} es_state_info;

ES_API es_status es_state_publish(es_client* c, const es_state_desc* d);  /* ES_PENDING */
ES_API es_status es_state_query_latest(es_client* c, const es_title* t, const char* compat_key,
                                       const char* slot, es_state_info* out); /* ES_E_NOT_FOUND if none */
/* Blocks up to timeout_ms; writes atomically to dest_path. */
ES_API es_status es_state_fetch(es_client* c, const char* state_id, const char* dest_path, uint32_t timeout_ms);

/* ------------------------------------------------------------------------------------------ */
/* Conflicts and history                                                                      */
/* ------------------------------------------------------------------------------------------ */

typedef struct es_version_info {
    char     version_id[48];
    char     device_name[64];
    int64_t  modified_unix_ms;
    uint64_t playtime_ms;       /* cumulative for the title at that device, if reported */
    uint64_t size;
    uint8_t  is_tombstone;
} es_version_info;

typedef struct es_conflict_info {
    char            unit_name[64];     /* e.g. "BASLUS-20062GTA3" */
    char            unit_title[128];   /* in-game title from metadata (icon.sys), if any */
    char            title_key[64];     /* e.g. "ps2:SLUS-20062" */
    es_version_info local;
    es_version_info remote[4];         /* divergent remote tips (usually 1) */
    uint32_t        remote_count;
} es_conflict_info;

typedef enum es_resolution {
    ES_RESOLVE_KEEP_LOCAL = 0,
    ES_RESOLVE_TAKE_REMOTE,            /* use remote[remote_index] */
} es_resolution;

ES_API es_status es_conflict_get(es_client* c, es_conflict_id id, es_conflict_info* out);
ES_API es_status es_conflict_resolve(es_client* c, es_conflict_id id, es_resolution r, uint32_t remote_index);

/* History browsing and restore (creates a new version whose content equals `version_id`). */
typedef void (*es_version_cb)(void* user, const es_version_info* v);
ES_API es_status es_unit_history(es_client* c, const char* unit_id, es_version_cb cb, void* user);
ES_API es_status es_unit_restore(es_client* c, const char* unit_id, const char* version_id);

/* ------------------------------------------------------------------------------------------ */
/* Events (pull model)                                                                        */
/* ------------------------------------------------------------------------------------------ */

typedef enum es_event_type {
    ES_EV_SYNC_STARTED = 1,
    ES_EV_SYNC_FINISHED,         /* status: ES_OK / ES_OFFLINE / error */
    ES_EV_PROGRESS,              /* bytes_done / bytes_total */
    ES_EV_UNIT_UPLOADED,
    ES_EV_UNIT_APPLIED,          /* remote version written into a local container */
    ES_EV_APPLY_BLOCKED,         /* status: ES_E_NO_SPACE / ES_E_BUSY / ES_E_FORMAT */
    ES_EV_CONFLICT,              /* conflict_id set; call es_conflict_get */
    ES_EV_LEASE_CHANGED,         /* another device started/stopped a title (device_name) */
    ES_EV_HANDOFF_REQUESTED,     /* another device wants this title: flush and end session */
    ES_EV_STATE_AVAILABLE,       /* newer compatible state published elsewhere */
    ES_EV_AUTH_REQUIRED,         /* message = remote uri */
    ES_EV_ERROR
} es_event_type;

typedef struct es_event {
    es_event_type   type;
    es_status       status;
    es_container_id container;
    es_conflict_id  conflict_id;
    uint64_t        bytes_done;
    uint64_t        bytes_total;
    const char*     title_key;     /* may be NULL */
    const char*     unit_name;     /* may be NULL */
    const char*     device_name;   /* may be NULL */
    const char*     message;       /* human-readable, may be NULL */
    void*           _internal;
} es_event;

/* Non-blocking. Returns 1 and fills `out` if an event was dequeued, else 0. String pointers stay
   valid until es_event_release(out). */
ES_API int  es_event_poll(es_client* c, es_event* out);
ES_API void es_event_release(es_client* c, es_event* ev);

/* Optional: `wake` is invoked from a library thread when the queue becomes non-empty, so the host
   can schedule es_event_poll on its own thread. Must not call into emusync from inside `wake`. */
ES_API void es_event_set_notifier(es_client* c, void (*wake)(void* user), void* user);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* EMUSYNC_H */

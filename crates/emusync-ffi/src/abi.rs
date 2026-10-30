//! `#[repr(C)]` mirrors of the structs and constants in `include/emusync.h`.
//!
//! C enums cross the ABI as plain integers (`i32`), so an out-of-range value from the host is
//! an error, never undefined behaviour. tests/c_abi.rs compares every size and offset here
//! with what a C compiler makes of the header.

#![allow(non_camel_case_types)]

use std::ffi::{c_char, c_int, c_void};

pub type es_container_id = u64;
pub type es_session_id = u64;
pub type es_conflict_id = u64;

pub const ES_OK: i32 = 0;
pub const ES_PENDING: i32 = 1;
pub const ES_TIMEOUT: i32 = 2;
pub const ES_OFFLINE: i32 = 3;
pub const ES_CONFLICT: i32 = 4;
pub const ES_E_INVALID_ARG: i32 = -1;
pub const ES_E_IO: i32 = -2;
pub const ES_E_FORMAT: i32 = -3;
pub const ES_E_NO_SPACE: i32 = -4;
pub const ES_E_BUSY: i32 = -5;
pub const ES_E_AUTH: i32 = -6;
pub const ES_E_QUOTA: i32 = -7;
pub const ES_E_INCOMPATIBLE: i32 = -8;
pub const ES_E_NOT_FOUND: i32 = -9;
pub const ES_E_ABI: i32 = -10;
pub const ES_E_LOCKED: i32 = -11;
pub const ES_E_PASSPHRASE: i32 = -12;
pub const ES_E_INTERNAL: i32 = -100;

pub const ES_LOG_ERROR: i32 = 0;
pub const ES_LOG_TRACE: i32 = 4;

pub const ES_POLICY_ASK: i32 = 0;
pub const ES_POLICY_NEWEST: i32 = 1;

pub const ES_CRED_NONE: i32 = 0;
pub const ES_CRED_S3: i32 = 1;
pub const ES_CRED_BEARER: i32 = 2;
pub const ES_CRED_OAUTH_REFRESH: i32 = 3;

pub const ES_CF_READ_ONLY: u32 = 1 << 0;
pub const ES_CF_NO_UPLOAD: u32 = 1 << 1;
pub const ES_CF_REPLACE: u32 = 1 << 2;

pub const ES_CFG_KNOWN_FLAGS: u32 = 0b111;
pub const ES_SB_QUERY_RESUME_STATE: u32 = 1 << 0;

pub const ES_CARD_UNITS: i32 = 0;
pub const ES_CARD_WHOLE_FILE: i32 = 1;

pub const ES_RESOLVE_KEEP_LOCAL: i32 = 0;
pub const ES_RESOLVE_TAKE_REMOTE: i32 = 1;

pub const ES_EV_SYNC_STARTED: i32 = 1;
pub const ES_EV_SYNC_FINISHED: i32 = 2;
pub const ES_EV_UNIT_UPLOADED: i32 = 4;
pub const ES_EV_UNIT_APPLIED: i32 = 5;
pub const ES_EV_APPLY_BLOCKED: i32 = 6;
pub const ES_EV_CARD_MODE_CHANGED: i32 = 7;
pub const ES_EV_CONFLICT: i32 = 8;
pub const ES_EV_AUTH_REQUIRED: i32 = 12;
pub const ES_EV_ERROR: i32 = 13;

pub type es_log_fn = Option<unsafe extern "C" fn(*mut c_void, c_int, *const c_char, *const c_char)>;
pub type es_wake_fn = Option<unsafe extern "C" fn(*mut c_void)>;
pub type es_credential_get_fn = Option<unsafe extern "C" fn(*mut c_void, *const c_char, *mut es_credentials) -> c_int>;
pub type es_version_cb = Option<unsafe extern "C" fn(*mut c_void, *const es_version_info)>;

#[repr(C)]
pub struct es_config {
    pub struct_size: u32,
    pub data_dir: *const c_char,
    pub device_name: *const c_char,
    pub emulator_id: *const c_char,
    pub emulator_version: *const c_char,
    pub vfs: *const es_vfs,
    pub conflict_policy: c_int,
    pub flags: u32,
    pub snapshot_budget_bytes: u64,
    pub log: es_log_fn,
    pub log_user: *mut c_void,
    pub log_level: c_int,
}

#[repr(C)]
pub struct es_credentials {
    pub struct_size: u32,
    pub kind: c_int,
    pub access_key_id: [c_char; 128],
    pub secret_access_key: [c_char; 128],
    pub session_token: [c_char; 4096],
    pub expires_unix_ms: i64,
    pub bearer_token: [c_char; 4096],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct es_vfs_stat {
    pub size: u64,
    pub mtime_unix_ms: i64,
    pub is_dir: u8,
}

pub type es_vfs_entry_fn = Option<unsafe extern "C" fn(*mut c_void, *const c_char, *const es_vfs_stat)>;
pub type es_vfs_sink_fn = Option<unsafe extern "C" fn(*mut c_void, *const c_void, usize)>;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct es_vfs {
    pub struct_size: u32,
    pub user: *mut c_void,
    pub stat: Option<unsafe extern "C" fn(*mut c_void, *const c_char, *mut es_vfs_stat) -> c_int>,
    pub list: Option<unsafe extern "C" fn(*mut c_void, *const c_char, es_vfs_entry_fn, *mut c_void) -> c_int>,
    pub read: Option<unsafe extern "C" fn(*mut c_void, *const c_char, es_vfs_sink_fn, *mut c_void) -> c_int>,
    pub write: Option<unsafe extern "C" fn(*mut c_void, *const c_char, *const c_void, usize) -> c_int>,
    pub mkdir: Option<unsafe extern "C" fn(*mut c_void, *const c_char) -> c_int>,
    pub rename: Option<unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int>,
    pub remove: Option<unsafe extern "C" fn(*mut c_void, *const c_char) -> c_int>,
    pub set_mtime: Option<unsafe extern "C" fn(*mut c_void, *const c_char, i64) -> c_int>,
    pub join: Option<unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, *mut c_char, usize) -> usize>,
    pub parent: Option<unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_char, usize) -> usize>,
    pub name: Option<unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_char, usize) -> usize>,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct es_credential_provider {
    pub user: *mut c_void,
    pub get: es_credential_get_fn,
}

#[repr(C)]
pub struct es_container_desc {
    pub struct_size: u32,
    pub format: *const c_char,
    pub location: *const c_char,
    pub platform: *const c_char,
    pub card: *const c_char,
    pub title_scope: *const c_char,
    pub flags: u32,
}

#[repr(C)]
pub struct es_title {
    pub struct_size: u32,
    pub platform: *const c_char,
    pub serial: *const c_char,
    pub content_hash: *const c_char,
    pub display_name: *const c_char,
}

#[repr(C)]
pub struct es_session_begin_args {
    pub struct_size: u32,
    pub title: *const es_title,
    pub mounted: *const es_container_id,
    pub mounted_count: usize,
    pub timeout_ms: u32,
    pub flags: u32,
    pub state_compat_key: *const c_char,
}

#[repr(C)]
pub struct es_session_info {
    pub struct_size: u32,
    pub id: es_session_id,
    pub units_applied: u32,
    pub units_blocked: u32,
    pub conflicts: u32,
    pub remote_reached: u8,
    pub other_device_active: [c_char; 64],
    pub resume_state_id: [c_char; 72],
    pub resume_state_unix_ms: i64,
}

#[repr(C)]
pub struct es_state_desc {
    pub struct_size: u32,
    pub title: *const es_title,
    pub path: *const c_char,
    pub compat_key: *const c_char,
    pub slot: *const c_char,
    pub screenshot_path: *const c_char,
}

#[repr(C)]
pub struct es_state_info {
    pub struct_size: u32,
    pub id: [c_char; 72],
    pub device_name: [c_char; 64],
    pub created_unix_ms: i64,
    pub size: u64,
    pub stale_vs_saves: u8,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct es_version_info {
    pub version_id: [c_char; 72],
    pub device_name: [c_char; 64],
    pub modified_unix_ms: i64,
    pub playtime_ms: u64,
    pub size: u64,
    pub is_tombstone: u8,
}

#[repr(C)]
pub struct es_conflict_info {
    pub struct_size: u32,
    pub container: es_container_id,
    pub unit_id: [c_char; 128],
    pub unit_name: [c_char; 64],
    pub unit_title: [c_char; 128],
    pub title_key: [c_char; 64],
    pub local: es_version_info,
    pub remote: [es_version_info; 4],
    pub remote_count: u32,
}

#[repr(C)]
pub struct es_event {
    pub struct_size: u32,
    pub type_: c_int,
    pub status: c_int,
    pub container: es_container_id,
    pub conflict_id: es_conflict_id,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub title_key: *const c_char,
    pub unit_id: *const c_char,
    pub unit_name: *const c_char,
    pub device_name: *const c_char,
    pub message: *const c_char,
    pub _internal: *mut c_void,
}

/// Bytes of a struct the host must provide (its v1 size): `struct_size` may be larger when a
/// newer header appended fields, never smaller.
pub fn size_ok<T>(struct_size: u32) -> bool {
    struct_size as usize >= std::mem::size_of::<T>()
}

/// Copies `s` into a fixed C buffer, truncated at a character boundary and NUL-terminated.
pub fn copy_str(dst: &mut [c_char], s: &str) {
    let Some(cap) = dst.len().checked_sub(1) else { return };
    let mut end = s.len().min(cap);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    for (d, b) in dst.iter_mut().zip(s.as_bytes()[..end].iter()) {
        *d = *b as c_char;
    }
    dst[end] = 0;
}

/// Reads a NUL-terminated UTF-8 string from a fixed buffer; `None` if it has no NUL or isn't
/// UTF-8.
pub fn read_buf(src: &[c_char]) -> Option<String> {
    let bytes: Vec<u8> = src.iter().map(|c| *c as u8).take_while(|b| *b != 0).collect();
    if bytes.len() == src.len() {
        return None;
    }
    String::from_utf8(bytes).ok()
}

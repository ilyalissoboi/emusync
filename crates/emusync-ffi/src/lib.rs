//! C ABI for emusync (`include/emusync.h`).
//!
//! M0 stub: only the version and status functions are exported. The client, container,
//! session and event APIs land with the ABI freeze in M2 (DESIGN §14).

use std::ffi::c_char;

pub const ES_ABI_VERSION: u32 = 1;

/// Mirrors `es_status` in emusync.h.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EsStatus {
    Ok = 0,
    Pending = 1,
    Timeout = 2,
    Offline = 3,
    Conflict = 4,
    InvalidArg = -1,
    Io = -2,
    Format = -3,
    NoSpace = -4,
    Busy = -5,
    Auth = -6,
    Quota = -7,
    Incompatible = -8,
    NotFound = -9,
    Abi = -10,
    Internal = -100,
}

#[no_mangle]
pub extern "C" fn es_abi_version() -> u32 {
    ES_ABI_VERSION
}

#[no_mangle]
pub extern "C" fn es_version_string() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), " (abi 1)\0").as_ptr().cast()
}

/// Static description of a status code. Takes the raw integer so unknown values are safe.
#[no_mangle]
pub extern "C" fn es_status_str(s: i32) -> *const c_char {
    let text: &'static str = match s {
        0 => "ok\0",
        1 => "pending\0",
        2 => "timeout\0",
        3 => "offline\0",
        4 => "conflict\0",
        -1 => "invalid argument\0",
        -2 => "i/o error\0",
        -3 => "invalid container format\0",
        -4 => "no space in container\0",
        -5 => "container busy\0",
        -6 => "authentication required\0",
        -7 => "quota exceeded\0",
        -8 => "incompatible\0",
        -9 => "not found\0",
        -10 => "ABI mismatch\0",
        -100 => "internal error\0",
        _ => "unknown status\0",
    };
    text.as_ptr().cast()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    #[test]
    fn strings() {
        assert_eq!(es_abi_version(), 1);
        let v = unsafe { CStr::from_ptr(es_version_string()) }.to_str().unwrap();
        assert!(v.starts_with(env!("CARGO_PKG_VERSION")));
        let s = unsafe { CStr::from_ptr(es_status_str(EsStatus::NoSpace as i32)) };
        assert_eq!(s.to_str().unwrap(), "no space in container");
        let s = unsafe { CStr::from_ptr(es_status_str(12345)) };
        assert_eq!(s.to_str().unwrap(), "unknown status");
    }
}

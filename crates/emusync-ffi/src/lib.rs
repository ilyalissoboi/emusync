//! C ABI for emusync (`include/emusync.h`, DESIGN §3.2, §6.4, §10, §12).
//!
//! Every exported function checks its pointers and `struct_size` fields, takes UTF-8 strings,
//! and catches panics, so nothing unwinds into the host. The client itself lives in
//! [`client`]: a worker thread that owns the engine, fed by a command queue.

pub mod abi;
mod api;
mod callbacks;
mod client;
mod creds;
mod events;
#[cfg(test)]
mod tests;

pub use api::*;
pub use client::Client;

use abi::*;
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
    Locked = -11,
    Passphrase = -12,
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
        -11 => "repository locked (passphrase needed)\0",
        -12 => "wrong passphrase\0",
        -100 => "internal error\0",
        _ => "unknown status\0",
    };
    text.as_ptr().cast()
}

/// The `es_status` for an engine error.
pub(crate) fn status_of(e: &emusync_core::Error) -> i32 {
    use emusync_core::Error;
    use emusync_remote::RemoteError;
    match e {
        Error::Remote(RemoteError::Network(_)) | Error::NoRemote => ES_OFFLINE,
        Error::Remote(RemoteError::Auth(_)) => ES_E_AUTH,
        Error::Remote(RemoteError::InvalidUri(_)) => ES_E_INVALID_ARG,
        Error::Remote(_) => ES_E_IO,
        Error::Io(io) if io.kind() == std::io::ErrorKind::NotFound => ES_E_NOT_FOUND,
        Error::Io(_) | Error::Db(_) | Error::Integrity(_) => ES_E_IO,
        Error::Format(_) => ES_E_FORMAT,
        Error::NotFound(_) => ES_E_NOT_FOUND,
        Error::InvalidArg(_) => ES_E_INVALID_ARG,
        Error::Locked => ES_E_LOCKED,
        Error::WrongPassphrase => ES_E_PASSPHRASE,
        Error::Incompatible(_) => ES_E_INCOMPATIBLE,
        Error::Json(_) => ES_E_INTERNAL,
    }
}

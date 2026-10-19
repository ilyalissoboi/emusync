//! `es_credential_provider` → [`emusync_remote::CallbackCredentials`].

use crate::abi::{self, es_credential_provider, es_credentials};
use crate::callbacks::{Callbacks, HostPtr};
use emusync_remote::{CallbackCredentials, ExpiringCredentials, RemoteError, S3Credentials};
use std::ffi::CString;
use std::sync::Arc;

/// Credentials for `uri` from the host's provider. The callback runs on whichever library
/// thread needs them (the worker, or the thread that sets up the remote).
pub(crate) fn provider(p: es_credential_provider, uri: &str, cb: Arc<Callbacks>) -> CallbackCredentials {
    let get = p.get;
    let user = HostPtr(p.user);
    let uri = crate::callbacks::cstring_lossy(uri);
    CallbackCredentials::new(move || fetch(get, user, &uri, &cb))
}

fn fetch(
    get: abi::es_credential_get_fn,
    user: HostPtr,
    uri: &CString,
    cb: &Callbacks,
) -> Result<ExpiringCredentials, RemoteError> {
    let get = get.ok_or_else(|| RemoteError::Auth("no credential provider".into()))?;
    // ~8 KiB: on the heap, zeroed, and wiped again before it is freed.
    let mut buf: Box<es_credentials> = Box::new(unsafe { std::mem::zeroed() });
    buf.struct_size = std::mem::size_of::<es_credentials>() as u32;
    // SAFETY: a host callback, given a valid, writable es_credentials of the size it states.
    let rc = cb.guarded(|| unsafe { get(user.0, uri.as_ptr(), &mut *buf) });
    let out = match rc {
        None => Err(RemoteError::Auth("the client was destroyed".into())),
        Some(0) => parse(&buf),
        Some(abi::ES_E_AUTH) => Err(RemoteError::Auth("the credential provider needs the user to sign in".into())),
        Some(rc) => Err(RemoteError::Auth(format!("the credential provider failed ({rc})"))),
    };
    wipe(&mut buf);
    out
}

fn parse(c: &es_credentials) -> Result<ExpiringCredentials, RemoteError> {
    let bad =
        |what: &str| RemoteError::Auth(format!("credential provider: {what} is not a NUL-terminated UTF-8 string"));
    match c.kind {
        abi::ES_CRED_S3 => {
            let access_key_id = abi::read_buf(&c.access_key_id).ok_or_else(|| bad("access_key_id"))?;
            let secret_access_key = abi::read_buf(&c.secret_access_key).ok_or_else(|| bad("secret_access_key"))?;
            let token = abi::read_buf(&c.session_token).ok_or_else(|| bad("session_token"))?;
            if access_key_id.is_empty() || secret_access_key.is_empty() {
                return Err(RemoteError::Auth("credential provider returned empty S3 keys".into()));
            }
            Ok(ExpiringCredentials {
                creds: S3Credentials {
                    access_key_id,
                    secret_access_key,
                    session_token: (!token.is_empty()).then_some(token),
                },
                expires_unix_ms: (c.expires_unix_ms != 0).then_some(c.expires_unix_ms),
            })
        }
        abi::ES_CRED_NONE => Err(RemoteError::Auth("S3 remotes need ES_CRED_S3 credentials".into())),
        k => Err(RemoteError::Auth(format!("credential kind {k} is not supported by S3 remotes"))),
    }
}

fn wipe(c: &mut es_credentials) {
    let p = c as *mut es_credentials as *mut u8;
    for i in 0..std::mem::size_of::<es_credentials>() {
        // SAFETY: within the struct; volatile so the wipe isn't optimised away.
        unsafe { std::ptr::write_volatile(p.add(i), 0) };
    }
}

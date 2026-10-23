//! The host's `es_vfs` (DESIGN §28) as the engine's [`Vfs`]: every container access becomes a
//! call into the host, which keeps the containers where emusync can't reach them by path
//! (Android's SAF).

use crate::abi::*;
use emusync_core::vfs::{self, Stat, Vfs};
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::io;

/// A copy of the host's `es_vfs`, checked by [`HostVfs::new`].
pub struct HostVfs(es_vfs);

// The host promises its functions may be called from any of emusync's threads (emusync.h).
unsafe impl Send for HostVfs {}
unsafe impl Sync for HostVfs {}

impl std::fmt::Debug for HostVfs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HostVfs")
    }
}

fn c(s: &str) -> io::Result<CString> {
    CString::new(s).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, format!("{s:?} contains NUL")))
}

fn check(status: c_int, what: &str, loc: &str) -> io::Result<()> {
    match status {
        ES_OK => Ok(()),
        ES_E_NOT_FOUND => Err(io::Error::new(io::ErrorKind::NotFound, format!("{loc} does not exist"))),
        ES_E_BUSY => Err(vfs::in_use()),
        s => Err(io::Error::other(format!("the host's storage failed to {what} {loc} (status {s})"))),
    }
}

fn stat_of(st: &es_vfs_stat) -> Stat {
    Stat { is_dir: st.is_dir != 0, size: st.size, mtime_ns: st.mtime_unix_ms.saturating_mul(1_000_000) }
}

unsafe extern "C" fn on_entry(ctx: *mut c_void, name: *const c_char, st: *const es_vfs_stat) {
    if ctx.is_null() || name.is_null() || st.is_null() {
        return;
    }
    let out = &mut *(ctx as *mut Vec<(String, Stat)>);
    out.push((CStr::from_ptr(name).to_string_lossy().into_owned(), stat_of(&*st)));
}

unsafe extern "C" fn on_data(ctx: *mut c_void, data: *const c_void, len: usize) {
    if ctx.is_null() || (data.is_null() && len > 0) {
        return;
    }
    let out = &mut *(ctx as *mut Vec<u8>);
    if len > 0 {
        out.extend_from_slice(std::slice::from_raw_parts(data as *const u8, len));
    }
}

impl HostVfs {
    /// Checks a host's `es_vfs`: every function but `set_mtime` is required.
    pub fn new(v: &es_vfs) -> Result<Self, String> {
        if (v.struct_size as usize) < std::mem::size_of::<es_vfs>() {
            return Err(format!("es_vfs.struct_size {} is too small", v.struct_size));
        }
        let required = [
            ("stat", v.stat.is_some()),
            ("list", v.list.is_some()),
            ("read", v.read.is_some()),
            ("write", v.write.is_some()),
            ("mkdir", v.mkdir.is_some()),
            ("rename", v.rename.is_some()),
            ("remove", v.remove.is_some()),
            ("join", v.join.is_some()),
            ("parent", v.parent.is_some()),
            ("name", v.name.is_some()),
        ];
        if let Some((name, _)) = required.iter().find(|(_, set)| !set) {
            return Err(format!("es_vfs.{name} is required"));
        }
        Ok(HostVfs(*v))
    }

    /// Calls a function that writes a string into a buffer and returns its length (0: none).
    fn string(&self, f: impl Fn(*mut c_char, usize) -> usize) -> Option<String> {
        let mut cap = 1024;
        loop {
            let mut buf = vec![0 as c_char; cap];
            let n = f(buf.as_mut_ptr(), cap);
            if n == 0 {
                return None;
            }
            if n < cap {
                let bytes: Vec<u8> = buf[..n].iter().map(|&b| b as u8).collect();
                return Some(String::from_utf8_lossy(&bytes).into_owned());
            }
            cap = n + 1;
        }
    }
}

impl Vfs for HostVfs {
    fn stat(&self, loc: &str) -> io::Result<Stat> {
        let p = c(loc)?;
        let mut st = es_vfs_stat::default();
        check(unsafe { self.0.stat.unwrap()(self.0.user, p.as_ptr(), &mut st) }, "read", loc)?;
        Ok(stat_of(&st))
    }

    fn list(&self, dir: &str) -> io::Result<Vec<(String, Stat)>> {
        let p = c(dir)?;
        let mut out: Vec<(String, Stat)> = Vec::new();
        let ctx = &mut out as *mut Vec<(String, Stat)> as *mut c_void;
        check(unsafe { self.0.list.unwrap()(self.0.user, p.as_ptr(), Some(on_entry), ctx) }, "list", dir)?;
        Ok(out)
    }

    fn read(&self, loc: &str) -> io::Result<Vec<u8>> {
        let p = c(loc)?;
        let mut out: Vec<u8> = Vec::new();
        let ctx = &mut out as *mut Vec<u8> as *mut c_void;
        check(unsafe { self.0.read.unwrap()(self.0.user, p.as_ptr(), Some(on_data), ctx) }, "read", loc)?;
        Ok(out)
    }

    fn write(&self, loc: &str, data: &[u8]) -> io::Result<()> {
        let p = c(loc)?;
        let s = unsafe { self.0.write.unwrap()(self.0.user, p.as_ptr(), data.as_ptr() as *const c_void, data.len()) };
        check(s, "write", loc)
    }

    fn create_dir(&self, loc: &str) -> io::Result<()> {
        let p = c(loc)?;
        check(unsafe { self.0.mkdir.unwrap()(self.0.user, p.as_ptr()) }, "create", loc)
    }

    fn rename(&self, loc: &str, new_name: &str) -> io::Result<()> {
        let (p, n) = (c(loc)?, c(new_name)?);
        check(unsafe { self.0.rename.unwrap()(self.0.user, p.as_ptr(), n.as_ptr()) }, "rename", loc)
    }

    fn remove(&self, loc: &str) -> io::Result<()> {
        let p = c(loc)?;
        check(unsafe { self.0.remove.unwrap()(self.0.user, p.as_ptr()) }, "remove", loc)
    }

    fn set_mtime(&self, loc: &str, secs: i64) -> io::Result<()> {
        let Some(f) = self.0.set_mtime else {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "the host's storage can't set times"));
        };
        let p = c(loc)?;
        check(unsafe { f(self.0.user, p.as_ptr(), secs.saturating_mul(1000)) }, "set the time of", loc)
    }

    fn join(&self, dir: &str, name: &str) -> String {
        let (Ok(d), Ok(n)) = (c(dir), c(name)) else { return format!("{dir}/{name}") };
        self.string(|buf, cap| unsafe { self.0.join.unwrap()(self.0.user, d.as_ptr(), n.as_ptr(), buf, cap) })
            .unwrap_or_else(|| format!("{dir}/{name}"))
    }

    fn parent(&self, loc: &str) -> Option<String> {
        let p = c(loc).ok()?;
        self.string(|buf, cap| unsafe { self.0.parent.unwrap()(self.0.user, p.as_ptr(), buf, cap) })
    }

    fn name(&self, loc: &str) -> Option<String> {
        let p = c(loc).ok()?;
        self.string(|buf, cap| unsafe { self.0.name.unwrap()(self.0.user, p.as_ptr(), buf, cap) })
    }
}

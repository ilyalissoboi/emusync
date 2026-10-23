//! Where containers live (DESIGN §28): the native file system ([`NativeVfs`], the default), or a
//! host's own storage through its callbacks, such as Android's Storage Access Framework, whose
//! `content://` documents have no file paths (`es_vfs` in the C ABI).
//!
//! The engine reaches every container through a [`Vfs`]; its own data directory is always
//! native. A location is a string the VFS gives meaning to, a native path or a URI, and only
//! the VFS composes them ([`Vfs::join`], [`Vfs::parent`], [`Vfs::name`]).
//!
//! A VFS supplies a few primitives. Reading and replacing whole directories and files are built
//! on them ([`read_tree`], [`replace_dir`], [`replace_file`], [`recover`], [`remove_dir`]), with
//! the journalled renames of DESIGN §19.3 for files too, since SAF can't rename over a file.
//! [`NativeVfs`] keeps its own versions: atomic `rename` over files, fsyncs, Windows locks.

use crate::fsutil;
use emusync_formats::tree::{self, EntryKind, Tree};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// A file's or directory's metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    pub is_dir: bool,
    /// Bytes, for a file.
    pub size: u64,
    /// Modification time in ns since the Unix epoch, as precise as the storage keeps it.
    pub mtime_ns: i64,
}

impl Stat {
    /// The modification time in whole seconds (rounded down), as tree images keep it.
    pub fn mtime_secs(&self) -> i64 {
        self.mtime_ns.div_euclid(1_000_000_000)
    }
}

/// Storage for containers. Every call may block; errors of kind `NotFound` mean the location
/// doesn't exist, and the engine relies on telling them apart.
pub trait Vfs: Send + Sync + std::fmt::Debug {
    /// Whether locations are native paths (made absolute when a container is added).
    fn is_native(&self) -> bool {
        false
    }
    fn stat(&self, loc: &str) -> io::Result<Stat>;
    /// The entries of a directory, with their metadata, in any order.
    fn list(&self, dir: &str) -> io::Result<Vec<(String, Stat)>>;
    fn read(&self, loc: &str) -> io::Result<Vec<u8>>;
    /// Creates or truncates the file and writes `data`; durable once it returns.
    fn write(&self, loc: &str, data: &[u8]) -> io::Result<()>;
    fn create_dir(&self, loc: &str) -> io::Result<()>;
    /// Renames within its directory. Nothing may be at the new name.
    fn rename(&self, loc: &str, new_name: &str) -> io::Result<()>;
    /// Removes a file, or a directory with everything in it.
    fn remove(&self, loc: &str) -> io::Result<()>;
    /// Sets a modification time, in seconds. Storage that can't (SAF) returns `Unsupported`;
    /// callers treat it as best effort.
    fn set_mtime(&self, loc: &str, secs: i64) -> io::Result<()>;
    /// The location of `name` in the directory `dir`.
    fn join(&self, dir: &str, name: &str) -> String;
    /// The directory a location is in, if the VFS can reach it.
    fn parent(&self, loc: &str) -> Option<String>;
    /// A location's own name (`Mcd001.ps2`, `SAVEDATA`).
    fn name(&self, loc: &str) -> Option<String>;

    /// Everything under the directory `dir`, as a tree image (DESIGN §19.3).
    fn read_tree(&self, dir: &str) -> io::Result<Tree> {
        read_tree_with(self, dir)
    }
    /// Replaces (or creates) the directory `dir` with `tree`, crash-safely.
    fn replace_dir(&self, dir: &str, tree: &Tree) -> io::Result<()> {
        swap_in(self, dir, |tmp| write_tree(self, tmp, tree))
    }
    /// Replaces (or creates) the file `loc` with `data`, crash-safely.
    fn replace_file(&self, loc: &str, data: &[u8]) -> io::Result<()> {
        swap_in(self, loc, |tmp| self.write(tmp, data))
    }
    /// Finishes or discards an interrupted replace or removal of `loc`.
    fn recover(&self, loc: &str) -> io::Result<()> {
        recover_with(self, loc)
    }
    /// Removes the directory `dir` as one step.
    fn remove_dir(&self, dir: &str) -> io::Result<()> {
        self.recover(dir)?;
        self.rename(dir, &scratch_name(self, dir, OLD)?)?;
        self.remove(&scratch(self, dir, OLD)?)
    }
}

/// The error a VFS returns for a file another program has open and keeps others out of (as
/// PCSX2 keeps a card file on Windows): [`crate::fsutil::is_locked`] recognises it, and the
/// card waits for the next sync instead of failing.
#[derive(Debug)]
pub struct InUse;

impl std::fmt::Display for InUse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("in use by another program")
    }
}

impl std::error::Error for InUse {}

/// An [`InUse`] error.
pub fn in_use() -> io::Error {
    io::Error::other(InUse)
}

const TMP: &str = ".emusync-tmp";
const OLD: &str = ".emusync-old";
const SWAP: &str = ".emusync-swap";

/// Whether `name` is one of emusync's scratch siblings (`<name>.emusync-tmp`, `-old`, `-swap`),
/// never a container or a save.
pub fn is_scratch_name(name: &str) -> bool {
    [TMP, OLD, SWAP].iter().any(|s| name.ends_with(s))
}

/// The container `name` a scratch name belongs to (`A.emusync-old` → `A`).
pub fn scratch_owner(name: &str) -> Option<&str> {
    [TMP, OLD, SWAP].iter().find_map(|s| name.strip_suffix(s))
}

fn no_parent(loc: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, format!("{loc} has no parent folder emusync can reach"))
}

fn scratch_name<V: Vfs + ?Sized>(v: &V, loc: &str, suffix: &str) -> io::Result<String> {
    Ok(format!("{}{suffix}", v.name(loc).ok_or_else(|| no_parent(loc))?))
}

fn scratch<V: Vfs + ?Sized>(v: &V, loc: &str, suffix: &str) -> io::Result<String> {
    Ok(v.join(&v.parent(loc).ok_or_else(|| no_parent(loc))?, &scratch_name(v, loc, suffix)?))
}

/// Whether `loc` exists.
pub fn exists<V: Vfs + ?Sized>(v: &V, loc: &str) -> io::Result<bool> {
    match v.stat(loc) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

fn invalid_data(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

fn read_tree_with<V: Vfs + ?Sized>(v: &V, dir: &str) -> io::Result<Tree> {
    fn walk<V: Vfs + ?Sized>(v: &V, tree: &mut Tree, dir: &str, rel: &str, depth: usize) -> io::Result<()> {
        if depth > tree::MAX_DEPTH {
            return Err(invalid_data(format!("{dir} is nested too deeply")));
        }
        let mut entries = v.list(dir)?;
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, st) in entries {
            if !tree::valid_segment(&name) {
                return Err(invalid_data(format!("unsupported file name {name:?} in {dir}")));
            }
            let (loc, child) = (v.join(dir, &name), tree::join(rel, &name));
            let bad = |e: emusync_formats::FormatError| invalid_data(e.to_string());
            if st.is_dir {
                tree.insert(child.clone(), st.mtime_secs(), EntryKind::Dir).map_err(bad)?;
                walk(v, tree, &loc, &child, depth + 1)?;
            } else {
                tree.insert(child, st.mtime_secs(), EntryKind::File(v.read(&loc)?)).map_err(bad)?;
            }
        }
        Ok(())
    }
    if !v.stat(dir)?.is_dir {
        return Err(invalid_data(format!("{dir} is not a directory")));
    }
    let mut tree = Tree::default();
    walk(v, &mut tree, dir, "", 1)?;
    Ok(tree)
}

/// Writes `tree` into the new directory `dir`, setting modification times where the storage
/// can (directories last, after what is in them).
fn write_tree<V: Vfs + ?Sized>(v: &V, dir: &str, tree: &Tree) -> io::Result<()> {
    v.create_dir(dir)?;
    let mut dirs = Vec::new();
    for (rel, e) in &tree.entries {
        let loc = rel.split('/').fold(dir.to_string(), |d, s| v.join(&d, s));
        match &e.kind {
            EntryKind::Dir => {
                v.create_dir(&loc)?;
                dirs.push((loc, e.mtime));
            }
            EntryKind::File(data) => {
                v.write(&loc, data)?;
                let _ = v.set_mtime(&loc, e.mtime);
            }
        }
    }
    for (d, mtime) in dirs.iter().rev() {
        let _ = v.set_mtime(d, *mtime);
    }
    Ok(())
}

/// The replace protocol of DESIGN §19.3, for a file or a directory: write the new one to
/// `<name>.emusync-tmp`, write the journal `<name>.emusync-swap`, rename `<name>` to
/// `<name>.emusync-old` and the new one to `<name>`, remove the old one, then the journal.
fn swap_in<V: Vfs + ?Sized>(v: &V, loc: &str, write_new: impl FnOnce(&str) -> io::Result<()>) -> io::Result<()> {
    v.recover(loc)?;
    let name = v.name(loc).ok_or_else(|| no_parent(loc))?;
    let (tmp, old, journal) = (scratch(v, loc, TMP)?, scratch(v, loc, OLD)?, scratch(v, loc, SWAP)?);
    if exists(v, &old)? {
        v.remove(&old)?;
    }
    if let Err(e) = write_new(&tmp) {
        let _ = v.remove(&tmp);
        return Err(e);
    }
    if let Err(e) = v.write(&journal, b"emusync replace\n") {
        let _ = v.remove(&tmp);
        return Err(e);
    }
    if let Err(e) = v.rename(loc, &format!("{name}{OLD}")) {
        if e.kind() != io::ErrorKind::NotFound {
            let _ = v.remove(&journal);
            let _ = v.remove(&tmp);
            return Err(e);
        }
    }
    if let Err(e) = v.rename(&tmp, &name) {
        if v.rename(&old, &name).is_ok() {
            let _ = v.remove(&journal);
            let _ = v.remove(&tmp);
        }
        // Otherwise the journal stays, and the next recovery completes the replace.
        return Err(e);
    }
    if exists(v, &old)? {
        v.remove(&old)?;
    }
    v.remove(&journal)
}

fn recover_with<V: Vfs + ?Sized>(v: &V, loc: &str) -> io::Result<()> {
    let Some(name) = v.name(loc) else { return Ok(()) };
    if v.parent(loc).is_none() {
        return Ok(());
    }
    let (tmp, old, journal) = (scratch(v, loc, TMP)?, scratch(v, loc, OLD)?, scratch(v, loc, SWAP)?);
    if !exists(v, &journal)? {
        // An incomplete new one, or what a removal left.
        for s in [&tmp, &old] {
            if exists(v, s)? {
                v.remove(s)?;
            }
        }
        return Ok(());
    }
    if exists(v, &tmp)? {
        if exists(v, loc)? {
            if exists(v, &old)? {
                // Not a state the protocol leaves behind: keep the live one.
                v.remove(&tmp)?;
                return v.remove(&journal);
            }
            v.rename(loc, &format!("{name}{OLD}"))?;
        }
        v.rename(&tmp, &name)?;
    }
    if exists(v, &old)? {
        v.remove(&old)?;
    }
    v.remove(&journal)
}

fn ns_since_epoch(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i64,
        Err(e) => -(e.duration().as_nanos() as i64),
    }
}

/// The native file system. Locations are paths; symbolic links are followed.
#[derive(Debug)]
pub struct NativeVfs;

impl NativeVfs {
    fn stat_of(meta: &fs::Metadata) -> Stat {
        let mtime_ns = meta.modified().map(ns_since_epoch).unwrap_or(0);
        Stat { is_dir: meta.is_dir(), size: if meta.is_dir() { 0 } else { meta.len() }, mtime_ns }
    }
}

impl Vfs for NativeVfs {
    fn is_native(&self) -> bool {
        true
    }

    fn stat(&self, loc: &str) -> io::Result<Stat> {
        Ok(Self::stat_of(&fs::metadata(loc)?))
    }

    fn list(&self, dir: &str) -> io::Result<Vec<(String, Stat)>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let Ok(name) = entry.file_name().into_string() else {
                log::warn!("{dir}: skipping an entry whose name isn't UTF-8");
                continue;
            };
            out.push((name, Self::stat_of(&fs::metadata(entry.path())?)));
        }
        Ok(out)
    }

    fn read(&self, loc: &str) -> io::Result<Vec<u8>> {
        fs::read(loc)
    }

    fn write(&self, loc: &str, data: &[u8]) -> io::Result<()> {
        use std::io::Write;
        let mut f = fs::File::create(loc)?;
        f.write_all(data)?;
        f.sync_all()
    }

    fn create_dir(&self, loc: &str) -> io::Result<()> {
        fs::create_dir(loc)
    }

    fn rename(&self, loc: &str, new_name: &str) -> io::Result<()> {
        let to = Path::new(loc).with_file_name(new_name);
        if to.exists() {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("{} exists", to.display())));
        }
        fs::rename(loc, to)
    }

    fn remove(&self, loc: &str) -> io::Result<()> {
        if fs::symlink_metadata(loc)?.is_dir() {
            fs::remove_dir_all(loc)
        } else {
            fs::remove_file(loc)
        }
    }

    fn set_mtime(&self, loc: &str, secs: i64) -> io::Result<()> {
        if Path::new(loc).is_dir() {
            fsutil::set_dir_mtime(Path::new(loc), secs);
            Ok(())
        } else {
            fs::File::options().write(true).open(loc)?.set_modified(fsutil::system_time(secs))
        }
    }

    fn join(&self, dir: &str, name: &str) -> String {
        Path::new(dir).join(name).to_string_lossy().into_owned()
    }

    fn parent(&self, loc: &str) -> Option<String> {
        Path::new(loc).parent().map(|p| p.to_string_lossy().into_owned())
    }

    fn name(&self, loc: &str) -> Option<String> {
        Path::new(loc).file_name().map(|n| n.to_string_lossy().into_owned())
    }

    fn read_tree(&self, dir: &str) -> io::Result<Tree> {
        fsutil::read_tree(Path::new(dir))
    }

    fn replace_dir(&self, dir: &str, tree: &Tree) -> io::Result<()> {
        fsutil::replace_dir(Path::new(dir), tree)
    }

    fn replace_file(&self, loc: &str, data: &[u8]) -> io::Result<()> {
        fsutil::atomic_replace(Path::new(loc), data)
    }

    fn recover(&self, loc: &str) -> io::Result<()> {
        fsutil::recover_dir_replace(Path::new(loc))
    }

    fn remove_dir(&self, dir: &str) -> io::Result<()> {
        fsutil::remove_dir(Path::new(dir))
    }
}

/// Path → contents (`None` for a directory) and modification time in ns.
type Nodes = BTreeMap<String, (Option<Vec<u8>>, i64)>;

/// Storage held in memory, for tests: locations are `/`-separated paths, and, as with SAF,
/// modification times can't be set (each write takes the time of the write) and nothing is
/// renamed over.
#[derive(Debug, Default)]
pub struct MemoryVfs {
    nodes: Mutex<Nodes>,
}

impl MemoryVfs {
    /// A VFS holding the empty directory `root` (and its parents).
    pub fn with_root(root: &str) -> Self {
        let v = MemoryVfs::default();
        let mut path = String::new();
        for seg in root.split('/').filter(|s| !s.is_empty()) {
            path = if path.is_empty() { format!("/{seg}") } else { format!("{path}/{seg}") };
            v.nodes.lock().unwrap().insert(path.clone(), (None, now_ns()));
        }
        v
    }
}

fn now_ns() -> i64 {
    ns_since_epoch(SystemTime::now())
}

fn not_found(loc: &str) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, format!("{loc} does not exist"))
}

impl MemoryVfs {
    fn need_dir(nodes: &Nodes, loc: &str) -> io::Result<()> {
        match nodes.get(loc) {
            Some((None, _)) => Ok(()),
            Some(_) => Err(io::Error::other(format!("{loc} is not a directory"))),
            None => Err(not_found(loc)),
        }
    }

    fn parent_of(loc: &str) -> &str {
        loc.rsplit_once('/').map_or("", |(p, _)| p)
    }
}

impl Vfs for MemoryVfs {
    fn stat(&self, loc: &str) -> io::Result<Stat> {
        let nodes = self.nodes.lock().unwrap();
        let (data, mtime_ns) = nodes.get(loc).ok_or_else(|| not_found(loc))?;
        Ok(Stat { is_dir: data.is_none(), size: data.as_ref().map_or(0, |d| d.len() as u64), mtime_ns: *mtime_ns })
    }

    fn list(&self, dir: &str) -> io::Result<Vec<(String, Stat)>> {
        let nodes = self.nodes.lock().unwrap();
        Self::need_dir(&nodes, dir)?;
        let prefix = format!("{dir}/");
        Ok(nodes
            .range(prefix.clone()..)
            .take_while(|(p, _)| p.starts_with(&prefix))
            .filter(|(p, _)| !p[prefix.len()..].contains('/'))
            .map(|(p, (data, mtime_ns))| {
                let st = Stat {
                    is_dir: data.is_none(),
                    size: data.as_ref().map_or(0, |d| d.len() as u64),
                    mtime_ns: *mtime_ns,
                };
                (p[prefix.len()..].to_string(), st)
            })
            .collect())
    }

    fn read(&self, loc: &str) -> io::Result<Vec<u8>> {
        match self.nodes.lock().unwrap().get(loc) {
            Some((Some(d), _)) => Ok(d.clone()),
            Some(_) => Err(io::Error::other(format!("{loc} is a directory"))),
            None => Err(not_found(loc)),
        }
    }

    fn write(&self, loc: &str, data: &[u8]) -> io::Result<()> {
        let mut nodes = self.nodes.lock().unwrap();
        Self::need_dir(&nodes, Self::parent_of(loc))?;
        if matches!(nodes.get(loc), Some((None, _))) {
            return Err(io::Error::other(format!("{loc} is a directory")));
        }
        nodes.insert(loc.to_string(), (Some(data.to_vec()), now_ns()));
        Ok(())
    }

    fn create_dir(&self, loc: &str) -> io::Result<()> {
        let mut nodes = self.nodes.lock().unwrap();
        Self::need_dir(&nodes, Self::parent_of(loc))?;
        if nodes.contains_key(loc) {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("{loc} exists")));
        }
        nodes.insert(loc.to_string(), (None, now_ns()));
        Ok(())
    }

    fn rename(&self, loc: &str, new_name: &str) -> io::Result<()> {
        let mut nodes = self.nodes.lock().unwrap();
        let to = format!("{}/{new_name}", Self::parent_of(loc));
        if !nodes.contains_key(loc) {
            return Err(not_found(loc));
        }
        if nodes.contains_key(&to) {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("{to} exists")));
        }
        let prefix = format!("{loc}/");
        let moved: Vec<String> = nodes.keys().filter(|p| *p == loc || p.starts_with(&prefix)).cloned().collect();
        for p in moved {
            let n = nodes.remove(&p).unwrap();
            nodes.insert(format!("{to}{}", &p[loc.len()..]), n);
        }
        Ok(())
    }

    fn remove(&self, loc: &str) -> io::Result<()> {
        let mut nodes = self.nodes.lock().unwrap();
        if nodes.remove(loc).is_none() {
            return Err(not_found(loc));
        }
        let prefix = format!("{loc}/");
        nodes.retain(|p, _| !p.starts_with(&prefix));
        Ok(())
    }

    fn set_mtime(&self, _loc: &str, _secs: i64) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "times can't be set"))
    }

    fn join(&self, dir: &str, name: &str) -> String {
        format!("{dir}/{name}")
    }

    fn parent(&self, loc: &str) -> Option<String> {
        loc.rsplit_once('/').map(|(p, _)| p.to_string()).filter(|p| !p.is_empty())
    }

    fn name(&self, loc: &str) -> Option<String> {
        loc.rsplit_once('/').map(|(_, n)| n.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Tree {
        let mut t = Tree::default();
        t.insert("SAVE".into(), 1_500_000_000, EntryKind::Dir).unwrap();
        t.insert("SAVE/data".into(), 1_600_000_000, EntryKind::File(b"new".to_vec())).unwrap();
        t.insert("top".into(), 1_700_000_000, EntryKind::File(vec![7; 10])).unwrap();
        t
    }

    fn contents(t: &Tree) -> Vec<(String, Option<Vec<u8>>)> {
        let data = |e: &tree::Entry| match &e.kind {
            EntryKind::File(d) => Some(d.clone()),
            EntryKind::Dir => None,
        };
        t.entries.iter().map(|(p, e)| (p.clone(), data(e))).collect()
    }

    fn leftovers(v: &MemoryVfs) -> Vec<String> {
        v.list("/m").unwrap().into_iter().map(|(n, _)| n).filter(|n| is_scratch_name(n)).collect()
    }

    #[test]
    fn directories_and_files_are_replaced() {
        let v = MemoryVfs::with_root("/m");
        v.replace_dir("/m/card", &sample()).unwrap();
        assert_eq!(contents(&v.read_tree("/m/card").unwrap()), contents(&sample()));
        let mut other = Tree::default();
        other.insert("x".into(), 0, EntryKind::File(b"x".to_vec())).unwrap();
        v.replace_dir("/m/card", &other).unwrap();
        assert_eq!(contents(&v.read_tree("/m/card").unwrap()), contents(&other));

        v.replace_file("/m/Mcd001.ps2", b"one").unwrap();
        v.replace_file("/m/Mcd001.ps2", b"two").unwrap();
        assert_eq!(v.read("/m/Mcd001.ps2").unwrap(), b"two");
        assert!(leftovers(&v).is_empty());

        v.remove_dir("/m/card").unwrap();
        assert!(!exists(&v, "/m/card").unwrap());
        assert!(leftovers(&v).is_empty());
    }

    /// The protocol interrupted after each step: discarded before the journal, rolled forward
    /// after it, for files and directories alike.
    #[test]
    fn interrupted_replaces_recover() {
        for dir in [false, true] {
            for step in 0..5 {
                let v = MemoryVfs::with_root("/m");
                let (loc, tmp, old, journal) = ("/m/c", "/m/c.emusync-tmp", "/m/c.emusync-old", "/m/c.emusync-swap");
                let put = |at: &str, what: &[u8]| {
                    if dir {
                        let mut t = Tree::default();
                        t.insert("f".into(), 0, EntryKind::File(what.to_vec())).unwrap();
                        write_tree(&v, at, &t).unwrap();
                    } else {
                        v.write(at, what).unwrap();
                    }
                };
                let get = || if dir { v.read(&format!("{loc}/f")).unwrap() } else { v.read(loc).unwrap() };
                put(loc, b"old");
                put(tmp, b"new");
                if step >= 1 {
                    v.write(journal, b"j").unwrap();
                }
                if step >= 2 {
                    v.rename(loc, "c.emusync-old").unwrap();
                }
                if step >= 3 {
                    v.rename(tmp, "c").unwrap();
                }
                if step >= 4 {
                    v.remove(old).unwrap();
                }
                v.recover(loc).unwrap();
                assert_eq!(get(), if step == 0 { b"old".to_vec() } else { b"new".to_vec() }, "dir {dir} step {step}");
                assert!(leftovers(&v).is_empty(), "dir {dir} step {step}: {:?}", leftovers(&v));
            }
        }
    }

    #[test]
    fn native_paths() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().to_string_lossy().into_owned();
        let v = NativeVfs;
        let card = v.join(&root, "card");
        v.replace_dir(&card, &sample()).unwrap();
        assert_eq!(v.read_tree(&card).unwrap(), sample(), "times included");
        assert_eq!(v.name(&card).as_deref(), Some("card"));
        assert_eq!(v.parent(&card), Some(root.clone()));
        let names: Vec<String> = v.list(&card).unwrap().into_iter().map(|(n, _)| n).collect();
        assert_eq!(names.len(), 2);
        assert!(v.stat(&v.join(&card, "SAVE")).unwrap().is_dir);
        assert_eq!(v.stat(&v.join(&root, "nothing")).unwrap_err().kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn scratch_names() {
        assert!(is_scratch_name("A.emusync-old") && is_scratch_name("Mcd001.ps2.emusync-swap"));
        assert!(!is_scratch_name("Mcd001.ps2"));
        assert_eq!(scratch_owner("A.emusync-tmp"), Some("A"));
    }
}

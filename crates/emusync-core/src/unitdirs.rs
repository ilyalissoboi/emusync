//! Containers whose subdirectories are the units ([`ContainerKind::UnitDirs`], DESIGN §27): a
//! PSP `SAVEDATA` folder, where a memory stick can hold hundreds of saves and GBs of game data.
//!
//! Each save folder is handled on its own. A scan lists the folders with their files' sizes and
//! modification times, without reading any file, and the engine reads only a folder whose
//! listing (its *signature*) changed since it last read it. An apply replaces only the folders
//! it changes, each with the crash-safe [`Vfs::replace_dir`]. Folders over the format's size
//! limit, files, and emusync's own scratch folders are never read or written.
//!
//! Everything goes through a [`Vfs`] (DESIGN §28), so a memory stick can be a `content://`
//! folder on Android.
//!
//! [`ContainerKind::UnitDirs`]: emusync_formats::ContainerKind::UnitDirs

use crate::model::content_id;
use crate::vfs::{self, Vfs};
use emusync_formats::tree::{self, EntryKind, Tree};
use std::io;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How recent a modification may be for a signature to be trusted. A file written again within
/// the same timestamp tick, with the same size, would keep its signature; one that was written
/// this recently is read again at the next scan instead (git's "racy" rule).
const SETTLE: Duration = Duration::from_secs(3);

/// Deepest folder walked below a unit folder, as deep as a [`Tree`] goes.
const MAX_DEPTH: usize = tree::MAX_DEPTH;

/// A unit folder as listed.
#[derive(Debug, Clone)]
pub struct UnitDir {
    pub name: String,
    /// Hash of every entry's path, kind, size and modification time.
    pub signature: String,
    /// Whether nothing in it changed in the last [`SETTLE`]: only then may the signature stand
    /// for the content until it changes.
    pub settled: bool,
}

/// What a container holds, from file metadata alone.
#[derive(Debug, Default)]
pub struct Listing {
    pub units: Vec<UnitDir>,
    /// Folders over the size limit, left alone.
    pub oversize: Vec<String>,
    /// Hash of every unit's name and signature: a change to any unit changes it.
    pub manifest: String,
}

struct Walk {
    text: String,
    size: u64,
    newest_ns: i64,
}

fn walk(v: &dyn Vfs, dir: &str, rel: &str, depth: usize, max: u64, w: &mut Walk) -> io::Result<()> {
    if depth > MAX_DEPTH {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{dir} is nested too deeply")));
    }
    let mut entries = v.list(dir)?;
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, st) in entries {
        w.newest_ns = w.newest_ns.max(st.mtime_ns);
        let child_rel = tree::join(rel, &name);
        if st.is_dir {
            w.text.push_str(&format!("d {} {child_rel}\n", st.mtime_ns));
            walk(v, &v.join(dir, &name), &child_rel, depth + 1, max, w)?;
        } else {
            w.size += st.size;
            w.text.push_str(&format!("f {} {} {child_rel}\n", st.size, st.mtime_ns));
        }
        if w.size > max {
            return Ok(());
        }
    }
    Ok(())
}

fn now_ns() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as i64)
}

/// The unit folder `name` in `dir`, from its metadata; `None` if it is over `max` bytes.
fn unit_dir(v: &dyn Vfs, dir: &str, name: &str, max: u64) -> io::Result<Option<UnitDir>> {
    let loc = v.join(dir, name);
    let mut w = Walk { text: String::new(), size: 0, newest_ns: v.stat(&loc)?.mtime_ns };
    walk(v, &loc, "", 1, max, &mut w)?;
    if w.size > max {
        return Ok(None);
    }
    let settled = now_ns().saturating_sub(w.newest_ns) >= SETTLE.as_nanos() as i64;
    Ok(Some(UnitDir { name: name.to_string(), signature: content_id(w.text.as_bytes()), settled }))
}

/// Lists the unit folders of `dir`: every subdirectory with a valid name, but emusync's scratch
/// folders and those over `max` bytes.
pub fn list(v: &dyn Vfs, dir: &str, max: Option<u64>) -> io::Result<Listing> {
    let max = max.unwrap_or(u64::MAX);
    let mut names: Vec<String> = v
        .list(dir)?
        .into_iter()
        .filter(|(n, st)| st.is_dir && tree::valid_segment(n) && !vfs::is_scratch_name(n))
        .map(|(n, _)| n)
        .collect();
    names.sort();
    let mut listing = Listing::default();
    let mut manifest = String::new();
    for name in names {
        match unit_dir(v, dir, &name, max)? {
            Some(u) => {
                manifest.push_str(&format!("{} {}\n", u.signature, u.name));
                listing.units.push(u);
            }
            None => listing.oversize.push(name),
        }
    }
    listing.manifest = content_id(manifest.as_bytes());
    Ok(listing)
}

/// Finishes the folder replaces and removals in `dir` that were interrupted.
pub fn recover(v: &dyn Vfs, dir: &str) -> io::Result<()> {
    let mut owners: Vec<String> =
        v.list(dir)?.into_iter().filter_map(|(n, _)| vfs::scratch_owner(&n).map(str::to_string)).collect();
    owners.sort();
    owners.dedup();
    for name in owners {
        v.recover(&v.join(dir, &name))?;
    }
    Ok(())
}

/// The signature of the unit folder `name` in `dir` as it is now, if it is settled.
pub fn settled_signature(v: &dyn Vfs, dir: &str, name: &str) -> io::Result<Option<String>> {
    Ok(unit_dir(v, dir, name, u64::MAX)?.filter(|u| u.settled).map(|u| u.signature))
}

/// Reads the unit folder `name` in `dir` as an image with that folder at its root.
pub fn read_unit(v: &dyn Vfs, dir: &str, name: &str) -> io::Result<Tree> {
    let loc = v.join(dir, name);
    let inner = v.read_tree(&loc)?;
    let mut out = Tree::default();
    let bad = |e: emusync_formats::FormatError| io::Error::new(io::ErrorKind::InvalidData, e.to_string());
    out.insert(name.to_string(), v.stat(&loc)?.mtime_secs(), EntryKind::Dir).map_err(bad)?;
    for (rel, e) in inner.entries {
        out.insert(tree::join(name, &rel), e.mtime, e.kind).map_err(bad)?;
    }
    Ok(out)
}

/// Replaces the unit folder `name` in `dir` (or creates it) with the folder `name` in `image`.
pub fn write_unit(v: &dyn Vfs, dir: &str, name: &str, image: &Tree) -> io::Result<()> {
    let folder = image
        .get(name)
        .filter(|e| matches!(e.kind, EntryKind::Dir))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("no folder {name:?} to write")))?;
    let prefix = format!("{name}/");
    let mut inner = Tree::default();
    for (path, e) in image.entries.range(prefix.clone()..).take_while(|(p, _)| p.starts_with(&prefix)) {
        inner
            .insert(path[prefix.len()..].to_string(), e.mtime, e.kind.clone())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    }
    let loc = v.join(dir, name);
    v.replace_dir(&loc, &inner)?;
    // Best effort: storage that can't set times (SAF) keeps the time of the write.
    let _ = v.set_mtime(&loc, folder.mtime);
    Ok(())
}

/// Removes the unit folder `name` from `dir`; nothing to do if it isn't there.
pub fn remove_unit(v: &dyn Vfs, dir: &str, name: &str) -> io::Result<()> {
    match v.remove_dir(&v.join(dir, name)) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::{MemoryVfs, NativeVfs};
    use std::fs;

    fn old(path: &std::path::Path) {
        let t = SystemTime::now() - Duration::from_secs(3600);
        let f = fs::File::options().write(true).open(path).unwrap();
        f.set_modified(t).unwrap();
    }

    fn native(d: &std::path::Path) -> String {
        d.to_string_lossy().into_owned()
    }

    #[test]
    fn listing_skips_files_scratch_and_large_folders() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        fs::write(d.join(".nomedia"), b"").unwrap();
        fs::create_dir(d.join("ULUS10041S0")).unwrap();
        fs::write(d.join("ULUS10041S0/DATA.BIN"), [1; 100]).unwrap();
        fs::create_dir(d.join("ULUS10041S1.emusync-tmp")).unwrap();
        fs::create_dir(d.join("ULJM05500INSTALL")).unwrap();
        fs::write(d.join("ULJM05500INSTALL/DATA.BIN"), [2; 300]).unwrap();
        let l = list(&NativeVfs, &native(d), Some(200)).unwrap();
        assert_eq!(l.units.iter().map(|u| u.name.as_str()).collect::<Vec<_>>(), ["ULUS10041S0"]);
        assert_eq!(l.oversize, ["ULJM05500INSTALL"]);
        // Just written: not settled.
        assert!(!l.units[0].settled);
    }

    #[test]
    fn signatures_follow_changes() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        fs::create_dir(d.join("S")).unwrap();
        fs::write(d.join("S/a"), b"one").unwrap();
        old(&d.join("S/a"));
        let first = list(&NativeVfs, &native(d), None).unwrap();
        fs::write(d.join("S/a"), b"two!").unwrap();
        let second = list(&NativeVfs, &native(d), None).unwrap();
        assert_ne!(first.units[0].signature, second.units[0].signature);
        assert_ne!(first.manifest, second.manifest);
        assert_eq!(list(&NativeVfs, &native(d), None).unwrap().manifest, second.manifest);
    }

    /// The same on the native file system and on storage that can't set times.
    #[test]
    fn units_are_read_written_and_removed_on_their_own() {
        let dir = tempfile::tempdir().unwrap();
        let mem = MemoryVfs::with_root("/stick/SAVEDATA");
        for (v, d) in [(&NativeVfs as &dyn Vfs, native(dir.path())), (&mem, "/stick/SAVEDATA".to_string())] {
            v.create_dir(&v.join(&d, "A")).unwrap();
            v.create_dir(&v.join(&v.join(&d, "A"), "sub")).unwrap();
            v.write(&v.join(&v.join(&v.join(&d, "A"), "sub"), "x"), b"x").unwrap();
            v.create_dir(&v.join(&d, "B")).unwrap();
            v.write(&v.join(&v.join(&d, "B"), "y"), b"y").unwrap();

            let a = read_unit(v, &d, "A").unwrap();
            assert_eq!(a.file("A/sub/x"), Some(&b"x"[..]));
            assert!(a.get("B").is_none());

            let mut image = Tree::default();
            image.insert("C".into(), 1_000_000, EntryKind::Dir).unwrap();
            image.insert("C/z".into(), 2_000_000, EntryKind::File(b"z".to_vec())).unwrap();
            write_unit(v, &d, "C", &image).unwrap();
            assert_eq!(read_unit(v, &d, "C").unwrap().file("C/z"), Some(&b"z"[..]));
            if v.is_native() {
                assert_eq!(read_unit(v, &d, "C").unwrap(), image, "times included");
            }

            remove_unit(v, &d, "A").unwrap();
            remove_unit(v, &d, "A").unwrap();
            assert!(!vfs::exists(v, &v.join(&d, "A")).unwrap());
            assert_eq!(v.read(&v.join(&v.join(&d, "B"), "y")).unwrap(), b"y");
            let names: Vec<String> = list(v, &d, None).unwrap().units.into_iter().map(|u| u.name).collect();
            assert_eq!(names, ["B", "C"]);
        }
    }

    /// A removal interrupted after the rename leaves `<name>.emusync-old`, which recovery
    /// deletes.
    #[test]
    fn recovery_finishes_an_interrupted_removal() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        fs::create_dir(d.join("A.emusync-old")).unwrap();
        fs::write(d.join("A.emusync-old/f"), b"f").unwrap();
        recover(&NativeVfs, &native(d)).unwrap();
        assert!(!d.join("A.emusync-old").exists());
    }
}

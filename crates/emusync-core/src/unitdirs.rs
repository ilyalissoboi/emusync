//! Containers whose subdirectories are the units ([`ContainerKind::UnitDirs`], DESIGN §27): a
//! PSP `SAVEDATA` folder, where a memory stick can hold hundreds of saves and GBs of game data.
//!
//! Each save folder is handled on its own. A scan lists the folders with their files' sizes and
//! modification times, without reading any file, and the engine reads only a folder whose
//! listing (its *signature*) changed since it last read it. An apply replaces only the folders
//! it changes, each with the crash-safe [`fsutil::replace_dir`]. Folders over the format's size
//! limit, files, and emusync's own scratch folders are never read or written.
//!
//! [`ContainerKind::UnitDirs`]: emusync_formats::ContainerKind::UnitDirs

use crate::fsutil;
use crate::model::content_id;
use emusync_formats::tree::{self, EntryKind, Tree};
use std::fs;
use std::io;
use std::path::Path;
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
    newest: SystemTime,
}

fn walk(path: &Path, rel: &str, depth: usize, max: u64, w: &mut Walk) -> io::Result<()> {
    if depth > MAX_DEPTH {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{} is nested too deeply", path.display())));
    }
    let mut entries: Vec<(String, std::path::PathBuf)> = Vec::new();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        entries.push((entry.file_name().to_string_lossy().into_owned(), entry.path()));
    }
    entries.sort();
    for (name, child) in entries {
        // Followed like `fsutil::read_tree` follows it.
        let meta = fs::metadata(&child)?;
        let modified = meta.modified().unwrap_or(UNIX_EPOCH);
        w.newest = w.newest.max(modified);
        let since = modified.duration_since(UNIX_EPOCH).unwrap_or_default();
        let child_rel = tree::join(rel, &name);
        if meta.is_dir() {
            w.text.push_str(&format!("d {} {child_rel}\n", since.as_nanos()));
            walk(&child, &child_rel, depth + 1, max, w)?;
        } else {
            w.size += meta.len();
            w.text.push_str(&format!("f {} {} {child_rel}\n", meta.len(), since.as_nanos()));
        }
        if w.size > max {
            return Ok(());
        }
    }
    Ok(())
}

/// The unit folder `name` in `dir`, from its metadata; `None` if it is over `max` bytes.
fn unit_dir(dir: &Path, name: &str, max: u64) -> io::Result<Option<UnitDir>> {
    let path = dir.join(name);
    let mut w = Walk { text: String::new(), size: 0, newest: fs::metadata(&path)?.modified().unwrap_or(UNIX_EPOCH) };
    walk(&path, "", 1, max, &mut w)?;
    if w.size > max {
        return Ok(None);
    }
    let settled = SystemTime::now().duration_since(w.newest).is_ok_and(|d| d >= SETTLE);
    Ok(Some(UnitDir { name: name.to_string(), signature: content_id(w.text.as_bytes()), settled }))
}

/// Lists the unit folders of `dir`: every subdirectory with a valid name, but emusync's scratch
/// folders and those over `max` bytes.
pub fn list(dir: &Path, max: Option<u64>) -> io::Result<Listing> {
    let max = max.unwrap_or(u64::MAX);
    let mut names = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let Ok(name) = entry.file_name().into_string() else {
            log::warn!("{}: skipping a folder whose name isn't UTF-8", dir.display());
            continue;
        };
        if tree::valid_segment(&name) && !fsutil::is_scratch(&entry.path()) && fs::metadata(entry.path())?.is_dir() {
            names.push(name);
        }
    }
    names.sort();
    let mut listing = Listing::default();
    let mut manifest = String::new();
    for name in names {
        match unit_dir(dir, &name, max)? {
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
pub fn recover(dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if !fsutil::is_scratch(&path) {
            continue;
        }
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        if let Some((base, _)) = name.rsplit_once(".emusync-") {
            fsutil::recover_dir_replace(&dir.join(base))?;
        }
    }
    Ok(())
}

/// The signature of the unit folder `name` in `dir` as it is now, if it is settled.
pub fn settled_signature(dir: &Path, name: &str) -> io::Result<Option<String>> {
    Ok(unit_dir(dir, name, u64::MAX)?.filter(|u| u.settled).map(|u| u.signature))
}

/// Reads the unit folder `name` in `dir` as an image with that folder at its root.
pub fn read_unit(dir: &Path, name: &str) -> io::Result<Tree> {
    let path = dir.join(name);
    let inner = fsutil::read_tree(&path)?;
    let mtime = fs::metadata(&path)?.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok());
    let mut out = Tree::default();
    let bad = |e: emusync_formats::FormatError| io::Error::new(io::ErrorKind::InvalidData, e.to_string());
    out.insert(name.to_string(), mtime.map_or(0, |d| d.as_secs() as i64), EntryKind::Dir).map_err(bad)?;
    for (rel, e) in inner.entries {
        out.insert(tree::join(name, &rel), e.mtime, e.kind).map_err(bad)?;
    }
    Ok(out)
}

/// Replaces the unit folder `name` in `dir` (or creates it) with the folder `name` in `image`.
pub fn write_unit(dir: &Path, name: &str, image: &Tree) -> io::Result<()> {
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
    let path = dir.join(name);
    fsutil::replace_dir(&path, &inner)?;
    fsutil::set_dir_mtime(&path, folder.mtime);
    Ok(())
}

/// Removes the unit folder `name` from `dir`; nothing to do if it isn't there.
pub fn remove_unit(dir: &Path, name: &str) -> io::Result<()> {
    match fsutil::remove_dir(&dir.join(name)) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn old(path: &Path) {
        let t = SystemTime::now() - Duration::from_secs(3600);
        let f = fs::File::options().write(true).open(path).unwrap();
        f.set_modified(t).unwrap();
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
        let l = list(d, Some(200)).unwrap();
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
        let first = list(d, None).unwrap();
        fs::write(d.join("S/a"), b"two!").unwrap();
        let second = list(d, None).unwrap();
        assert_ne!(first.units[0].signature, second.units[0].signature);
        assert_ne!(first.manifest, second.manifest);
        assert_eq!(list(d, None).unwrap().manifest, second.manifest);
    }

    #[test]
    fn units_are_read_written_and_removed_on_their_own() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        fs::create_dir(d.join("A")).unwrap();
        fs::create_dir(d.join("A/sub")).unwrap();
        fs::write(d.join("A/sub/x"), b"x").unwrap();
        fs::create_dir(d.join("B")).unwrap();
        fs::write(d.join("B/y"), b"y").unwrap();

        let a = read_unit(d, "A").unwrap();
        assert_eq!(a.file("A/sub/x"), Some(&b"x"[..]));
        assert!(a.get("B").is_none());

        let mut image = Tree::default();
        image.insert("C".into(), 1_000_000, EntryKind::Dir).unwrap();
        image.insert("C/z".into(), 2_000_000, EntryKind::File(b"z".to_vec())).unwrap();
        write_unit(d, "C", &image).unwrap();
        assert_eq!(read_unit(d, "C").unwrap(), image);

        remove_unit(d, "A").unwrap();
        remove_unit(d, "A").unwrap();
        assert!(!d.join("A").exists());
        assert_eq!(fs::read(d.join("B/y")).unwrap(), b"y");
        let names: Vec<String> = list(d, None).unwrap().units.into_iter().map(|u| u.name).collect();
        assert_eq!(names, ["B", "C"]);
    }

    /// A removal interrupted after the rename leaves `<name>.emusync-old`, which recovery
    /// deletes.
    #[test]
    fn recovery_finishes_an_interrupted_removal() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        fs::create_dir(d.join("A.emusync-old")).unwrap();
        fs::write(d.join("A.emusync-old/f"), b"f").unwrap();
        recover(d).unwrap();
        assert!(!d.join("A.emusync-old").exists());
    }
}

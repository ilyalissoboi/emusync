//! File helpers: atomic replace of files and directories (DESIGN §5, step 5; §19).

use emusync_formats::tree::{self, EntryKind, Tree};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(suffix);
    path.with_file_name(name)
}

/// Sibling temp path used while replacing `path`: `<name>.emusync-tmp`.
pub fn temp_path(path: &Path) -> PathBuf {
    sibling(path, ".emusync-tmp")
}

/// Where a replaced directory waits until the new one is in place: `<name>.emusync-old`.
pub fn old_path(path: &Path) -> PathBuf {
    sibling(path, ".emusync-old")
}

/// The journal of a directory replace in progress: `<name>.emusync-swap`.
pub fn journal_path(path: &Path) -> PathBuf {
    sibling(path, ".emusync-swap")
}

/// Writes `data` to a temp file next to `path`, fsyncs it, and renames it over `path`.
/// `rename` is atomic on POSIX; on Windows the standard library uses
/// `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`, which replaces the file in one step.
/// The original is untouched until the rename.
pub fn atomic_replace(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = temp_path(path);
    let result = (|| {
        let mut f = File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)?;
        #[cfg(unix)]
        if let Some(dir) = path.parent() {
            File::open(dir)?.sync_all()?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Writes a content-addressed file if it doesn't exist yet (temp + rename, so a crash never
/// leaves a truncated file under the final name).
pub fn write_once(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    atomic_replace(path, data)
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

fn unix_secs(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

fn invalid_data(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// Reads everything under `dir` into a [`Tree`]: files with their contents and every
/// modification time. Symbolic links are followed. Names must be UTF-8.
pub fn read_tree(dir: &Path) -> io::Result<Tree> {
    fn walk(tree: &mut Tree, dir: &Path, rel: &str, depth: usize) -> io::Result<()> {
        if depth > tree::MAX_DEPTH {
            return Err(invalid_data(format!("{} is nested too deeply", dir.display())));
        }
        let mut entries: Vec<(String, PathBuf)> = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|n| invalid_data(format!("{} has a name that isn't UTF-8: {n:?}", dir.display())))?;
            entries.push((name, entry.path()));
        }
        entries.sort();
        for (name, path) in entries {
            if !tree::valid_segment(&name) {
                return Err(invalid_data(format!("unsupported file name {}", path.display())));
            }
            let meta = fs::metadata(&path)?;
            let mtime = meta.modified().map(unix_secs).unwrap_or(0);
            let child = tree::join(rel, &name);
            if meta.is_dir() {
                tree.insert(child.clone(), mtime, EntryKind::Dir).map_err(|e| invalid_data(e.to_string()))?;
                walk(tree, &path, &child, depth + 1)?;
            } else {
                let data = fs::read(&path)?;
                tree.insert(child, mtime, EntryKind::File(data)).map_err(|e| invalid_data(e.to_string()))?;
            }
        }
        Ok(())
    }
    if !fs::metadata(dir)?.is_dir() {
        return Err(invalid_data(format!("{} is not a directory", dir.display())));
    }
    let mut tree = Tree::default();
    walk(&mut tree, dir, "", 1)?;
    Ok(tree)
}

/// Writes `tree` into the new directory `dir`, fsyncing every file.
fn system_time(secs: i64) -> SystemTime {
    if secs >= 0 {
        UNIX_EPOCH + Duration::from_secs(secs as u64)
    } else {
        UNIX_EPOCH - Duration::from_secs(secs.unsigned_abs())
    }
}

/// Opens a directory so its timestamps can be set.
#[cfg(windows)]
fn open_dir_for_times(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    fs::OpenOptions::new().access_mode(FILE_WRITE_ATTRIBUTES).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(path)
}

#[cfg(not(windows))]
fn open_dir_for_times(path: &Path) -> io::Result<File> {
    File::open(path)
}

/// Writes `tree` into the new directory `dir`, fsyncing every file. Modification times are
/// restored too (directories last, after their children), so reading the result back gives
/// the same tree image. Setting a time is best effort: a file system that can't store it
/// exactly only makes the next scan see a change.
fn write_tree(dir: &Path, tree: &Tree) -> io::Result<()> {
    fs::create_dir(dir)?;
    let mut dirs = vec![(dir.to_path_buf(), None)];
    for (rel, e) in &tree.entries {
        let path = rel.split('/').fold(dir.to_path_buf(), |p, s| p.join(s));
        match &e.kind {
            EntryKind::Dir => {
                fs::create_dir(&path)?;
                dirs.push((path, Some(e.mtime)));
            }
            EntryKind::File(data) => {
                let mut f = File::create(&path)?;
                f.write_all(data)?;
                // Also the host time PCSX2 falls back to for files its index doesn't list.
                let _ = f.set_modified(system_time(e.mtime));
                f.sync_all()?;
            }
        }
    }
    // Children come after their parent in path order, so this sets each directory's time
    // once nothing more is written into it.
    for (d, mtime) in dirs.iter().rev() {
        if let Some(mtime) = mtime {
            if let Ok(f) = open_dir_for_times(d) {
                let _ = f.set_modified(system_time(*mtime));
            }
        }
        sync_dir(d)?;
    }
    Ok(())
}

/// Replaces the directory `dir` with `tree`, crash-safely:
///
/// 1. write the new tree to `<dir>.emusync-tmp` and fsync it;
/// 2. write the journal `<dir>.emusync-swap`;
/// 3. rename `dir` to `<dir>.emusync-old`, then the new tree to `dir`;
/// 4. remove the old tree, then the journal.
///
/// Until step 2 the original is untouched and an interrupted replace is discarded; after it,
/// [`recover_dir_replace`] finishes the replace. A failed rename (on Windows, a file in the
/// directory that another program holds open) rolls back and returns the error.
pub fn replace_dir(dir: &Path, tree: &Tree) -> io::Result<()> {
    recover_dir_replace(dir)?;
    let (tmp, old, journal) = (temp_path(dir), old_path(dir), journal_path(dir));
    if old.exists() {
        fs::remove_dir_all(&old)?;
    }
    if let Err(e) = write_tree(&tmp, tree) {
        let _ = fs::remove_dir_all(&tmp);
        return Err(e);
    }
    if let Err(e) = atomic_replace(&journal, b"emusync directory replace\n") {
        let _ = fs::remove_dir_all(&tmp);
        return Err(e);
    }
    if let Err(e) = fs::rename(dir, &old) {
        if e.kind() != io::ErrorKind::NotFound {
            let _ = fs::remove_file(&journal);
            let _ = fs::remove_dir_all(&tmp);
            return Err(e);
        }
    }
    if let Err(e) = fs::rename(&tmp, dir) {
        if fs::rename(&old, dir).is_ok() {
            let _ = fs::remove_file(&journal);
            let _ = fs::remove_dir_all(&tmp);
        }
        // Otherwise the journal stays, and the next recovery completes the replace.
        return Err(e);
    }
    finish(dir, &old, &journal)
}

fn finish(dir: &Path, old: &Path, journal: &Path) -> io::Result<()> {
    if let Some(parent) = dir.parent() {
        sync_dir(parent)?;
    }
    if old.exists() {
        fs::remove_dir_all(old)?;
    }
    fs::remove_file(journal)
}

/// Completes or discards a [`replace_dir`] that was interrupted. With a journal, the new tree
/// was complete before anything was renamed, so the replace is rolled forward; without one,
/// a leftover temp tree is incomplete and is removed.
pub fn recover_dir_replace(dir: &Path) -> io::Result<()> {
    let (tmp, old, journal) = (temp_path(dir), old_path(dir), journal_path(dir));
    if !journal.exists() {
        if tmp.exists() {
            fs::remove_dir_all(&tmp)?;
        }
        return Ok(());
    }
    if tmp.exists() {
        if dir.exists() {
            if old.exists() {
                // Not a state replace_dir leaves behind: keep the live directory.
                fs::remove_dir_all(&tmp)?;
                return fs::remove_file(&journal);
            }
            fs::rename(dir, &old)?;
        }
        fs::rename(&tmp, dir)?;
    }
    finish(dir, &old, &journal)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Tree {
        let mut t = Tree::default();
        t.insert("_pcsx2_superblock".into(), 1_700_000_000, EntryKind::File(vec![7; 100])).unwrap();
        t.insert("SAVE".into(), 1_500_000_000, EntryKind::Dir).unwrap();
        t.insert("SAVE/data".into(), 1_600_000_000, EntryKind::File(b"new".to_vec())).unwrap();
        t.insert("SAVE/empty".into(), 1_400_000_000, EntryKind::Dir).unwrap();
        t
    }

    fn files(t: &Tree) -> Vec<(String, Option<Vec<u8>>)> {
        t.entries
            .iter()
            .map(|(p, e)| match &e.kind {
                EntryKind::File(d) => (p.clone(), Some(d.clone())),
                EntryKind::Dir => (p.clone(), None),
            })
            .collect()
    }

    #[test]
    fn read_and_replace_directories() {
        let root = tempfile::tempdir().unwrap();
        let card = root.path().join("Mcd001.ps2");
        fs::create_dir_all(card.join("OLD")).unwrap();
        fs::write(card.join("OLD/x"), b"old").unwrap();

        replace_dir(&card, &sample()).unwrap();
        let back = read_tree(&card).unwrap();
        assert_eq!(files(&back), files(&sample()));
        assert_eq!(back, sample(), "the same image, modification times included");
        for p in [temp_path(&card), old_path(&card), journal_path(&card)] {
            assert!(!p.exists(), "{} left behind", p.display());
        }
        assert!(read_tree(&card.join("_pcsx2_superblock")).is_err());
    }

    #[test]
    fn interrupted_replaces_recover() {
        let root = tempfile::tempdir().unwrap();
        let card = root.path().join("card");
        let mut original = Tree::default();
        original.insert("a".into(), 0, EntryKind::File(b"original".to_vec())).unwrap();
        let fresh = || {
            for p in [card.clone(), temp_path(&card), old_path(&card)] {
                let _ = fs::remove_dir_all(p);
            }
            let _ = fs::remove_file(journal_path(&card));
            write_tree(&card, &original).unwrap();
        };
        // Crash while writing the temp tree: discarded, the original stays.
        fresh();
        write_tree(&temp_path(&card), &sample()).unwrap();
        recover_dir_replace(&card).unwrap();
        assert_eq!(files(&read_tree(&card).unwrap()), files(&original));
        assert!(!temp_path(&card).exists());
        // Crash after each later step: rolled forward to the new tree.
        for step in 0..4 {
            fresh();
            write_tree(&temp_path(&card), &sample()).unwrap();
            atomic_replace(&journal_path(&card), b"x").unwrap();
            if step >= 1 {
                fs::rename(&card, old_path(&card)).unwrap();
            }
            if step >= 2 {
                fs::rename(temp_path(&card), &card).unwrap();
            }
            if step >= 3 {
                fs::remove_dir_all(old_path(&card)).unwrap();
            }
            recover_dir_replace(&card).unwrap();
            assert_eq!(files(&read_tree(&card).unwrap()), files(&sample()), "step {step}");
            for p in [temp_path(&card), old_path(&card), journal_path(&card)] {
                assert!(!p.exists(), "step {step}: {} left behind", p.display());
            }
        }
    }

    #[test]
    fn replace_and_write_once() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("Mcd001.ps2");
        fs::write(&p, b"old").unwrap();
        atomic_replace(&p, b"new").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"new");
        assert!(!temp_path(&p).exists());

        let q = dir.path().join("a/b/c");
        write_once(&q, b"1").unwrap();
        write_once(&q, b"2").unwrap();
        assert_eq!(fs::read(&q).unwrap(), b"1");
    }
}

//! File helpers: atomic replace (DESIGN §5, step 5).

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Sibling temp path used while replacing `path`: `<name>.emusync-tmp`.
pub fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".emusync-tmp");
    path.with_file_name(name)
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

#[cfg(test)]
mod tests {
    use super::*;

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

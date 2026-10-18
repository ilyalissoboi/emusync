//! Directory containers held in memory (DESIGN §5.2, §18).
//!
//! Adapters are pure, so a container that is a directory on the host (a PCSX2 folder memory
//! card) reaches them as a [`Tree`] image: every file and directory under the container
//! root, with its contents and modification time, serialized into one byte string. The core
//! reads the directory into this image, and writes an adapter's output image back with a
//! journalled directory replace. Both directions live in `emusync-core` (`fsutil`).
//!
//! Image layout: the magic `ES-TREE1`, then one record per entry in path order:
//! kind (`u8`: 0 file, 1 directory), path length (`u16` LE), the path (UTF-8, `/`-separated,
//! relative to the root), the modification time (`i64` LE, Unix seconds), and for files the
//! content length (`u32` LE) and the content.

use crate::{FormatError, Result};
use std::collections::BTreeMap;

const MAGIC: &[u8; 8] = b"ES-TREE1";
const KIND_FILE: u8 = 0;
const KIND_DIR: u8 = 1;
/// Deepest nesting accepted below the root.
pub const MAX_DEPTH: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    File(Vec<u8>),
    Dir,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Modification time in Unix seconds (0 when unknown).
    pub mtime: i64,
    pub kind: EntryKind,
}

/// A directory tree. Keys are paths relative to the root; every parent directory of an entry
/// is itself an entry.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Tree {
    pub entries: BTreeMap<String, Entry>,
}

fn invalid(msg: impl Into<String>) -> FormatError {
    FormatError::Invalid(msg.into())
}

/// Whether `name` can be one path segment.
pub fn valid_segment(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0'])
}

fn check_path(path: &str) -> Result<()> {
    let segments: Vec<&str> = path.split('/').collect();
    if segments.len() > MAX_DEPTH || !segments.iter().all(|s| valid_segment(s)) {
        return Err(invalid(format!("invalid path {path:?} in directory container")));
    }
    Ok(())
}

/// `parent/name`, or `name` at the root (`parent` empty).
pub fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

/// Whether `bytes` starts like a tree image.
pub fn is_tree(bytes: &[u8]) -> bool {
    bytes.starts_with(MAGIC)
}

impl Tree {
    pub fn encode(&self) -> Vec<u8> {
        let size: usize = self
            .entries
            .iter()
            .map(|(p, e)| 15 + p.len() + if let EntryKind::File(d) = &e.kind { 4 + d.len() } else { 0 })
            .sum();
        let mut out = Vec::with_capacity(MAGIC.len() + size);
        out.extend_from_slice(MAGIC);
        for (path, e) in &self.entries {
            out.push(if matches!(e.kind, EntryKind::Dir) { KIND_DIR } else { KIND_FILE });
            out.extend_from_slice(&(path.len() as u16).to_le_bytes());
            out.extend_from_slice(path.as_bytes());
            out.extend_from_slice(&e.mtime.to_le_bytes());
            if let EntryKind::File(data) = &e.kind {
                out.extend_from_slice(&(data.len() as u32).to_le_bytes());
                out.extend_from_slice(data);
            }
        }
        out
    }

    /// Parses and validates an image: paths are well-formed, sorted, unique, and their
    /// parents are directories in the tree.
    pub fn decode(bytes: &[u8]) -> Result<Tree> {
        let mut b = bytes.strip_prefix(MAGIC.as_slice()).ok_or_else(|| invalid("not a directory container"))?;
        let mut take = |n: usize| -> Result<&[u8]> {
            if b.len() < n {
                return Err(invalid("truncated directory container"));
            }
            let (head, rest) = b.split_at(n);
            b = rest;
            Ok(head)
        };
        let mut entries = BTreeMap::new();
        let mut last: Option<String> = None;
        while let Ok(kind) = take(1) {
            let kind = kind[0];
            let len = u16::from_le_bytes(take(2)?.try_into().unwrap()) as usize;
            let path = std::str::from_utf8(take(len)?)
                .map_err(|_| invalid("path in directory container is not UTF-8"))?
                .to_string();
            check_path(&path)?;
            let mtime = i64::from_le_bytes(take(8)?.try_into().unwrap());
            let kind = match kind {
                KIND_FILE => {
                    let n = u32::from_le_bytes(take(4)?.try_into().unwrap()) as usize;
                    EntryKind::File(take(n)?.to_vec())
                }
                KIND_DIR => EntryKind::Dir,
                other => return Err(invalid(format!("unknown entry kind {other} in directory container"))),
            };
            if last.as_ref().is_some_and(|l| *l >= path) {
                return Err(invalid("directory container entries are not sorted or not unique"));
            }
            if let Some((parent, _)) = path.rsplit_once('/') {
                if !matches!(entries.get(parent), Some(Entry { kind: EntryKind::Dir, .. })) {
                    return Err(invalid(format!("{path:?} has no parent directory in the container")));
                }
            }
            last = Some(path.clone());
            entries.insert(path, Entry { mtime, kind });
        }
        Ok(Tree { entries })
    }

    pub fn get(&self, path: &str) -> Option<&Entry> {
        self.entries.get(path)
    }

    /// Contents of the file at `path`, if it is one.
    pub fn file(&self, path: &str) -> Option<&[u8]> {
        match &self.entries.get(path)?.kind {
            EntryKind::File(d) => Some(d),
            EntryKind::Dir => None,
        }
    }

    pub fn is_dir(&self, path: &str) -> bool {
        matches!(self.entries.get(path), Some(Entry { kind: EntryKind::Dir, .. }))
    }

    /// Direct children of the directory `dir` (empty for the root), sorted by name.
    pub fn children<'a>(&'a self, dir: &str) -> impl Iterator<Item = (&'a str, &'a Entry)> + 'a {
        let prefix = if dir.is_empty() { String::new() } else { format!("{dir}/") };
        let len = prefix.len();
        self.entries.range(prefix.clone()..).take_while(move |(p, _)| p.starts_with(&prefix)).filter_map(
            move |(p, e)| {
                let name = &p[len..];
                (!name.contains('/')).then_some((name, e))
            },
        )
    }

    /// Adds a directory, or a file with its contents. The parent must exist.
    pub fn insert(&mut self, path: String, mtime: i64, kind: EntryKind) -> Result<()> {
        check_path(&path)?;
        if let Some((parent, _)) = path.rsplit_once('/') {
            if !self.is_dir(parent) {
                return Err(invalid(format!("{path:?} has no parent directory")));
            }
        }
        self.entries.insert(path, Entry { mtime, kind });
        Ok(())
    }

    /// Removes `path` and everything under it.
    pub fn remove(&mut self, path: &str) {
        let prefix = format!("{path}/");
        self.entries.retain(|p, _| p != path && !p.starts_with(&prefix));
    }

    /// Moves `from` and everything under it to `to`, replacing whatever was at `to`.
    pub fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        check_path(to)?;
        self.remove(to);
        let prefix = format!("{from}/");
        let moved: Vec<String> =
            self.entries.keys().filter(|p| *p == from || p.starts_with(&prefix)).cloned().collect();
        for p in moved {
            let e = self.entries.remove(&p).unwrap();
            self.entries.insert(format!("{to}{}", &p[from.len()..]), e);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Tree {
        let mut t = Tree::default();
        t.insert("_pcsx2_superblock".into(), 5, EntryKind::File(vec![1, 2, 3])).unwrap();
        t.insert("SAVE".into(), 6, EntryKind::Dir).unwrap();
        t.insert("SAVE/a".into(), 7, EntryKind::File(vec![])).unwrap();
        t.insert("SAVE/sub".into(), -1, EntryKind::Dir).unwrap();
        t.insert("SAVE/sub/b".into(), 8, EntryKind::File(vec![9; 3000])).unwrap();
        t.insert("SAVE-2".into(), 0, EntryKind::Dir).unwrap();
        t
    }

    #[test]
    fn round_trip() {
        let t = sample();
        let img = t.encode();
        assert!(is_tree(&img));
        assert_eq!(Tree::decode(&img).unwrap(), t);
        let names: Vec<&str> = t.children("").map(|(n, _)| n).collect();
        assert_eq!(names, ["SAVE", "SAVE-2", "_pcsx2_superblock"]);
        let names: Vec<&str> = t.children("SAVE").map(|(n, _)| n).collect();
        assert_eq!(names, ["a", "sub"]);
        assert_eq!(Tree::decode(&Tree::default().encode()).unwrap(), Tree::default());
    }

    #[test]
    fn rename_and_remove() {
        let mut t = sample();
        t.rename("SAVE", "_pcsx2_deleted_SAVE").unwrap();
        assert!(t.get("SAVE/a").is_none());
        assert_eq!(t.file("_pcsx2_deleted_SAVE/sub/b").unwrap().len(), 3000);
        assert!(Tree::decode(&t.encode()).is_ok());
        t.remove("_pcsx2_deleted_SAVE");
        assert_eq!(t.entries.len(), 2);
        assert!(t.insert("missing/x".into(), 0, EntryKind::Dir).is_err());
        assert!(t.insert("a/../b".into(), 0, EntryKind::Dir).is_err());
    }

    #[test]
    fn rejects_malformed_images() {
        let img = sample().encode();
        for n in [0, 4, 9, 12, img.len() - 1] {
            if n == MAGIC.len() {
                continue; // an empty tree
            }
            assert!(Tree::decode(&img[..n]).is_err(), "truncated to {n}");
        }
        // An entry whose parent is missing.
        let mut t = Tree::default();
        t.entries.insert("a/b".into(), Entry { mtime: 0, kind: EntryKind::Dir });
        assert!(Tree::decode(&t.encode()).is_err());
        // A file used as a directory.
        let mut t = Tree::default();
        t.entries.insert("a".into(), Entry { mtime: 0, kind: EntryKind::File(vec![]) });
        t.entries.insert("a/b".into(), Entry { mtime: 0, kind: EntryKind::Dir });
        assert!(Tree::decode(&t.encode()).is_err());
        for bad in ["", ".", "a//b", "a\\b", "..", "x/.."] {
            let mut t = Tree::default();
            t.entries.insert(bad.into(), Entry { mtime: 0, kind: EntryKind::Dir });
            assert!(Tree::decode(&t.encode()).is_err(), "{bad:?}");
        }
    }
}

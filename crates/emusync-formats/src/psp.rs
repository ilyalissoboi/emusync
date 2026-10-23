//! `psp.savedata`: a PSP memory stick's `PSP/SAVEDATA` folder (DESIGN §27), where a PSP and
//! PPSSPP keep saves: one folder per save, named after the game's product code and the save's
//! name (`ULUS10041S0`), holding `PARAM.SFO`, the save's icons and the game's data files.
//!
//! It is a [`ContainerKind::UnitDirs`] container: each save folder is a unit named as the
//! folder, which the core reads and replaces on its own, so a large memory stick costs only the
//! saves that changed. Games also install data there (the PSP's game data install utility:
//! hundreds of MB, made again from the game disc). A folder over [`MAX_SAVE_SIZE`] is taken for
//! that and left alone. Files directly in `SAVEDATA` (PPSSPP's `.nomedia` on Android) are not
//! units either.
//!
//! A unit's canonical form is the folder's tree image ([`Tree`]) with paths relative to the
//! folder and every modification time zero, so the same save copied to another device hashes
//! the same. The times travel in the `psp.mtimes` meta and are restored on apply: PPSSPP and
//! the PSP show a save's date from them.

use crate::tree::{self, EntryKind, Tree};
use crate::{ApplyOp, Canonical, Confidence, ContainerFormat, ContainerKind, FormatError, Result, UnitRef};
use std::collections::{BTreeMap, HashMap};

pub struct PspSavedata;

/// The largest save folder synced: saves are rarely over 2 MB, game install data is far
/// larger.
pub const MAX_SAVE_SIZE: u64 = 32 << 20;

/// Meta key of a save's modification times: one line per entry, `<unix seconds> <path>`, the
/// folder itself as `.`.
const MTIMES_META: &str = "psp.mtimes";
const FOLDER: &str = ".";

/// Whether a folder is a memory stick's save folder, by its name. PPSSPP and the PSP name it
/// `SAVEDATA`; a FAT memory stick read on another system may show it in another case.
pub fn is_savedata_folder(name: &str) -> bool {
    name.eq_ignore_ascii_case("SAVEDATA")
}

fn invalid(msg: impl Into<String>) -> FormatError {
    FormatError::Invalid(msg.into())
}

/// The save folders in an image: its root directories, by name.
fn saves(tree: &Tree) -> Vec<&str> {
    tree.children("").filter(|(_, e)| matches!(e.kind, EntryKind::Dir)).map(|(n, _)| n).collect()
}

/// The canonical unit of the save folder `name` in `tree`.
fn canonical(tree: &Tree, name: &str) -> Result<Canonical> {
    let folder = tree.get(name).filter(|e| matches!(e.kind, EntryKind::Dir));
    let folder = folder.ok_or_else(|| FormatError::NotFound(name.to_string()))?;
    let prefix = format!("{name}/");
    let mut body = Tree::default();
    let mut mtimes = vec![format!("{} {FOLDER}", folder.mtime)];
    // In path order, so a directory comes before what is in it.
    for (path, e) in tree.entries.range(prefix.clone()..).take_while(|(p, _)| p.starts_with(&prefix)) {
        let rel = &path[prefix.len()..];
        body.insert(rel.to_string(), 0, e.kind.clone())?;
        mtimes.push(format!("{} {rel}", e.mtime));
    }
    let meta = BTreeMap::from([(MTIMES_META.to_string(), mtimes.join("\n"))]);
    Ok(Canonical { name: name.to_string(), bytes: body.encode(), meta })
}

/// The modification times in a unit's meta, by path (`.` for the folder).
fn mtimes(c: &Canonical) -> HashMap<&str, i64> {
    let lines = c.meta.get(MTIMES_META).map(String::as_str).unwrap_or_default().lines();
    lines.filter_map(|l| l.split_once(' ')).filter_map(|(t, p)| Some((p, t.parse().ok()?))).collect()
}

/// The title of a save folder: its first nine characters when they are a product code, four
/// letters and five digits (`ULUS10041`, `NPJH50040`).
pub fn title_of(name: &str) -> Option<&str> {
    let b = name.as_bytes();
    let code = b.len() >= 9 && b[..4].iter().all(u8::is_ascii_uppercase) && b[4..9].iter().all(u8::is_ascii_digit);
    code.then(|| &name[..9])
}

impl ContainerFormat for PspSavedata {
    fn id(&self) -> &'static str {
        "psp.savedata"
    }

    fn kind(&self) -> ContainerKind {
        ContainerKind::UnitDirs
    }

    fn supports_units(&self) -> bool {
        true
    }

    /// Certain for a folder with a save folder in it (one with a `PARAM.SFO`). The core
    /// recognises a `SAVEDATA` folder by its name first, without reading it.
    fn probe(&self, image: &[u8]) -> Confidence {
        match Tree::decode(image) {
            Ok(t) if saves(&t).iter().any(|s| t.file(&tree::join(s, "PARAM.SFO")).is_some()) => Confidence::Certain,
            _ => Confidence::No,
        }
    }

    fn validate(&self, image: &[u8]) -> Result<()> {
        Tree::decode(image).map(|_| ())
    }

    fn list(&self, image: &[u8]) -> Result<Vec<UnitRef>> {
        Ok(saves(&Tree::decode(image)?).into_iter().map(|n| UnitRef { name: n.to_string() }).collect())
    }

    fn extract(&self, image: &[u8], unit: &UnitRef) -> Result<Canonical> {
        canonical(&Tree::decode(image)?, &unit.name)
    }

    fn extract_all(&self, image: &[u8]) -> Result<Vec<Canonical>> {
        let tree = Tree::decode(image)?;
        saves(&tree).into_iter().map(|n| canonical(&tree, n)).collect()
    }

    fn titles(&self, unit_name: &str) -> Vec<String> {
        title_of(unit_name).map(|t| format!("psp:{t}")).into_iter().collect()
    }

    fn apply(&self, image: &[u8], ops: &[ApplyOp]) -> Result<Vec<u8>> {
        let mut tree = Tree::decode(image)?;
        for op in ops {
            match op {
                ApplyOp::Delete(name) => tree.remove(name),
                ApplyOp::Put(c) => {
                    if !tree::valid_segment(&c.name) {
                        return Err(invalid(format!("{:?} is not a save folder name", c.name)));
                    }
                    let body = Tree::decode(&c.bytes)?;
                    let times = mtimes(c);
                    let time = |p: &str| times.get(p).copied().unwrap_or(0);
                    tree.remove(&c.name);
                    tree.insert(c.name.clone(), time(FOLDER), EntryKind::Dir)?;
                    for (rel, e) in &body.entries {
                        tree.insert(tree::join(&c.name, rel), time(rel), e.kind.clone())?;
                    }
                }
            }
        }
        Ok(tree.encode())
    }

    /// An empty folder, as PPSSPP creates `SAVEDATA` on a new memory stick.
    fn empty(&self, _file_name: &str) -> Option<Vec<u8>> {
        Some(Tree::default().encode())
    }

    fn max_unit_size(&self) -> Option<u64> {
        Some(MAX_SAVE_SIZE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(t: &mut Tree, path: &str, mtime: i64, data: &[u8]) {
        t.insert(path.to_string(), mtime, EntryKind::File(data.to_vec())).unwrap();
    }

    fn dir(t: &mut Tree, path: &str, mtime: i64) {
        t.insert(path.to_string(), mtime, EntryKind::Dir).unwrap();
    }

    /// A save folder as PPSSPP writes one, plus a nested folder some games use.
    fn stick(mtime: i64) -> Tree {
        let mut t = Tree::default();
        file(&mut t, ".nomedia", 1, b"");
        dir(&mut t, "ULUS10041S0", mtime);
        file(&mut t, "ULUS10041S0/DATA.BIN", mtime, &[7; 5000]);
        file(&mut t, "ULUS10041S0/ICON0.PNG", mtime, b"png");
        file(&mut t, "ULUS10041S0/PARAM.SFO", mtime + 1, b"\0PSF");
        dir(&mut t, "ULUS10041S0/sub", mtime);
        file(&mut t, "ULUS10041S0/sub/x", mtime + 2, b"x");
        dir(&mut t, "NPJH50040-SYS", mtime);
        file(&mut t, "NPJH50040-SYS/PARAM.SFO", mtime, b"\0PSF");
        t
    }

    #[test]
    fn units_are_the_save_folders() {
        let img = stick(100).encode();
        assert_eq!(PspSavedata.probe(&img), Confidence::Certain);
        let names: Vec<String> = PspSavedata.list(&img).unwrap().into_iter().map(|u| u.name).collect();
        assert_eq!(names, ["NPJH50040-SYS", "ULUS10041S0"]);
        assert_eq!(PspSavedata.titles("ULUS10041S0"), ["psp:ULUS10041"]);
        assert_eq!(PspSavedata.titles("NPJH50040-SYS"), ["psp:NPJH50040"]);
        assert!(PspSavedata.titles("ulus10041").is_empty());
        assert!(PspSavedata.titles("MYGAME").is_empty());
        assert_eq!(PspSavedata.probe(&Tree::default().encode()), Confidence::No);
        assert!(is_savedata_folder("SAVEDATA") && is_savedata_folder("savedata") && !is_savedata_folder("GAME"));
    }

    /// The same save with other modification times is the same unit, and the times come back
    /// on apply.
    #[test]
    fn times_are_meta_and_come_back() {
        let a = PspSavedata.extract_all(&stick(100).encode()).unwrap();
        let b = PspSavedata.extract_all(&stick(900).encode()).unwrap();
        assert_eq!(a[1].bytes, b[1].bytes);
        assert_ne!(a[1].meta, b[1].meta);

        let mut other = Tree::default();
        file(&mut other, "ULUS99999X", 5, b"a file, not a save");
        let out = PspSavedata.apply(&other.encode(), &[ApplyOp::Put(a[1].clone())]).unwrap();
        let out = Tree::decode(&out).unwrap();
        let src = stick(100);
        for (path, e) in src.entries.iter().filter(|(p, _)| p.starts_with("ULUS10041S0")) {
            assert_eq!(out.get(path), Some(e), "{path}");
        }
        assert_eq!(out.file("ULUS99999X"), Some(&b"a file, not a save"[..]));
        assert_eq!(PspSavedata.extract_all(&out.encode()).unwrap(), vec![a[1].clone()]);
    }

    #[test]
    fn put_replaces_and_delete_removes() {
        let img = stick(100).encode();
        let mut newer = stick(200);
        file(&mut newer, "ULUS10041S0/DATA.BIN", 200, &[8; 6000]);
        newer.remove("ULUS10041S0/sub");
        let save = PspSavedata.extract(&newer.encode(), &UnitRef { name: "ULUS10041S0".into() }).unwrap();
        let out = PspSavedata.apply(&img, &[ApplyOp::Put(save.clone()), ApplyOp::Delete("NPJH50040-SYS".into())]);
        let out = Tree::decode(&out.unwrap()).unwrap();
        assert_eq!(out.file("ULUS10041S0/DATA.BIN"), Some(&[8; 6000][..]));
        assert!(out.get("ULUS10041S0/sub").is_none());
        assert!(out.get("NPJH50040-SYS").is_none());
        assert_eq!(out.file(".nomedia"), Some(&b""[..]));
        assert_eq!(PspSavedata.extract_all(&out.encode()).unwrap(), vec![save]);
    }

    #[test]
    fn bad_names_and_content_are_refused() {
        let mut c = PspSavedata.extract_all(&stick(1).encode()).unwrap().remove(0);
        c.name = "../evil".into();
        assert!(PspSavedata.apply(&Tree::default().encode(), &[ApplyOp::Put(c.clone())]).is_err());
        c.name = "ULUS10041S0".into();
        c.bytes = b"not a tree".to_vec();
        assert!(PspSavedata.apply(&Tree::default().encode(), &[ApplyOp::Put(c)]).is_err());
    }
}

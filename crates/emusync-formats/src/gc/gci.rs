//! `gc.gci`: Dolphin's GCI folders (DESIGN §25), the default memory card type in Dolphin:
//! `GC/<region>/Card A/` holds one `.gci` file per save, each the save's 64-byte directory entry
//! followed by its data blocks.
//!
//! Dolphin builds a virtual card from the folder when a game starts, and writes saves back to
//! their files as the game changes them. It reads every `*.gci` file whatever its name, skips one
//! that is malformed or has the same identity as a file already loaded, and names files it
//! creates `<maker>-<gamecode>-<filename>.gci` (escaped). This adapter does the same: units are
//! the root `.gci` files in path order, malformed and duplicate ones are left alone (neither
//! synced nor deleted), a replaced save keeps its file, and a new one gets Dolphin's file name.
//! Other files (Dolphin's `MC_SYSTEM_AREA`) are kept as they are.
//!
//! A save is written with its first block as `0xFFFF` and, for the copy-protected saves (see the
//! [module docs](super)), the card serial zeroed: Dolphin assigns blocks and fills in the serial
//! for its virtual card when it loads the folder.

use super::{Save, BLOCK, DENTRY, GC_EPOCH_UNIX};
use crate::tree::{EntryKind, Tree};
use crate::{ApplyOp, Canonical, Confidence, ContainerFormat, ContainerKind, FormatError, Result, UnitRef};

pub struct GciFolder;

/// Parses a `.gci` file.
pub fn parse_gci(bytes: &[u8]) -> Result<Save> {
    if bytes.len() < DENTRY || (bytes.len() - DENTRY) % BLOCK != 0 {
        return Err(FormatError::Invalid(format!("{} bytes is not a GCI file", bytes.len())));
    }
    let save = Save { dentry: bytes[..DENTRY].try_into().unwrap(), data: bytes[DENTRY..].to_vec() };
    save.check()?;
    Ok(save)
}

/// A `.gci` file's bytes.
pub fn gci_bytes(save: &Save) -> Vec<u8> {
    let mut out = save.with_placement(0xFFFF).to_vec();
    out.extend_from_slice(&save.data);
    out
}

fn is_gci(path: &str) -> bool {
    !path.contains('/') && path.len() > 4 && path[path.len() - 4..].eq_ignore_ascii_case(".gci")
}

/// Dolphin's `EscapeFileName`: control characters and `"*/:<>?\|` and DEL become `__xx__`,
/// existing double underscores are escaped first, and names made only of dots are escaped.
pub fn escape_file_name(name: &str) -> String {
    if !name.is_empty() && name.chars().all(|c| c == '.') {
        return name.replace('.', "__2e__");
    }
    let mut out = String::new();
    for c in name.replace("__", "__5f____5f__").chars() {
        if (c as u32) <= 0x1F || "\"*/:<>?\\|\x7f".contains(c) {
            out.push_str(&format!("__{:02x}__", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

/// The file name Dolphin gives a new save: `<maker>-<gamecode>-<filename>.gci`.
pub fn default_file_name(save: &Save) -> String {
    let (game, maker, file) = save.identity();
    // Dolphin decodes these as Windows-1252 (or Shift JIS on Japanese cards); for the ASCII names
    // of real saves that is the same as Latin-1, which is what this uses for the rest.
    let text = |b: &[u8]| b.iter().map(|&c| c as char).take_while(|&c| c != '\0').collect::<String>();
    escape_file_name(&format!("{}-{}-{}.gci", text(maker), text(game), text(file)))
}

/// The logical card name for a Dolphin GCI folder, from its path's last two components:
/// `GC/USA/Card A` → `MemoryCardA.USA`, the stem of the raw card Dolphin keeps for the same slot
/// and region (`GC/MemoryCardA.USA.raw`), so the two are the same card. `None` for other folders.
pub fn dolphin_card_name(region: &str, folder: &str) -> Option<String> {
    let slot = folder.strip_prefix("Card ").filter(|s| matches!(*s, "A" | "B"))?;
    let valid = !region.is_empty() && region.chars().all(|c| c.is_ascii_alphanumeric());
    valid.then(|| format!("MemoryCard{slot}.{region}"))
}

/// The folder's usable saves: (path, save), in path order, first of each identity only.
fn saves(tree: &Tree) -> Vec<(String, Save)> {
    let mut out: Vec<(String, Save)> = Vec::new();
    for (path, entry) in &tree.entries {
        let EntryKind::File(bytes) = &entry.kind else { continue };
        if !is_gci(path) {
            continue;
        }
        let Ok(save) = parse_gci(bytes) else { continue };
        if out.iter().any(|(_, s)| s.identity() == save.identity()) {
            continue;
        }
        out.push((path.clone(), save));
    }
    out
}

impl ContainerFormat for GciFolder {
    fn id(&self) -> &'static str {
        "gc.gci"
    }

    fn kind(&self) -> ContainerKind {
        ContainerKind::Directory
    }

    fn supports_units(&self) -> bool {
        true
    }

    /// Certain for a folder with at least one valid `.gci` file at its root. An empty GCI folder
    /// can't be told from any other empty folder: it is registered with the format given.
    fn probe(&self, image: &[u8]) -> Confidence {
        match Tree::decode(image) {
            Ok(tree) if !saves(&tree).is_empty() => Confidence::Certain,
            _ => Confidence::No,
        }
    }

    fn validate(&self, image: &[u8]) -> Result<()> {
        Tree::decode(image).map(|_| ())
    }

    fn list(&self, image: &[u8]) -> Result<Vec<UnitRef>> {
        Ok(saves(&Tree::decode(image)?).into_iter().map(|(_, s)| UnitRef { name: s.unit_name() }).collect())
    }

    fn extract(&self, image: &[u8], unit: &UnitRef) -> Result<Canonical> {
        saves(&Tree::decode(image)?)
            .into_iter()
            .find(|(_, s)| s.unit_name() == unit.name)
            .map(|(_, s)| super::canonical(&s))
            .ok_or_else(|| FormatError::NotFound(unit.name.clone()))
    }

    fn extract_all(&self, image: &[u8]) -> Result<Vec<Canonical>> {
        Ok(saves(&Tree::decode(image)?).iter().map(|(_, s)| super::canonical(s)).collect())
    }

    fn titles(&self, unit_name: &str) -> Vec<String> {
        super::titles_of(unit_name)
    }

    fn apply(&self, image: &[u8], ops: &[ApplyOp]) -> Result<Vec<u8>> {
        let mut tree = Tree::decode(image)?;
        for op in ops {
            match op {
                ApplyOp::Delete(name) => {
                    if let Some((path, _)) = saves(&tree).into_iter().find(|(_, s)| s.unit_name() == *name) {
                        tree.remove(&path);
                    }
                }
                ApplyOp::Put(c) => {
                    let save = super::decode(c)?;
                    let path = match saves(&tree).into_iter().find(|(_, s)| s.unit_name() == c.name) {
                        Some((path, _)) => path,
                        None => {
                            // Dolphin's name, made unique if a file (perhaps malformed) has it.
                            let base = default_file_name(&save);
                            let stem = base[..base.len() - 4].to_string();
                            let mut path = base;
                            let mut n = 1;
                            while tree.get(&path).is_some() {
                                path = format!("{stem} ({n}).gci");
                                n += 1;
                            }
                            path
                        }
                    };
                    let mtime = GC_EPOCH_UNIX + save.mod_time() as i64;
                    tree.remove(&path);
                    tree.insert(path, mtime, EntryKind::File(gci_bytes(&save)))?;
                }
            }
        }
        Ok(tree.encode())
    }
}

#[cfg(test)]
mod tests {
    use super::super::canonical;
    use super::super::card::{blank_card, GcCard};
    use super::super::tests::save;
    use super::*;

    fn folder(files: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut t = Tree::default();
        for (p, b) in files {
            t.insert(p.to_string(), 0, EntryKind::File(b.clone())).unwrap();
        }
        t.encode()
    }

    #[test]
    fn dolphin_folders_name_the_raw_card() {
        assert_eq!(dolphin_card_name("USA", "Card A").as_deref(), Some("MemoryCardA.USA"));
        assert_eq!(dolphin_card_name("JAP", "Card B").as_deref(), Some("MemoryCardB.JAP"));
        assert_eq!(dolphin_card_name("USA", "Card C"), None);
        assert_eq!(dolphin_card_name("", "Card A"), None);
        assert_eq!(dolphin_card_name("my saves", "Card A"), None);
    }

    #[test]
    fn escaping_matches_dolphin() {
        assert_eq!(escape_file_name("01-GM8E-MetroidPrime A.gci"), "01-GM8E-MetroidPrime A.gci");
        assert_eq!(escape_file_name("a:b?c"), "a__3a__b__3f__c");
        assert_eq!(escape_file_name("x__y"), "x__5f____5f__y");
        assert_eq!(escape_file_name(".."), "__2e____2e__");
        let s = save(b"GM8E", b"01", "MetroidPrime A", 1, 1);
        assert_eq!(default_file_name(&s), "01-GM8E-MetroidPrime A.gci");
    }

    #[test]
    fn folder_round_trip() {
        let m = save(b"GM8E", b"01", "MetroidPrime A", 1, 1);
        let r = save(b"GSWE", b"64", "RogueLeader", 3, 2);
        let img = folder(&[
            ("01-GM8E-MetroidPrime A.gci", gci_bytes(&m)),
            ("MC_SYSTEM_AREA", vec![1, 2, 3]),
            ("broken.gci", vec![0; 10]),
        ]);
        assert_eq!(GciFolder.probe(&img), Confidence::Certain);
        assert_eq!(GciFolder.extract_all(&img).unwrap(), vec![canonical(&m)]);

        let img = GciFolder.apply(&img, &[ApplyOp::Put(canonical(&r))]).unwrap();
        let tree = Tree::decode(&img).unwrap();
        assert!(tree.file("64-GSWE-RogueLeader.gci").is_some());
        assert_eq!(tree.file("MC_SYSTEM_AREA"), Some(&[1u8, 2, 3][..]), "other files are kept");
        assert!(tree.file("broken.gci").is_some(), "malformed files are left alone");

        // Replacing writes the same file; deleting removes it.
        let m2 = save(b"GM8E", b"01", "MetroidPrime A", 2, 9);
        let img = GciFolder.apply(&img, &[ApplyOp::Put(canonical(&m2)), ApplyOp::Delete(r.unit_name())]).unwrap();
        let tree = Tree::decode(&img).unwrap();
        assert_eq!(parse_gci(tree.file("01-GM8E-MetroidPrime A.gci").unwrap()).unwrap().data, m2.data);
        assert!(tree.file("64-GSWE-RogueLeader.gci").is_none());
    }

    #[test]
    fn duplicates_are_left_alone() {
        let a = save(b"GM8E", b"01", "m", 1, 1);
        let b = save(b"GM8E", b"01", "m", 1, 2);
        let img = folder(&[("a.gci", gci_bytes(&a)), ("b.gci", gci_bytes(&b))]);
        assert_eq!(GciFolder.extract_all(&img).unwrap(), vec![canonical(&a)], "the first by path wins, like Dolphin");
    }

    /// The same save in a GCI folder and on a raw card is the same unit.
    #[test]
    fn folders_and_cards_interoperate() {
        let pso = save(b"GPOE", b"8P", "PSO_SYSTEM", 3, 7);
        let m = save(b"GM8E", b"01", "MetroidPrime A", 1, 1);
        let card = GcCard
            .apply(
                &blank_card(16, *b"0123456789ab", 5, false),
                &[ApplyOp::Put(canonical(&pso)), ApplyOp::Put(canonical(&m))],
            )
            .unwrap();
        // A card lists saves in directory order, a folder by file name: compare them sorted.
        let sorted = |mut v: Vec<Canonical>| {
            v.sort_by(|a, b| a.name.cmp(&b.name));
            v
        };
        let units = sorted(GcCard.extract_all(&card).unwrap());
        let puts = units.iter().cloned().map(ApplyOp::Put).collect::<Vec<_>>();
        let dir = GciFolder.apply(&folder(&[]), &puts).unwrap();
        assert_eq!(sorted(GciFolder.extract_all(&dir).unwrap()), units);
        let back = GcCard.apply(&blank_card(16, *b"zyxwvutsrqpo", 9, false), &puts).unwrap();
        assert_eq!(sorted(GcCard.extract_all(&back).unwrap()), units);
    }
}

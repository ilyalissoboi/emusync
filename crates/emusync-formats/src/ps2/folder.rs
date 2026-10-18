//! `ps2.card.folder`: PCSX2 folder memory cards (DESIGN §5.2, §18).
//!
//! A folder card is a host directory that PCSX2 turns into a card image while a game runs:
//!
//! - `_pcsx2_superblock`: the card's first erase block (8 KiB, no ECC). PCSX2 creates new folder
//!   cards with an empty file and treats the card as unformatted until byte `0x16` is `0x6F`
//!   (the `o` of "Format" in the magic). An unformatted card shows no saves.
//! - One directory per save in the root. Root-level files, and every name starting with
//!   `_pcsx2_`, are ignored by PCSX2. Deleted saves are renamed to `_pcsx2_deleted_<name>`.
//! - `_pcsx2_index` in each directory: YAML with the directory's own timestamps (`$ROOT`) and,
//!   per file, its position (`order`) and timestamps, all as Unix seconds (UTC).
//! - `_pcsx2_meta_directory` in a directory and `_pcsx2_meta/<file>` next to files: the raw
//!   512-byte directory entry, written only when it is not the default (another mode, a
//!   non-zero attribute, or a name that had to change on the host). When present it supplies
//!   the name, mode, attribute and timestamps.
//!
//! Units and their canonical form are the same as for `ps2.card` (one unit per save, built
//! with [`super::canonical`]), so a folder card and a card file of the same logical card sync
//! with each other directly. The adapter works on [`Tree`] images; the core does the I/O.

use super::index;
use super::mcfs::{self, check_name, Node, NodeKind, MODE_DIR, MODE_DIR_DEFAULT, MODE_EXISTS, MODE_FILE};
use super::mcfs::{blank_card, MODE_FILE_DEFAULT};
use super::{canonical, decode, name_to_bytes, name_to_string, titles_of};
use crate::tree::{join, EntryKind, Tree};
use crate::{ApplyOp, Canonical, Confidence, ContainerFormat, ContainerKind, FormatError, Result, UnitRef};

pub const SUPERBLOCK: &str = "_pcsx2_superblock";
const INDEX: &str = "_pcsx2_index";
const META_DIRECTORY: &str = "_pcsx2_meta_directory";
const META: &str = "_pcsx2_meta";
const RESERVED: &str = "_pcsx2_";
const DELETED: &str = "_pcsx2_deleted_";
/// One erase block: what PCSX2 reads from and writes to `_pcsx2_superblock`.
const SUPERBLOCK_LEN: usize = 8192;
const ENTRY_LEN: usize = 512;
/// Directory nesting PCSX2's folder cards and the MCFS parser accept.
const MAX_DEPTH: usize = 8;
/// Time for entries without a recorded one: 2000-01-01 00:00:00.
const FALLBACK_TIME: [u8; 8] = [0, 0, 0, 0, 1, 1, 0xD0, 0x07];
/// Characters PCSX2 replaces with `_` in host names (`FileAccessHelper::CleanMemcardFilename`).
const UNSAFE_CHARS: [char; 7] = ['\\', '%', ':', '|', '"', '<', '>'];

pub struct Ps2Folder;

fn invalid(msg: impl Into<String>) -> FormatError {
    FormatError::Invalid(msg.into())
}

// ---- timestamps ----

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, u8, u8) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// A card timestamp as Unix seconds, as PCSX2's `ToTime` computes it: UTC, month and year
/// clamped below at January 1900, out-of-range fields carried over (`timegm`).
pub fn ps2_time_to_unix(t: [u8; 8]) -> i64 {
    let (sec, min, hour, day, month) = (t[1] as i64, t[2] as i64, t[3] as i64, t[4] as i64, t[5] as i64);
    let year = u16::from_le_bytes([t[6], t[7]]) as i64;
    let mon0 = (month - 1).max(0);
    let days = days_from_civil(year.max(1900) + mon0 / 12, mon0 % 12 + 1, 1) + day - 1;
    days * 86400 + hour * 3600 + min * 60 + sec
}

/// Inverse of [`ps2_time_to_unix`] (PCSX2's `FromTime`): UTC, reserved byte 0.
pub fn unix_to_ps2_time(t: i64) -> [u8; 8] {
    let (y, m, d) = civil_from_days(t.div_euclid(86400));
    let s = t.rem_euclid(86400);
    let year = y.clamp(0, u16::MAX as i64) as u16;
    let [y0, y1] = year.to_le_bytes();
    [0, (s % 60) as u8, (s / 60 % 60) as u8, (s / 3600) as u8, d, m, y0, y1]
}

// ---- host names ----

/// The host name PCSX2 uses for a card name, and whether it differs from the card name (in
/// which case the real name is kept in a metadata file). Names that aren't UTF-8 are decoded
/// as Latin-1; names of 32 bytes are kept in metadata because PCSX2 reads back at most 31.
pub fn host_name(name: &[u8]) -> (String, bool) {
    let (mut s, mut changed) = match std::str::from_utf8(name) {
        Ok(s) => (s.to_string(), false),
        Err(_) => (name_to_string(name), true),
    };
    if s.contains(UNSAFE_CHARS) {
        s = s.replace(UNSAFE_CHARS, "_");
        changed = true;
    }
    let trimmed = s.trim_end_matches(['.', ' ']).len();
    if trimmed < s.len() {
        s = format!("{}{}", &s[..trimmed], "_".repeat(s.len() - trimmed));
        changed = true;
    }
    (s, changed || name.len() > 31)
}

fn same_host_name(a: &str, b: &str) -> bool {
    // Windows and macOS compare names without case; reject what would clash there.
    a.to_lowercase() == b.to_lowercase()
}

// ---- reading ----

/// Name bytes of a raw entry: up to the first NUL of the 32-byte name field.
fn entry_name(e: &[u8; ENTRY_LEN]) -> Vec<u8> {
    let field = &e[0x40..0x60];
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    field[..end].to_vec()
}

/// A node's name, mode, attribute and timestamps: from its metadata file when there is one,
/// else the host name, the default mode and the given times.
fn entry_from(meta: Option<&[u8]>, host: &str, default_mode: u16, times: ([u8; 8], [u8; 8])) -> Result<Node> {
    // PCSX2 copies at most 31 bytes of a host name into the entry.
    let host_bytes = host.as_bytes()[..host.len().min(31)].to_vec();
    let node = match meta {
        Some(raw) => {
            let mut e = [0u8; ENTRY_LEN];
            let n = raw.len().min(ENTRY_LEN);
            e[..n].copy_from_slice(&raw[..n]);
            Node {
                // As PCSX2: the name comes from the metadata only if it was read in full.
                name: if raw.len() >= 0x60 { entry_name(&e) } else { host_bytes },
                mode: u16::from_le_bytes([e[0], e[1]]) | MODE_EXISTS,
                attr: u32::from_le_bytes(e[0x20..0x24].try_into().unwrap()),
                created: e[0x08..0x10].try_into().unwrap(),
                modified: e[0x18..0x20].try_into().unwrap(),
                kind: NodeKind::File(vec![]),
            }
        }
        None => Node {
            name: host_bytes,
            mode: default_mode,
            attr: 0,
            created: times.0,
            modified: times.1,
            kind: NodeKind::File(vec![]),
        },
    };
    check_name(&node.name)?;
    Ok(node)
}

fn times(entry: Option<&index::IndexEntry>, host_mtime: i64) -> ([u8; 8], [u8; 8]) {
    let e = entry.copied().unwrap_or_default();
    (unix_to_ps2_time(e.created.unwrap_or(host_mtime)), unix_to_ps2_time(e.modified.unwrap_or(host_mtime)))
}

/// Reads the directory at `path` the way PCSX2's `AddFolder` loads it: subdirectories first
/// (in host order), then files by their `order` in the index. Files the index doesn't list
/// get negative positions in host order (PCSX2's "legacy" files), so they come first.
fn read_dir(tree: &Tree, path: &str, host: &str, host_mtime: i64, depth: usize) -> Result<Node> {
    if depth > MAX_DEPTH {
        return Err(invalid("directories nested too deeply"));
    }
    let ix = tree
        .file(&join(path, INDEX))
        .and_then(|b| std::str::from_utf8(b).ok())
        .and_then(index::parse)
        .unwrap_or_default();
    let mut node =
        entry_from(tree.file(&join(path, META_DIRECTORY)), host, MODE_DIR_DEFAULT, times(Some(&ix.root), host_mtime))?;
    if node.mode & MODE_DIR == 0 {
        return Err(invalid(format!("{path}: directory metadata without the directory mode bit")));
    }
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let mut legacy = -1i64;
    for (name, e) in tree.children(path) {
        if name.starts_with(RESERVED) {
            continue;
        }
        let child = join(path, name);
        match &e.kind {
            EntryKind::Dir => dirs.push(read_dir(tree, &child, name, e.mtime, depth + 1)?),
            EntryKind::File(data) => {
                let ie = ix.entries.get(name);
                let mut order = legacy;
                legacy -= 1;
                if let Some(o) = ie.and_then(|i| i.order) {
                    order = o;
                }
                let meta = tree.file(&join(&join(path, META), name));
                let mut f = entry_from(meta, name, MODE_FILE_DEFAULT, times(ie, e.mtime))?;
                if f.mode & MODE_FILE == 0 {
                    return Err(invalid(format!("{child}: file metadata without the file mode bit")));
                }
                f.kind = NodeKind::File(data.clone());
                files.push((order, f));
            }
        }
    }
    files.sort_by_key(|(o, _)| *o);
    let children: Vec<Node> = dirs.into_iter().chain(files.into_iter().map(|(_, f)| f)).collect();
    let mut names: Vec<&[u8]> = children.iter().map(|c| c.name.as_slice()).collect();
    names.sort();
    if names.windows(2).any(|w| w[0] == w[1]) {
        return Err(invalid(format!("{path}: two entries have the same name on the card")));
    }
    node.kind = NodeKind::Dir(children);
    Ok(node)
}

struct Parsed {
    tree: Tree,
    formatted: bool,
    /// Host directory name and contents of each save, in host order.
    saves: Vec<(String, Node)>,
}

fn is_formatted(superblock: &[u8]) -> bool {
    superblock.len() >= SUPERBLOCK_LEN && superblock[0x16] == 0x6F
}

/// Saves of a formatted card: the root directories PCSX2 loads.
fn read_saves(tree: &Tree) -> Result<Vec<(String, Node)>> {
    let mut saves = Vec::new();
    for (name, e) in tree.children("") {
        if matches!(e.kind, EntryKind::Dir) && !name.starts_with(RESERVED) {
            saves.push((name.to_string(), read_dir(tree, name, name, e.mtime, 1)?));
        }
    }
    let mut names: Vec<&[u8]> = saves.iter().map(|(_, n)| n.name.as_slice()).collect();
    names.sort();
    if names.windows(2).any(|w| w[0] == w[1]) {
        return Err(invalid("two saves have the same name on the card"));
    }
    Ok(saves)
}

fn parse(image: &[u8]) -> Result<Parsed> {
    let tree = Tree::decode(image)?;
    let sb = tree.file(SUPERBLOCK).ok_or_else(|| invalid("not a PCSX2 folder memory card (no _pcsx2_superblock)"))?;
    let formatted = is_formatted(sb);
    let saves = if formatted { read_saves(&tree)? } else { vec![] };
    Ok(Parsed { tree, formatted, saves })
}

/// A folder card PCSX2 created but the PS2 BIOS hasn't formatted yet (its superblock file is
/// empty or short). It holds no saves until the first apply formats it.
pub fn is_unformatted(image: &[u8]) -> bool {
    parse(image).is_ok_and(|p| !p.formatted)
}

// ---- writing ----

/// The raw 512-byte entry PCSX2 keeps in a metadata file.
fn raw_entry(node: &Node, length: u32) -> Vec<u8> {
    let mut e = vec![0u8; ENTRY_LEN];
    e[0x00..0x02].copy_from_slice(&node.mode.to_le_bytes());
    e[0x04..0x08].copy_from_slice(&length.to_le_bytes());
    e[0x08..0x10].copy_from_slice(&node.created);
    e[0x18..0x20].copy_from_slice(&node.modified);
    e[0x20..0x24].copy_from_slice(&node.attr.to_le_bytes());
    e[0x40..0x40 + node.name.len()].copy_from_slice(&node.name);
    e
}

fn unsupported(msg: impl Into<String>) -> FormatError {
    FormatError::Unsupported(msg.into())
}

/// Writes `node` as the directory `path`, the way PCSX2 flushes it: entries, metadata files
/// for non-default entries, and an index holding file order and every timestamp.
fn write_dir(tree: &mut Tree, path: &str, renamed: bool, node: &Node, depth: usize) -> Result<()> {
    let NodeKind::Dir(children) = &node.kind else { unreachable!("write_dir takes directories") };
    if depth > MAX_DEPTH {
        return Err(invalid("directories nested too deeply"));
    }
    let mtime = ps2_time_to_unix(node.modified);
    tree.insert(path.to_string(), mtime, EntryKind::Dir)?;
    if renamed || node.mode | MODE_EXISTS != MODE_DIR_DEFAULT || node.attr != 0 {
        let raw = raw_entry(node, children.len() as u32 + 2);
        tree.insert(join(path, META_DIRECTORY), mtime, EntryKind::File(raw))?;
    }
    let mut hosts: Vec<String> = Vec::new();
    let mut index_entries = Vec::new();
    for child in children {
        let (host, renamed) = host_name(&child.name);
        if host.starts_with(RESERVED) {
            return Err(unsupported(format!("PCSX2 ignores entries named {host:?}")));
        }
        if hosts.iter().any(|h| same_host_name(h, &host)) {
            return Err(unsupported(format!("{path}: two entries would share the host name {host:?}")));
        }
        hosts.push(host.clone());
        let child_path = join(path, &host);
        match &child.kind {
            NodeKind::Dir(_) => write_dir(tree, &child_path, renamed, child, depth + 1)?,
            NodeKind::File(data) => {
                let modified = ps2_time_to_unix(child.modified);
                tree.insert(child_path, modified, EntryKind::File(data.clone()))?;
                index_entries.push((
                    host.clone(),
                    index_entries.len() as i64 + 1,
                    ps2_time_to_unix(child.created),
                    modified,
                ));
                if renamed || child.mode | MODE_EXISTS != MODE_FILE_DEFAULT || child.attr != 0 {
                    let meta_dir = join(path, META);
                    if !tree.is_dir(&meta_dir) {
                        tree.insert(meta_dir.clone(), mtime, EntryKind::Dir)?;
                    }
                    let raw = raw_entry(child, data.len() as u32);
                    tree.insert(join(&meta_dir, &host), modified, EntryKind::File(raw))?;
                }
            }
        }
    }
    let text = index::write((ps2_time_to_unix(node.created), mtime), &index_entries);
    tree.insert(join(path, INDEX), mtime, EntryKind::File(text.into_bytes()))
}

/// Clusters a directory takes on the card, counted as PCSX2's
/// `CalculateRequiredClustersOfDirectory` does: two entries per cluster (including `.` and
/// `..`), plus the file data in whole clusters.
fn dir_clusters(node: &Node) -> u64 {
    match &node.kind {
        NodeKind::File(data) => (data.len() as u64).div_ceil(mcfs::CLUSTER as u64),
        NodeKind::Dir(children) => {
            (children.len() as u64 + 2).div_ceil(2) + children.iter().map(dir_clusters).sum::<u64>()
        }
    }
}

/// Clusters the saves take, root directory included.
fn card_clusters<'a>(saves: impl Iterator<Item = &'a Node>) -> u64 {
    let (n, sum) = saves.fold((0u64, 0u64), |(n, s), node| (n + 1, s + dir_clusters(node)));
    (n + 2).div_ceil(2) + sum
}

/// Clusters PCSX2 lets saves use: the count the BIOS reports, `alloc_end` rounded down to a
/// thousand, minus one (7,999 on an 8 MB card). Saves past that are not loaded.
fn capacity(superblock: &[u8]) -> u64 {
    let alloc_end = u32::from_le_bytes(superblock[0x38..0x3C].try_into().unwrap()) as u64;
    (alloc_end / 1000 * 1000).saturating_sub(1)
}

impl ContainerFormat for Ps2Folder {
    fn id(&self) -> &'static str {
        "ps2.card.folder"
    }

    fn kind(&self) -> ContainerKind {
        ContainerKind::Directory
    }

    fn supports_units(&self) -> bool {
        true
    }

    fn probe(&self, image: &[u8]) -> Confidence {
        match Tree::decode(image) {
            Ok(t) if t.file(SUPERBLOCK).is_some() => Confidence::Certain,
            Ok(_) => Confidence::Maybe,
            Err(_) => Confidence::No,
        }
    }

    fn validate(&self, image: &[u8]) -> Result<()> {
        parse(image).map(|_| ())
    }

    fn list(&self, image: &[u8]) -> Result<Vec<UnitRef>> {
        Ok(parse(image)?.saves.iter().map(|(_, n)| UnitRef { name: name_to_string(&n.name) }).collect())
    }

    fn extract(&self, image: &[u8], unit: &UnitRef) -> Result<Canonical> {
        let want = name_to_bytes(&unit.name)?;
        parse(image)?
            .saves
            .iter()
            .find(|(_, n)| n.name == want)
            .map(|(_, n)| canonical(n))
            .ok_or_else(|| FormatError::NotFound(unit.name.clone()))
    }

    fn extract_all(&self, image: &[u8]) -> Result<Vec<Canonical>> {
        Ok(parse(image)?.saves.iter().map(|(_, n)| canonical(n)).collect())
    }

    fn titles(&self, unit_name: &str) -> Vec<String> {
        titles_of(unit_name)
    }

    /// Writes each `Put` save as its directory (replacing an existing one) and renames each
    /// deleted save to `_pcsx2_deleted_<name>`, as PCSX2 does. Everything else in the folder is
    /// kept. An unformatted card is formatted first (the superblock of a standard 8 MB card),
    /// stamped with the first incoming save's creation time.
    fn apply(&self, image: &[u8], ops: &[ApplyOp]) -> Result<Vec<u8>> {
        let parsed = parse(image)?;
        let mut tree = parsed.tree;
        let mut saves = parsed.saves;
        let before = card_clusters(saves.iter().map(|(_, n)| n));
        if !parsed.formatted {
            let first_put = ops.iter().find_map(|op| match op {
                ApplyOp::Put(c) => Some(decode(c, FALLBACK_TIME).map(|n| n.created)),
                ApplyOp::Delete(_) => None,
            });
            if let Some(time) = first_put {
                let time = time?;
                let sb = blank_card(mcfs::CARD_CLUSTERS[0], false, time)[..SUPERBLOCK_LEN].to_vec();
                tree.insert(SUPERBLOCK.into(), ps2_time_to_unix(time), EntryKind::File(sb))?;
                // Directories already in the folder become visible once the card is formatted.
                saves = read_saves(&tree)?;
            }
        }
        for op in ops {
            match op {
                ApplyOp::Put(c) => {
                    let node = decode(c, FALLBACK_TIME)?;
                    if !matches!(node.kind, NodeKind::Dir(_)) {
                        return Err(unsupported("PCSX2 folder cards only hold directories in the root"));
                    }
                    if let Some(pos) = saves.iter().position(|(_, n)| n.name == node.name) {
                        let (host, _) = saves.remove(pos);
                        tree.remove(&host);
                    }
                    let (host, renamed) = host_name(&node.name);
                    if host.starts_with(RESERVED) {
                        return Err(unsupported(format!("PCSX2 ignores saves named {host:?}")));
                    }
                    if tree.children("").any(|(name, _)| same_host_name(name, &host)) {
                        return Err(unsupported(format!("{host:?} clashes with another entry in the folder")));
                    }
                    write_dir(&mut tree, &host, renamed, &node, 1)?;
                    saves.push((host, node));
                }
                ApplyOp::Delete(name) => {
                    let want = name_to_bytes(name)?;
                    if let Some(pos) = saves.iter().position(|(_, n)| n.name == want) {
                        let (host, _) = saves.remove(pos);
                        tree.rename(&host, &format!("{DELETED}{host}"))?;
                    }
                }
            }
        }
        if let Some(sb) = tree.file(SUPERBLOCK).filter(|sb| is_formatted(sb)) {
            let after = card_clusters(saves.iter().map(|(_, n)| n));
            if after > capacity(sb) && after > before {
                return Err(FormatError::NoSpace);
            }
        }
        Ok(tree.encode())
    }
}

#[cfg(test)]
mod tests {
    use super::super::encode;
    use super::*;
    use crate::Ps2Card;

    const T0: [u8; 8] = [0, 1, 2, 3, 4, 5, 0xD2, 0x07];
    const T1: [u8; 8] = [0, 9, 9, 9, 9, 9, 0xEA, 0x07];

    fn file(name: &[u8], data: &[u8], t: [u8; 8]) -> Node {
        Node {
            name: name.to_vec(),
            mode: MODE_FILE_DEFAULT,
            attr: 0,
            created: t,
            modified: t,
            kind: NodeKind::File(data.to_vec()),
        }
    }

    fn save(name: &[u8], children: Vec<Node>, t: [u8; 8]) -> Node {
        Node {
            name: name.to_vec(),
            mode: MODE_DIR_DEFAULT,
            attr: 0,
            created: t,
            modified: t,
            kind: NodeKind::Dir(children),
        }
    }

    fn gta(t: [u8; 8]) -> Node {
        save(b"BASLUS-20062GTA3", vec![file(b"icon.sys", &[1; 964], t), file(b"GTA3SAVE", &[2; 4000], t)], t)
    }

    /// A folder card as PCSX2 creates it: a directory with an empty superblock file.
    fn new_card() -> Vec<u8> {
        let mut t = Tree::default();
        t.insert(SUPERBLOCK.into(), 0, EntryKind::File(vec![])).unwrap();
        t.encode()
    }

    #[test]
    fn time_conversion() {
        for t in [0i64, 1, 951_782_400, 1_700_000_000, -86_401, 4_102_444_800] {
            assert_eq!(ps2_time_to_unix(unix_to_ps2_time(t)), t);
        }
        // 2002-05-04 03:02:01 UTC
        assert_eq!(ps2_time_to_unix(T0), 1_020_481_321);
        assert_eq!(unix_to_ps2_time(1_020_481_321), T0);
        // timegm carries over: month 13 is January of the next year, day 0 the last of the month.
        assert_eq!(ps2_time_to_unix([0, 0, 0, 0, 1, 13, 0xD2, 0x07]), ps2_time_to_unix([0, 0, 0, 0, 1, 1, 0xD3, 0x07]));
        assert_eq!(ps2_time_to_unix([0, 0, 0, 0, 0, 3, 0xD0, 0x07]), ps2_time_to_unix([0, 0, 0, 0, 29, 2, 0xD0, 0x07]));
    }

    #[test]
    fn host_names() {
        assert_eq!(host_name(b"BASLUS-20062GTA3"), ("BASLUS-20062GTA3".into(), false));
        assert_eq!(host_name(b"a:b|c"), ("a_b_c".into(), true));
        assert_eq!(host_name(b"end. ."), ("end___".into(), true));
        assert_eq!(host_name(b"caf\xe9"), ("caf\u{e9}".into(), true));
        assert!(host_name(&[b'x'; 32]).1, "PCSX2 reads back 31 bytes");
        assert!(!host_name(&[b'x'; 31]).1);
    }

    #[test]
    fn writes_what_pcsx2_reads() {
        let g = canonical(&gta(T0));
        let img = Ps2Folder.apply(&new_card(), &[ApplyOp::Put(g.clone())]).unwrap();
        let t = Tree::decode(&img).unwrap();
        // Formatted with the standard superblock, stamped with the save's time.
        let sb = t.file(SUPERBLOCK).unwrap();
        assert_eq!(sb, &blank_card(8192, false, T0)[..SUPERBLOCK_LEN]);
        assert!(is_formatted(sb));
        assert_eq!(capacity(sb), 7999);
        // Default entries: no metadata files, order and times in the index.
        assert!(t.file("BASLUS-20062GTA3/icon.sys").is_some());
        assert!(t.get("BASLUS-20062GTA3/_pcsx2_meta").is_none());
        assert!(t.get("BASLUS-20062GTA3/_pcsx2_meta_directory").is_none());
        let ix = index::parse(std::str::from_utf8(t.file("BASLUS-20062GTA3/_pcsx2_index").unwrap()).unwrap()).unwrap();
        assert_eq!(ix.root.modified, Some(1_020_481_321));
        assert_eq!(ix.entries["icon.sys"].order, Some(1));
        assert_eq!(ix.entries["GTA3SAVE"].order, Some(2));
        // Reads back as the same unit, order and timestamps included.
        assert_eq!(Ps2Folder.extract_all(&img).unwrap(), vec![g]);
    }

    #[test]
    fn same_units_as_card_files() {
        let g = canonical(&gta(T0));
        let odd = save(
            b"BESLES-5:odd.",
            vec![
                Node { mode: MODE_FILE_DEFAULT & !0x0080, attr: 7, ..file(b"x|y", b"data", T1) },
                file(b"empty", b"", T1),
                save(b"sub", vec![file(b"deep", &[5; 2000], T0)], T0),
            ],
            T1,
        );
        let odd = Node { attr: 0x10, ..odd };
        let o = canonical(&odd);
        let ops = [ApplyOp::Put(g.clone()), ApplyOp::Put(o.clone())];

        let folder = Ps2Folder.apply(&new_card(), &ops).unwrap();
        let file = Ps2Card.apply(&blank_card(8192, true, T0), &ops).unwrap();
        let mut from_folder = Ps2Folder.extract_all(&folder).unwrap();
        let mut from_file = Ps2Card.extract_all(&file).unwrap();
        from_folder.sort_by(|a, b| a.name.cmp(&b.name));
        from_file.sort_by(|a, b| a.name.cmp(&b.name));
        let bytes = |v: &[Canonical]| v.iter().map(|c| (c.name.clone(), c.bytes.clone())).collect::<Vec<_>>();
        assert_eq!(bytes(&from_folder), bytes(&from_file));
        // Names that changed on the host, odd modes and attributes live in metadata files.
        let t = Tree::decode(&folder).unwrap();
        assert!(t.file("BESLES-5_odd_/_pcsx2_meta_directory").is_some());
        assert!(t.file("BESLES-5_odd_/_pcsx2_meta/x_y").is_some());
        assert!(t.get("BESLES-5_odd_/_pcsx2_meta/empty").is_none());
        // Subdirectories come before files when PCSX2 loads a folder; everything else
        // (timestamps included) survives.
        let back = decode(&from_folder.iter().find(|c| c.name == "BESLES-5:odd.").unwrap().clone(), T0).unwrap();
        let NodeKind::Dir(children) = &back.kind else { panic!() };
        let names: Vec<&[u8]> = children.iter().map(|c| c.name.as_slice()).collect();
        assert_eq!(names, [b"sub".as_slice(), b"x|y", b"empty"]);
        assert_eq!((back.created, back.modified, back.attr), (T1, T1, 0x10));
        assert_eq!(encode(&back), o.bytes);
    }

    #[test]
    fn replace_delete_and_keep_the_rest() {
        let g = canonical(&gta(T0));
        let mut img = Ps2Folder.apply(&new_card(), &[ApplyOp::Put(g)]).unwrap();
        // Things PCSX2 or the user left in the folder.
        let mut t = Tree::decode(&img).unwrap();
        t.insert("_pcsx2_deleted_OLD".into(), 1, EntryKind::Dir).unwrap();
        t.insert("notes.txt".into(), 1, EntryKind::File(b"hi".to_vec())).unwrap();
        img = t.encode();

        let g2 = canonical(&save(b"BASLUS-20062GTA3", vec![file(b"GTA3SAVE", &[9; 10], T1)], T1));
        let img2 = Ps2Folder.apply(&img, &[ApplyOp::Put(g2.clone())]).unwrap();
        assert_eq!(Ps2Folder.extract_all(&img2).unwrap(), vec![g2]);
        let t2 = Tree::decode(&img2).unwrap();
        assert!(t2.get("BASLUS-20062GTA3/icon.sys").is_none(), "the old save's files are gone");
        assert!(t2.get("_pcsx2_deleted_OLD").is_some() && t2.get("notes.txt").is_some());

        let img3 = Ps2Folder.apply(&img2, &[ApplyOp::Delete("BASLUS-20062GTA3".into())]).unwrap();
        assert!(Ps2Folder.extract_all(&img3).unwrap().is_empty());
        let t3 = Tree::decode(&img3).unwrap();
        assert!(t3.file("_pcsx2_deleted_BASLUS-20062GTA3/GTA3SAVE").is_some(), "deleted as PCSX2 does");
        // Deleting what isn't there changes nothing.
        assert_eq!(Ps2Folder.apply(&img3, &[ApplyOp::Delete("NOPE".into())]).unwrap(), img3);
    }

    #[test]
    fn reads_folders_pcsx2_wrote() {
        // A save without metadata files, an index in flow style missing one file, and a file
        // whose real name is in `_pcsx2_meta`.
        let mut t = Tree::default();
        let sb = blank_card(8192, false, T0)[..SUPERBLOCK_LEN].to_vec();
        t.insert(SUPERBLOCK.into(), 0, EntryKind::File(sb)).unwrap();
        t.insert("stray.bin".into(), 0, EntryKind::File(vec![1])).unwrap();
        t.insert("SAVE".into(), 1_000_000_000, EntryKind::Dir).unwrap();
        let ix = "{$ROOT: {timeCreated: 1020481321, timeModified: 1020481321}, b: {order: 2, timeCreated: 5, \
                  timeModified: 6}, a_b: {order: 1}}";
        t.insert("SAVE/_pcsx2_index".into(), 0, EntryKind::File(ix.as_bytes().to_vec())).unwrap();
        t.insert("SAVE/b".into(), 0, EntryKind::File(b"bee".to_vec())).unwrap();
        t.insert("SAVE/a_b".into(), 0, EntryKind::File(b"ab".to_vec())).unwrap();
        t.insert("SAVE/legacy".into(), 1_000_000_000, EntryKind::File(b"old".to_vec())).unwrap();
        t.insert("SAVE/_pcsx2_meta".into(), 0, EntryKind::Dir).unwrap();
        let meta = raw_entry(&file(b"a:b", b"", T1), 2);
        t.insert("SAVE/_pcsx2_meta/a_b".into(), 0, EntryKind::File(meta)).unwrap();
        let units = Ps2Folder.extract_all(&t.encode()).unwrap();
        assert_eq!(units.len(), 1, "root files are not saves");
        let node = decode(&units[0], FALLBACK_TIME).unwrap();
        assert_eq!(node.created, T0);
        let NodeKind::Dir(children) = node.kind else { panic!() };
        let names: Vec<&[u8]> = children.iter().map(|c| c.name.as_slice()).collect();
        assert_eq!(names, [b"legacy".as_slice(), b"a:b", b"b"]);
        assert_eq!(children[0].modified, unix_to_ps2_time(1_000_000_000), "host time when not indexed");
        assert_eq!(children[1].modified, T1, "metadata times win");
        assert_eq!(children[2].created, unix_to_ps2_time(5));
    }

    #[test]
    fn unformatted_and_invalid_cards() {
        assert_eq!(Ps2Folder.probe(&new_card()), Confidence::Certain);
        assert_eq!(Ps2Folder.probe(&Tree::default().encode()), Confidence::Maybe);
        assert_eq!(Ps2Folder.probe(b"Sony PS2 Memory Card Format "), Confidence::No);
        assert!(Ps2Folder.validate(&Tree::default().encode()).is_err());
        // Unformatted: no saves even if folders exist, and deletes don't format it.
        let mut t = Tree::decode(&new_card()).unwrap();
        t.insert("BASLUS-20062GTA3".into(), 0, EntryKind::Dir).unwrap();
        let img = t.encode();
        assert!(Ps2Folder.extract_all(&img).unwrap().is_empty());
        assert_eq!(Ps2Folder.apply(&img, &[ApplyOp::Delete("X".into())]).unwrap(), img);
        // Formatting makes the existing folder visible.
        let r = canonical(&save(b"BASCUS-97199RAC", vec![file(b"data", &[3; 100], T1)], T1));
        let img = Ps2Folder.apply(&img, &[ApplyOp::Put(r)]).unwrap();
        assert_eq!(Ps2Folder.list(&img).unwrap().len(), 2);
    }

    #[test]
    fn refuses_what_pcsx2_would_drop() {
        let root_file = canonical(&file(b"ROOTFILE", b"x", T0));
        assert!(matches!(Ps2Folder.apply(&new_card(), &[ApplyOp::Put(root_file)]), Err(FormatError::Unsupported(_))));
        let reserved = canonical(&save(b"_pcsx2_x", vec![], T0));
        assert!(Ps2Folder.apply(&new_card(), &[ApplyOp::Put(reserved)]).is_err());
        let clash = canonical(&save(b"S", vec![file(b"a:b", b"1", T0), file(b"A_B", b"2", T0)], T0));
        assert!(Ps2Folder.apply(&new_card(), &[ApplyOp::Put(clash)]).is_err());
        // Card full: 8 MB of data does not fit in 7,999 clusters.
        let big = canonical(&save(b"BIG", vec![file(b"f", &vec![0; 8000 * 1024], T0)], T0));
        assert!(matches!(Ps2Folder.apply(&new_card(), &[ApplyOp::Put(big)]), Err(FormatError::NoSpace)));
        let fits = canonical(&save(b"BIG", vec![file(b"f", &vec![0; 7990 * 1024], T0)], T0));
        assert!(Ps2Folder.apply(&new_card(), &[ApplyOp::Put(fits)]).is_ok());
    }
}

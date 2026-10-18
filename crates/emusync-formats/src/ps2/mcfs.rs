//! The PS2 memory card file system (MCFS): parsing a card image into a tree of entries, and
//! rebuilding the allocatable area from such a tree (DESIGN §5.1).
//!
//! Layout: 512-byte pages, each followed by a 16-byte ECC spare area when the image has ECC
//! (8 MB cards are 8,650,752 bytes with ECC, 8,388,608 without); two pages per 1 KiB
//! cluster; a superblock in page 0; an indirect FAT pointing at FAT clusters; FAT entries for
//! the allocatable clusters; 512-byte directory entries, two per cluster.
//!
//! Rebuilding rewrites the whole allocatable area and the FAT. Everything else (superblock,
//! indirect FAT, reserved and backup blocks) is copied from the original image unchanged.
//! Entries are laid out contiguously in directory order, so a rebuilt card is never
//! fragmented and two cards holding the same entries in the same order are byte-identical.

use super::ecc;
use crate::{FormatError, Result};

pub const PAGE: usize = 512;
const SPARE: usize = 16;
pub const CLUSTER: usize = 1024;
const ENTRY: usize = 512;
const ENTRIES_PER_CLUSTER: u32 = (CLUSTER / ENTRY) as u32;
const FAT_PER_CLUSTER: u32 = (CLUSTER / 4) as u32;
/// Clusters in one erase block (16 pages).
const CLUSTERS_PER_BLOCK: u32 = 8;
/// Deepest directory nesting accepted (saves are one level deep in practice).
const MAX_DEPTH: usize = 8;

const MAGIC: &[u8; 28] = b"Sony PS2 Memory Card Format ";
const FAT_ALLOCATED: u32 = 0x8000_0000;
const FAT_CHAIN_END: u32 = 0xFFFF_FFFF;
const FAT_FREE: u32 = 0x7FFF_FFFF;
/// First-cluster value of an empty file.
const NO_CLUSTER: u32 = 0xFFFF_FFFF;

pub const MODE_READ: u16 = 0x0001;
pub const MODE_WRITE: u16 = 0x0002;
pub const MODE_EXECUTE: u16 = 0x0004;
pub const MODE_FILE: u16 = 0x0010;
pub const MODE_DIR: u16 = 0x0020;
pub const MODE_0400: u16 = 0x0400;
pub const MODE_HIDDEN: u16 = 0x2000;
pub const MODE_EXISTS: u16 = 0x8000;
/// Mode of a directory created by the PS2 browser or mymc.
pub const MODE_DIR_DEFAULT: u16 = MODE_EXISTS | MODE_0400 | MODE_DIR | MODE_READ | MODE_WRITE | MODE_EXECUTE;
/// Mode of a file created by a typical game (`0x8497`).
pub const MODE_FILE_DEFAULT: u16 = MODE_EXISTS | MODE_0400 | 0x0080 | MODE_FILE | MODE_READ | MODE_WRITE | MODE_EXECUTE;

/// Card sizes (clusters per card) that PCSX2 and real hardware use: 8, 16, 32 and 64 MB.
pub const CARD_CLUSTERS: [u32; 4] = [8192, 16384, 32768, 65536];

/// Whether `len` is the size of a standard card image, with or without ECC.
pub fn is_card_size(len: usize) -> bool {
    CARD_CLUSTERS.iter().any(|&c| {
        let pages = c as usize * 2;
        len == pages * (PAGE + SPARE) || len == pages * PAGE
    })
}

/// A file or directory on the card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// Raw name bytes (at most 32, no NUL or `/`).
    pub name: Vec<u8>,
    pub mode: u16,
    pub attr: u32,
    /// `sceMcStDateTime` as stored: reserved, sec, min, hour, day, month, year (LE u16).
    pub created: [u8; 8],
    pub modified: [u8; 8],
    pub kind: NodeKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeKind {
    File(Vec<u8>),
    /// Children in on-card order.
    Dir(Vec<Node>),
}

/// A parsed card: the geometry needed to rebuild it, and its root entries in on-card order.
#[derive(Debug, Clone)]
pub struct Card {
    ecc: bool,
    alloc_offset: u32,
    alloc_end: u32,
    root_cluster: u32,
    /// Absolute cluster numbers of the FAT clusters, in order.
    fat_clusters: Vec<u32>,
    has_bad_blocks: bool,
    root_dot: [u8; ENTRY],
    root_dotdot: [u8; ENTRY],
    pub entries: Vec<Node>,
}

fn invalid(msg: impl Into<String>) -> FormatError {
    FormatError::Invalid(msg.into())
}

fn u16_at(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn div_ceil(a: u64, b: u64) -> u64 {
    a.div_ceil(b)
}

/// Name bytes of a directory entry: up to the first NUL in the 32-byte field.
fn entry_name(e: &[u8]) -> &[u8] {
    let field = &e[0x40..0x60];
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    &field[..end]
}

/// Checks a name for use on the card.
pub fn check_name(name: &[u8]) -> Result<()> {
    if name.is_empty() || name.len() > 32 || name == b"." || name == b".." || name.iter().any(|&b| b == 0 || b == b'/')
    {
        return Err(invalid(format!("invalid entry name {:?}", String::from_utf8_lossy(name))));
    }
    Ok(())
}

struct Reader<'a> {
    img: &'a [u8],
    raw_page: usize,
    alloc_offset: u32,
    alloc_end: u32,
    fat: Vec<u32>,
    used: Vec<bool>,
}

/// Data bytes of one cluster (both pages, without spare areas).
fn read_cluster(img: &[u8], raw_page: usize, abs: u32) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(CLUSTER);
    for p in 0..2 {
        let off = (abs as usize * 2 + p) * raw_page;
        let page = img.get(off..off + PAGE).ok_or_else(|| invalid("cluster outside the image"))?;
        out.extend_from_slice(page);
    }
    Ok(out)
}

impl Reader<'_> {
    fn cluster(&self, abs: u32) -> Result<Vec<u8>> {
        read_cluster(self.img, self.raw_page, abs)
    }

    /// Follows a FAT chain from `first` (relative to the allocatable area), marking clusters
    /// as used. Loops, cross-links and chains through free clusters are corruption.
    fn chain(&mut self, first: u32) -> Result<Vec<u32>> {
        let mut out = Vec::new();
        let mut c = first;
        loop {
            if c >= self.alloc_end {
                return Err(invalid(format!("cluster {c} is outside the allocatable area")));
            }
            if self.used[c as usize] {
                return Err(invalid(format!("cluster {c} is used twice (looping or cross-linked chain)")));
            }
            self.used[c as usize] = true;
            out.push(c);
            let e = self.fat[c as usize];
            if e & FAT_ALLOCATED == 0 {
                return Err(invalid(format!("cluster {c} is in a chain but marked free")));
            }
            let next = e & !FAT_ALLOCATED;
            if next == FAT_FREE {
                return Ok(out);
            }
            c = next;
        }
    }

    fn data_cluster(&self, rel: u32) -> Result<Vec<u8>> {
        self.cluster(self.alloc_offset + rel)
    }

    /// Reads `count` directory entries from the chain starting at `first`.
    fn dir_entries(&mut self, first: u32, count: u32) -> Result<Vec<[u8; ENTRY]>> {
        if count < 2 {
            return Err(invalid("directory without . and .. entries"));
        }
        let chain = self.chain(first)?;
        if (chain.len() as u64) < div_ceil(count as u64, ENTRIES_PER_CLUSTER as u64) {
            return Err(invalid(format!("directory claims {count} entries but its chain is too short")));
        }
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count {
            let data = self.data_cluster(chain[(i / ENTRIES_PER_CLUSTER) as usize])?;
            let off = (i % ENTRIES_PER_CLUSTER) as usize * ENTRY;
            out.push(data[off..off + ENTRY].try_into().unwrap());
        }
        Ok(out)
    }

    /// Reads the children of a directory (skipping `.`, `..` and deleted entries).
    fn children(&mut self, raw: &[[u8; ENTRY]], depth: usize) -> Result<Vec<Node>> {
        if depth > MAX_DEPTH {
            return Err(invalid("directories nested too deeply"));
        }
        let mut out: Vec<Node> = Vec::new();
        for e in &raw[2..] {
            let mode = u16_at(e, 0);
            if mode & MODE_EXISTS == 0 {
                continue;
            }
            let name = entry_name(e).to_vec();
            check_name(&name)?;
            if out.iter().any(|n| n.name == name) {
                return Err(invalid(format!("duplicate entry {:?}", String::from_utf8_lossy(&name))));
            }
            let length = u32_at(e, 0x04);
            let cluster = u32_at(e, 0x10);
            let kind = if mode & MODE_DIR != 0 {
                let entries = self.dir_entries(cluster, length)?;
                NodeKind::Dir(self.children(&entries, depth + 1)?)
            } else if mode & MODE_FILE != 0 {
                let mut data = Vec::new();
                if length > 0 {
                    let chain = self.chain(cluster)?;
                    if (chain.len() as u64) < div_ceil(length as u64, CLUSTER as u64) {
                        return Err(invalid(format!(
                            "file {:?} is {length} bytes but its chain is too short",
                            String::from_utf8_lossy(&name)
                        )));
                    }
                    data.reserve_exact(length as usize);
                    for c in chain {
                        if data.len() >= length as usize {
                            break;
                        }
                        let d = self.data_cluster(c)?;
                        let take = (length as usize - data.len()).min(CLUSTER);
                        data.extend_from_slice(&d[..take]);
                    }
                }
                NodeKind::File(data)
            } else {
                return Err(invalid(format!(
                    "entry {:?} is neither a file nor a directory (mode {mode:#06x})",
                    String::from_utf8_lossy(&name)
                )));
            };
            out.push(Node {
                name,
                mode,
                attr: u32_at(e, 0x20),
                created: e[0x08..0x10].try_into().unwrap(),
                modified: e[0x18..0x20].try_into().unwrap(),
                kind,
            });
        }
        Ok(out)
    }
}

impl Card {
    /// Parses and fully validates a card image. ECC is not checked on read: pages that were
    /// never written legitimately have none, and emulators don't check it either. It is
    /// recomputed for every page a rebuild writes.
    pub fn parse(img: &[u8]) -> Result<Card> {
        if img.len() < PAGE || &img[..28] != MAGIC {
            return Err(invalid("not a formatted PS2 memory card (no superblock)"));
        }
        if !img[28..40].starts_with(b"1.") {
            return Err(invalid("unsupported card format version"));
        }
        let (page_len, pages_per_cluster) = (u16_at(img, 0x28), u16_at(img, 0x2A));
        if page_len as usize != PAGE || pages_per_cluster != 2 {
            return Err(invalid(format!(
                "unsupported geometry: {page_len}-byte pages, {pages_per_cluster} per cluster"
            )));
        }
        let clusters_per_card = u32_at(img, 0x30);
        if clusters_per_card == 0 || clusters_per_card > 1 << 20 {
            return Err(invalid(format!("implausible card size of {clusters_per_card} clusters")));
        }
        let pages = clusters_per_card as usize * 2;
        let ecc = if img.len() == pages * (PAGE + SPARE) {
            true
        } else if img.len() == pages * PAGE {
            false
        } else {
            return Err(invalid(format!(
                "image is {} bytes, but its superblock describes a card of {} clusters",
                img.len(),
                clusters_per_card
            )));
        };
        let alloc_offset = u32_at(img, 0x34);
        let alloc_end = u32_at(img, 0x38);
        let root_cluster = u32_at(img, 0x3C);
        if alloc_end == 0 || alloc_offset as u64 + alloc_end as u64 > clusters_per_card as u64 {
            return Err(invalid("allocatable area does not fit on the card"));
        }
        if root_cluster >= alloc_end {
            return Err(invalid("root directory is outside the allocatable area"));
        }
        let ifc_list: Vec<u32> = (0..32).map(|i| u32_at(img, 0x50 + i * 4)).collect();
        // Block 0 holds the superblock, so 0 can't be a bad block: treat it as an empty slot.
        let has_bad_blocks = (0..32).map(|i| u32_at(img, 0xD0 + i * 4)).any(|b| b != 0xFFFF_FFFF && b != 0);

        let raw_page = if ecc { PAGE + SPARE } else { PAGE };
        let mut reader =
            Reader { img, raw_page, alloc_offset, alloc_end, fat: Vec::new(), used: vec![false; alloc_end as usize] };
        let n_fat = div_ceil(alloc_end as u64, FAT_PER_CLUSTER as u64) as u32;
        let n_ifc = div_ceil(n_fat as u64, FAT_PER_CLUSTER as u64) as usize;
        if n_ifc > ifc_list.len() {
            return Err(invalid("FAT too large for the indirect FAT list"));
        }
        let mut fat_clusters = Vec::with_capacity(n_fat as usize);
        for (k, &ifc) in ifc_list.iter().take(n_ifc).enumerate() {
            if ifc >= clusters_per_card {
                return Err(invalid("indirect FAT cluster is outside the card"));
            }
            let data = reader.cluster(ifc)?;
            let here = (n_fat as usize - k * FAT_PER_CLUSTER as usize).min(FAT_PER_CLUSTER as usize);
            for j in 0..here {
                let fc = u32_at(&data, j * 4);
                if fc >= clusters_per_card || (fc >= alloc_offset && fc < alloc_offset + alloc_end) {
                    return Err(invalid("FAT cluster is outside the reserved area"));
                }
                fat_clusters.push(fc);
            }
        }
        let mut fat = Vec::with_capacity(alloc_end as usize);
        for &fc in &fat_clusters {
            let data = reader.cluster(fc)?;
            for j in 0..FAT_PER_CLUSTER as usize {
                if fat.len() == alloc_end as usize {
                    break;
                }
                fat.push(u32_at(&data, j * 4));
            }
        }
        reader.fat = fat;

        // The root's "." entry holds the root's own entry count.
        let first = reader.data_cluster(root_cluster)?;
        let root_dot: [u8; ENTRY] = first[..ENTRY].try_into().unwrap();
        let root_mode = u16_at(&root_dot, 0);
        if root_mode & (MODE_EXISTS | MODE_DIR) != MODE_EXISTS | MODE_DIR {
            return Err(invalid("root directory entry is missing"));
        }
        let root_len = u32_at(&root_dot, 0x04);
        if root_len > alloc_end.saturating_mul(ENTRIES_PER_CLUSTER) {
            return Err(invalid("root directory claims more entries than the card can hold"));
        }
        let raw = reader.dir_entries(root_cluster, root_len)?;
        let root_dotdot = raw[1];
        let entries = reader.children(&raw, 0)?;
        Ok(Card {
            ecc,
            alloc_offset,
            alloc_end,
            root_cluster,
            fat_clusters,
            has_bad_blocks,
            root_dot,
            root_dotdot,
            entries,
        })
    }

    /// Modification time of the root directory (its `.` entry).
    pub fn root_time(&self) -> [u8; 8] {
        self.root_dot[0x18..0x20].try_into().unwrap()
    }

    /// Clusters available for entries (root directory included).
    pub fn capacity_clusters(&self) -> u32 {
        self.alloc_end
    }

    /// Builds a new image from `original` (the image this card was parsed from) holding
    /// `entries` at the root, in order.
    pub fn build(&self, original: &[u8], entries: &[Node]) -> Result<Vec<u8>> {
        if self.has_bad_blocks {
            return Err(FormatError::Unsupported("rewriting cards with bad blocks".into()));
        }
        if self.root_cluster != 0 {
            return Err(FormatError::Unsupported("root directory not at the first allocatable cluster".into()));
        }
        let mut w = Writer {
            out: original.to_vec(),
            raw_page: if self.ecc { PAGE + SPARE } else { PAGE },
            ecc: self.ecc,
            alloc_offset: self.alloc_offset,
            alloc_end: self.alloc_end,
            next: 0,
            fat: vec![FAT_FREE; self.alloc_end as usize],
        };
        for rel in 0..self.alloc_end {
            w.erase_cluster(self.alloc_offset + rel);
        }
        // Root directory first, at cluster 0.
        let root_slots = entries.len() as u64 + 2;
        let root_first = w.alloc(div_ceil(root_slots, ENTRIES_PER_CLUSTER as u64))?;
        let mut dot = self.root_dot;
        dot[0x04..0x08].copy_from_slice(&(root_slots as u32).to_le_bytes());
        let mut slots = vec![dot, self.root_dotdot];
        for (i, node) in entries.iter().enumerate() {
            slots.push(w.place(node, root_first, i as u32 + 2, 1)?);
        }
        w.write_dir(root_first, &slots)?;

        // FAT: entries past the allocatable area keep their original values.
        for (k, &fc) in self.fat_clusters.iter().enumerate() {
            let mut data = read_cluster(original, w.raw_page, fc)?;
            for j in 0..FAT_PER_CLUSTER as usize {
                let idx = k * FAT_PER_CLUSTER as usize + j;
                if idx < w.fat.len() {
                    data[j * 4..j * 4 + 4].copy_from_slice(&w.fat[idx].to_le_bytes());
                }
            }
            w.write_cluster(fc, &data);
        }
        Ok(w.out)
    }
}

struct Writer {
    out: Vec<u8>,
    raw_page: usize,
    ecc: bool,
    alloc_offset: u32,
    alloc_end: u32,
    next: u32,
    fat: Vec<u32>,
}

/// Encodes one 512-byte directory entry.
#[allow(clippy::too_many_arguments)]
fn dirent(
    mode: u16,
    length: u32,
    created: &[u8; 8],
    cluster: u32,
    dir_entry: u32,
    modified: &[u8; 8],
    attr: u32,
    name: &[u8],
) -> [u8; ENTRY] {
    let mut e = [0u8; ENTRY];
    e[0x00..0x02].copy_from_slice(&mode.to_le_bytes());
    e[0x04..0x08].copy_from_slice(&length.to_le_bytes());
    e[0x08..0x10].copy_from_slice(created);
    e[0x10..0x14].copy_from_slice(&cluster.to_le_bytes());
    e[0x14..0x18].copy_from_slice(&dir_entry.to_le_bytes());
    e[0x18..0x20].copy_from_slice(modified);
    e[0x20..0x24].copy_from_slice(&attr.to_le_bytes());
    e[0x40..0x40 + name.len()].copy_from_slice(name);
    e
}

impl Writer {
    /// Allocates `n` contiguous clusters as one chain and returns the first (relative).
    fn alloc(&mut self, n: u64) -> Result<u32> {
        if n == 0 {
            return Ok(NO_CLUSTER);
        }
        if self.next as u64 + n > self.alloc_end as u64 {
            return Err(FormatError::NoSpace);
        }
        let first = self.next;
        let last = first + n as u32 - 1;
        for c in first..last {
            self.fat[c as usize] = FAT_ALLOCATED | (c + 1);
        }
        self.fat[last as usize] = FAT_CHAIN_END;
        self.next = last + 1;
        Ok(first)
    }

    fn erase_cluster(&mut self, abs: u32) {
        let off = abs as usize * 2 * self.raw_page;
        self.out[off..off + 2 * self.raw_page].fill(0xFF);
    }

    fn write_cluster(&mut self, abs: u32, data: &[u8]) {
        debug_assert_eq!(data.len(), CLUSTER);
        for p in 0..2 {
            let off = (abs as usize * 2 + p) * self.raw_page;
            let page = &data[p * PAGE..(p + 1) * PAGE];
            self.out[off..off + PAGE].copy_from_slice(page);
            if self.ecc {
                self.out[off + PAGE..off + PAGE + SPARE].copy_from_slice(&ecc::page_spare(page));
            }
        }
    }

    /// Writes data into the chain starting at `first` (contiguous, from [`Self::alloc`]).
    fn write_chain(&mut self, first: u32, data: &[u8]) {
        for (i, piece) in data.chunks(CLUSTER).enumerate() {
            let mut buf = [0u8; CLUSTER];
            buf[..piece.len()].copy_from_slice(piece);
            self.write_cluster(self.alloc_offset + first + i as u32, &buf);
        }
    }

    fn write_dir(&mut self, first: u32, slots: &[[u8; ENTRY]]) -> Result<()> {
        let bytes: Vec<u8> = slots.iter().flatten().copied().collect();
        self.write_chain(first, &bytes);
        Ok(())
    }

    /// Lays out `node` (and its children), returning its entry for the parent directory.
    /// `parent_first`/`slot` locate that entry, which a directory's `.` entry points back to.
    fn place(&mut self, node: &Node, parent_first: u32, slot: u32, depth: usize) -> Result<[u8; ENTRY]> {
        check_name(&node.name)?;
        if depth > MAX_DEPTH {
            return Err(invalid("directories nested too deeply"));
        }
        let mode = node.mode | MODE_EXISTS;
        match &node.kind {
            NodeKind::File(data) => {
                let len = u32::try_from(data.len()).map_err(|_| FormatError::NoSpace)?;
                let first = self.alloc(div_ceil(data.len() as u64, CLUSTER as u64))?;
                self.write_chain(first, data);
                Ok(dirent(mode, len, &node.created, first, 0, &node.modified, node.attr, &node.name))
            }
            NodeKind::Dir(children) => {
                let count = children.len() as u64 + 2;
                let first = self.alloc(div_ceil(count, ENTRIES_PER_CLUSTER as u64))?;
                let mut slots = vec![
                    dirent(mode, 0, &node.created, parent_first, slot, &node.modified, 0, b"."),
                    dirent(mode, 0, &node.created, 0, 0, &node.modified, 0, b".."),
                ];
                for (i, child) in children.iter().enumerate() {
                    slots.push(self.place(child, first, i as u32 + 2, depth + 1)?);
                }
                self.write_dir(first, &slots)?;
                Ok(dirent(mode, count as u32, &node.created, first, 0, &node.modified, node.attr, &node.name))
            }
        }
    }
}

/// A freshly formatted, empty card with the standard layout: the geometry, superblock values
/// and root directory that PCSX2's folder memory cards and mymc produce. Used to format a
/// card file that was never formatted (all `0xFF`), and by tests.
pub fn blank_card(clusters_per_card: u32, ecc: bool, time: [u8; 8]) -> Vec<u8> {
    let raw_page = if ecc { PAGE + SPARE } else { PAGE };
    let pages = clusters_per_card as usize * 2;
    let mut w = Writer {
        out: vec![0xFF; pages * raw_page],
        raw_page,
        ecc,
        alloc_offset: 0,
        alloc_end: 0,
        next: 0,
        fat: Vec::new(),
    };
    // Erase block 0 holds the superblock; the indirect FAT follows, then the FAT. The last two
    // erase blocks are the backup blocks.
    let ifc = CLUSTERS_PER_BLOCK;
    let usable = clusters_per_card - 2 * CLUSTERS_PER_BLOCK;
    let n_fat = (usable - ifc - 1).div_ceil(FAT_PER_CLUSTER);
    let alloc_offset = ifc + 1 + n_fat;
    let alloc_end = usable - alloc_offset;
    let blocks = clusters_per_card / CLUSTERS_PER_BLOCK;

    let mut sb = [0xFFu8; PAGE];
    sb[..28].copy_from_slice(MAGIC);
    sb[28..40].copy_from_slice(b"1.2.0.0\0\0\0\0\0");
    sb[0x28..0x2A].copy_from_slice(&(PAGE as u16).to_le_bytes());
    sb[0x2A..0x2C].copy_from_slice(&2u16.to_le_bytes());
    sb[0x2C..0x2E].copy_from_slice(&16u16.to_le_bytes());
    sb[0x2E..0x30].copy_from_slice(&0xFF00u16.to_le_bytes());
    for (off, v) in [
        (0x30, clusters_per_card),
        (0x34, alloc_offset),
        (0x38, alloc_end),
        (0x3C, 0),
        (0x40, blocks - 1),
        (0x44, blocks - 2),
    ] {
        sb[off..off + 4].copy_from_slice(&u32::to_le_bytes(v));
    }
    sb[0x48..0x50].fill(0);
    for i in 0..32 {
        let ifc_entry = if i == 0 { ifc } else { 0 };
        sb[0x50 + i * 4..0x54 + i * 4].copy_from_slice(&ifc_entry.to_le_bytes());
        sb[0xD0 + i * 4..0xD4 + i * 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    }
    sb[0x150] = 2; // card type: PS2
    sb[0x151] = 0x2B; // flags, as PCSX2's folder memory cards write them (ECC in use)
    sb[0x152..0x154].fill(0);
    w.out[..PAGE].copy_from_slice(&sb);
    if ecc {
        w.out[PAGE..PAGE + SPARE].copy_from_slice(&ecc::page_spare(&sb));
    }

    let mut ifc_data = [0u8; CLUSTER];
    for k in 0..n_fat {
        ifc_data[k as usize * 4..k as usize * 4 + 4].copy_from_slice(&(ifc + 1 + k).to_le_bytes());
    }
    w.write_cluster(ifc, &ifc_data);
    for k in 0..n_fat {
        let mut data = [0u8; CLUSTER];
        for j in 0..FAT_PER_CLUSTER {
            let idx = k * FAT_PER_CLUSTER + j;
            let v = if idx == 0 {
                FAT_CHAIN_END // root directory
            } else if idx < alloc_end {
                FAT_FREE
            } else {
                FAT_CHAIN_END
            };
            data[j as usize * 4..j as usize * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        w.write_cluster(ifc + 1 + k, &data);
    }
    let dot = dirent(MODE_DIR_DEFAULT, 2, &time, 0, 0, &time, 0, b".");
    let dotdot = dirent((MODE_DIR_DEFAULT & !MODE_READ) | MODE_HIDDEN, 0, &time, 0, 0, &time, 0, b"..");
    let mut root = [0u8; CLUSTER];
    root[..ENTRY].copy_from_slice(&dot);
    root[ENTRY..].copy_from_slice(&dotdot);
    w.write_cluster(alloc_offset, &root);
    w.out
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: [u8; 8] = [0, 30, 15, 12, 18, 10, 0xEA, 0x07]; // 2026-10-18 12:15:30

    fn file(name: &str, data: &[u8]) -> Node {
        Node {
            name: name.as_bytes().to_vec(),
            mode: MODE_FILE_DEFAULT,
            attr: 0,
            created: T,
            modified: T,
            kind: NodeKind::File(data.to_vec()),
        }
    }

    fn dir(name: &str, children: Vec<Node>) -> Node {
        Node {
            name: name.as_bytes().to_vec(),
            mode: MODE_DIR_DEFAULT,
            attr: 0,
            created: T,
            modified: T,
            kind: NodeKind::Dir(children),
        }
    }

    #[test]
    fn blank_card_geometry() {
        let img = blank_card(8192, true, T);
        assert_eq!(img.len(), 8_650_752);
        assert!(is_card_size(img.len()));
        let card = Card::parse(&img).unwrap();
        assert_eq!((card.alloc_offset, card.alloc_end), (41, 8135), "matches the standard 8 MB layout");
        assert!(card.entries.is_empty());
        let no_ecc = blank_card(8192, false, T);
        assert_eq!(no_ecc.len(), 8_388_608);
        assert!(Card::parse(&no_ecc).unwrap().entries.is_empty());
        let big = blank_card(65536, true, T);
        assert!(Card::parse(&big).unwrap().entries.is_empty());
    }

    #[test]
    fn rebuild_round_trip() {
        let img = blank_card(8192, true, T);
        let card = Card::parse(&img).unwrap();
        let big: Vec<u8> = (0..5000u32).map(|i| (i * 7) as u8).collect();
        let entries = vec![
            dir("BASLUS-20062GTA3", vec![file("icon.sys", &[1; 964]), file("GTA3SAVE", &big), file("empty", &[])]),
            dir("BESLES-50330", vec![dir("sub", vec![file("x", b"nested")])]),
            dir("BASCUS-97199RAC", vec![]),
        ];
        let out = card.build(&img, &entries).unwrap();
        assert_eq!(out.len(), img.len());
        let back = Card::parse(&out).unwrap();
        assert_eq!(back.entries, entries);
        // Same entries in the same order give the same bytes.
        assert_eq!(back.build(&out, &entries).unwrap(), out);
        // Removing everything returns to the blank card's allocatable area.
        let empty = back.build(&out, &[]).unwrap();
        assert!(Card::parse(&empty).unwrap().entries.is_empty());
    }

    #[test]
    fn written_pages_have_valid_ecc() {
        let img = blank_card(8192, true, T);
        let card = Card::parse(&img).unwrap();
        let out = card.build(&img, &[dir("SAVE", vec![file("f", &[0x5A; 3000])])]).unwrap();
        for page in 0..(card.alloc_offset as usize + 4) * 2 {
            let off = page * (PAGE + SPARE);
            let data = &out[off..off + PAGE];
            if data.iter().all(|&b| b == 0xFF) {
                continue; // erased page: no ECC
            }
            assert_eq!(&out[off + PAGE..off + PAGE + SPARE], &ecc::page_spare(data), "page {page}");
        }
    }

    #[test]
    fn full_card() {
        let img = blank_card(8192, false, T);
        let card = Card::parse(&img).unwrap();
        let cap = card.capacity_clusters() as usize;
        // The root (3 entries) and the save directory (3 entries) take two clusters each.
        let fits = vec![dir("SAVE", vec![file("big", &vec![1; (cap - 4) * CLUSTER])])];
        assert!(card.build(&img, &fits).is_ok());
        let too_big = vec![dir("SAVE", vec![file("big", &vec![1; (cap - 4) * CLUSTER + 1])])];
        assert!(matches!(card.build(&img, &too_big), Err(FormatError::NoSpace)));
    }

    #[test]
    fn rejects_damage() {
        let img = blank_card(8192, false, T);
        let card = Card::parse(&img).unwrap();
        let good = card.build(&img, &[dir("SAVE", vec![file("f", &[7; 2500])])]).unwrap();
        assert!(Card::parse(&good).is_ok());
        let fat0 = (card.fat_clusters[0] as usize) * CLUSTER;
        let entry = |i: usize| fat0 + i * 4;

        // A chain that loops back on itself.
        let mut looped = good.clone();
        let last = (0..8).rev().find(|&i| u32_at(&good, entry(i)) == FAT_CHAIN_END).unwrap();
        looped[entry(last)..entry(last) + 4].copy_from_slice(&(FAT_ALLOCATED | 2).to_le_bytes());
        assert!(matches!(Card::parse(&looped), Err(FormatError::Invalid(_))));

        // A chain that runs into a free cluster.
        let mut freed = good.clone();
        freed[entry(last)..entry(last) + 4].copy_from_slice(&FAT_FREE.to_le_bytes());
        assert!(matches!(Card::parse(&freed), Err(FormatError::Invalid(_))));

        // Wrong size, no superblock.
        assert!(Card::parse(&good[..good.len() - 1]).is_err());
        assert!(Card::parse(&vec![0xFF; good.len()]).is_err());
    }
}

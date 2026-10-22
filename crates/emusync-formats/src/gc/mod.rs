//! GameCube memory cards (DESIGN §5.3, §25): one unit per save file.
//!
//! - [`card`] (`gc.card`): raw card images, Dolphin's `MemoryCardA.USA.raw` and real-card dumps.
//! - [`gci`] (`gc.gci`): Dolphin's GCI folders, one `.gci` file per save.
//!
//! Both hold the same saves: a 64-byte directory entry (DEntry) plus whole 8 KiB blocks of data.
//! A save is identified on a card by its game code, maker code and file name up to the first
//! NUL, which is what Dolphin (`HasSameIdentity`) and games go by. Its unit name is those three,
//! decoded as Latin-1 (lossless): `GM8E01-MetroidPrime A`.
//!
//! The canonical form (DESIGN §2.5) is the magic `ES-GCU01`, the directory entry with the fields
//! that describe placement or bookkeeping rather than content set to fixed values (first block,
//! modification time, copy counter, the two unused fields), and the data. The modification time
//! and copy counter travel in the `gc.dentry` meta field instead, so a save rewritten with the
//! same bytes, or the same save on two cards, hashes the same.
//!
//! Three saves carry the card's serial number (and a checksum over it): Phantasy Star Online's
//! `PSO_SYSTEM` and `PSO3_SYSTEM`, and F-Zero GX's `f_zero.dat`. Dolphin rewrites those fields for
//! the destination card when it imports one (`PSO_MakeSaveGameValid`, `FZEROGX_MakeSaveGameValid`),
//! and in a GCI folder when it loads one. The canonical form has them zeroed, so the copies on two
//! cards hash the same, and writing into a raw card fills them in for that card the way Dolphin
//! does; a GCI folder gets them zeroed and Dolphin fills them in when it loads the folder.

pub mod card;
pub mod gci;

use crate::{Canonical, FormatError, Result};
use std::collections::BTreeMap;

/// Size of a card block, and of each block of a save's data.
pub const BLOCK: usize = 0x2000;
/// Size of a directory entry (and of a GCI file's header).
pub const DENTRY: usize = 0x40;
/// Meta key with the modification time and copy counter: `"<mtime hex8> <copy hex2>"`.
pub const META: &str = "gc.dentry";
const CANONICAL_MAGIC: &[u8; 8] = b"ES-GCU01";

// Directory entry fields (Dolphin's `DEntry`).
const GAMECODE: std::ops::Range<usize> = 0x00..0x04;
const MAKERCODE: std::ops::Range<usize> = 0x04..0x06;
const UNUSED_1: usize = 0x06;
const FILENAME: std::ops::Range<usize> = 0x08..0x28;
const MOD_TIME: std::ops::Range<usize> = 0x28..0x2C;
const COPY_COUNTER: usize = 0x35;
const FIRST_BLOCK: std::ops::Range<usize> = 0x36..0x38;
const BLOCK_COUNT: std::ops::Range<usize> = 0x38..0x3A;
const UNUSED_2: std::ops::Range<usize> = 0x3A..0x3C;

/// Seconds from the Unix epoch to the GameCube's (2000-01-01 00:00:00 UTC).
pub const GC_EPOCH_UNIX: i64 = 946_684_800;

fn invalid(msg: impl Into<String>) -> FormatError {
    FormatError::Invalid(msg.into())
}

pub(crate) fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

pub(crate) fn set_be16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_be_bytes());
}

/// A save: its directory entry and its data (whole blocks).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Save {
    pub dentry: [u8; DENTRY],
    pub data: Vec<u8>,
}

/// Whether a directory entry slot is free (its game code is all `0xFF`).
pub fn is_free(dentry: &[u8]) -> bool {
    dentry[GAMECODE] == [0xFF; 4]
}

fn latin1(b: &[u8]) -> String {
    b.iter().map(|&c| c as char).collect()
}

fn c_str(b: &[u8]) -> &[u8] {
    b.iter().position(|&c| c == 0).map_or(b, |n| &b[..n])
}

impl Save {
    pub fn block_count(&self) -> u16 {
        be16(&self.dentry, BLOCK_COUNT.start)
    }

    /// Game code, maker code and file name up to its first NUL: how a card tells saves apart.
    pub fn identity(&self) -> (&[u8], &[u8], &[u8]) {
        (&self.dentry[GAMECODE], &self.dentry[MAKERCODE], c_str(&self.dentry[FILENAME]))
    }

    /// `GM8E01-MetroidPrime A`.
    pub fn unit_name(&self) -> String {
        let (game, maker, file) = self.identity();
        format!("{}{}-{}", latin1(game), latin1(maker), latin1(file))
    }

    pub fn mod_time(&self) -> u32 {
        u32::from_be_bytes(self.dentry[MOD_TIME].try_into().unwrap())
    }

    pub fn copy_counter(&self) -> u8 {
        self.dentry[COPY_COUNTER]
    }

    /// Checks the entry describes this data: in use, at least one block, a matching block count.
    pub fn check(&self) -> Result<()> {
        if is_free(&self.dentry) {
            return Err(invalid("save with an empty directory entry"));
        }
        let blocks = self.block_count() as usize;
        if blocks == 0 || self.data.len() != blocks * BLOCK {
            return Err(invalid(format!(
                "save {:?} claims {blocks} block(s) but has {} bytes of data",
                self.unit_name(),
                self.data.len()
            )));
        }
        Ok(())
    }

    /// The saves that hold the card's serial number (see the module docs).
    fn protection(&self) -> Option<Protection> {
        match c_str(&self.dentry[FILENAME]) {
            b"PSO_SYSTEM" if self.data.len() >= 2 * BLOCK => Some(Protection::Pso { extra: 0 }),
            b"PSO3_SYSTEM" if self.data.len() >= 2 * BLOCK => Some(Protection::Pso { extra: 0x10 }),
            b"f_zero.dat" if self.data.len() == 4 * BLOCK => Some(Protection::FZero),
            _ => None,
        }
    }

    /// Zeroes the card serial and its checksum in a copy-protected save (canonical form).
    fn clear_serial(&mut self) {
        let protection = self.protection();
        let d = &mut self.data;
        match protection {
            Some(Protection::Pso { .. }) => {
                d[BLOCK + 0x48..BLOCK + 0x4C].fill(0);
                d[BLOCK + 0x158..BLOCK + 0x160].fill(0);
            }
            Some(Protection::FZero) => {
                d[0..2].fill(0);
                for at in [BLOCK + 0x60, BLOCK + 0x66, BLOCK + 0x200, 3 * BLOCK + 0x1580] {
                    d[at..at + 2].fill(0);
                }
            }
            None => {}
        }
    }

    /// Writes the destination card's serial (from its header's first 32 bytes) and the checksum
    /// into a copy-protected save, exactly as Dolphin does on import.
    pub fn set_serial(&mut self, header: &[u8]) {
        let Some(kind) = self.protection() else { return };
        let mut s1 = [0u8; 4];
        let mut s2 = [0u8; 4];
        for i in (0..32).step_by(8) {
            for k in 0..4 {
                s1[k] ^= header[i + k];
                s2[k] ^= header[i + 4 + k];
            }
        }
        let d = &mut self.data;
        match kind {
            Protection::Pso { extra } => {
                d[BLOCK + 0x158..BLOCK + 0x15C].copy_from_slice(&s1);
                d[BLOCK + 0x15C..BLOCK + 0x160].copy_from_slice(&s2);
                let mut crc: u32 = 0xDEBB_20E3;
                for &b in &d[BLOCK + 0x4C..BLOCK + 0x164 + extra] {
                    crc = (crc >> 8) ^ crc32_table()[((crc ^ b as u32) & 0xFF) as usize];
                }
                d[BLOCK + 0x48..BLOCK + 0x4C].copy_from_slice(&(crc ^ 0xFFFF_FFFF).to_be_bytes());
            }
            Protection::FZero => {
                d[BLOCK + 0x66..BLOCK + 0x68].copy_from_slice(&s1[0..2]);
                d[3 * BLOCK + 0x1580..3 * BLOCK + 0x1582].copy_from_slice(&s2[0..2]);
                d[BLOCK + 0x60..BLOCK + 0x62].copy_from_slice(&s1[2..4]);
                d[BLOCK + 0x200..BLOCK + 0x202].copy_from_slice(&s2[2..4]);
                let mut sum: u16 = 0xFFFF;
                for &b in &d[2..0x8000] {
                    sum ^= b as u16;
                    for _ in 0..8 {
                        sum = if sum & 1 != 0 { (sum >> 1) ^ 0x8408 } else { sum >> 1 };
                    }
                }
                d[0..2].copy_from_slice(&(!sum).to_be_bytes());
            }
        }
    }

    /// The entry as written to a card or a GCI file: the unused fields as the BIOS leaves them.
    fn with_placement(&self, first_block: u16) -> [u8; DENTRY] {
        let mut e = self.dentry;
        e[UNUSED_1] = 0xFF;
        e[UNUSED_2].fill(0xFF);
        set_be16(&mut e, FIRST_BLOCK.start, first_block);
        e
    }
}

#[derive(Clone, Copy)]
enum Protection {
    Pso { extra: usize },
    FZero,
}

fn crc32_table() -> &'static [u32; 256] {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, slot) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 };
            }
            *slot = c;
        }
        t
    })
}

/// Title keys of a save, from its unit name: `GM8E01-…` → `gc:GM8E01` (game code + maker code).
pub fn titles_of(name: &str) -> Vec<String> {
    let b = name.as_bytes();
    if b.len() >= 7 && b[6] == b'-' && b[..6].iter().all(u8::is_ascii_alphanumeric) {
        vec![format!("gc:{}", &name[..6])]
    } else {
        vec![]
    }
}

/// The canonical unit of a save.
pub fn canonical(save: &Save) -> Canonical {
    let mut normal = save.clone();
    normal.clear_serial();
    let mut e = normal.with_placement(0xFFFF);
    e[MOD_TIME].fill(0);
    e[COPY_COUNTER] = 0;
    let mut bytes = CANONICAL_MAGIC.to_vec();
    bytes.extend_from_slice(&e);
    bytes.extend_from_slice(&normal.data);
    let mut meta = BTreeMap::new();
    meta.insert(META.to_string(), format!("{:08x} {:02x}", save.mod_time(), save.copy_counter()));
    Canonical { name: save.unit_name(), bytes, meta }
}

/// Decodes a canonical unit, restoring the modification time and copy counter from `gc.dentry`
/// (zero when absent). The first block is left for the writer to set.
pub fn decode(c: &Canonical) -> Result<Save> {
    let body = c.bytes.strip_prefix(CANONICAL_MAGIC.as_slice()).ok_or_else(|| invalid("not a GameCube save"))?;
    if body.len() < DENTRY {
        return Err(invalid("truncated GameCube save"));
    }
    let mut save = Save { dentry: body[..DENTRY].try_into().unwrap(), data: body[DENTRY..].to_vec() };
    save.check()?;
    if save.unit_name() != c.name {
        return Err(invalid(format!("save is named {:?}, not {:?}", save.unit_name(), c.name)));
    }
    let (mut mtime, mut copy) = (0u32, 0u8);
    if let Some(m) = c.meta.get(META) {
        let mut parts = m.split(' ');
        if let (Some(t), Some(n)) = (parts.next(), parts.next()) {
            mtime = u32::from_str_radix(t, 16).unwrap_or(0);
            copy = u8::from_str_radix(n, 16).unwrap_or(0);
        }
    }
    save.dentry[MOD_TIME].copy_from_slice(&mtime.to_be_bytes());
    save.dentry[COPY_COUNTER] = copy;
    Ok(save)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A save of `blocks` blocks filled with `fill`, named `<game><maker>-<file>`.
    pub fn save(game: &[u8; 4], maker: &[u8; 2], file: &str, blocks: u16, fill: u8) -> Save {
        let mut e = [0xFFu8; DENTRY];
        e[GAMECODE].copy_from_slice(game);
        e[MAKERCODE].copy_from_slice(maker);
        e[0x07] = 0x02;
        e[FILENAME].fill(0);
        e[FILENAME.start..FILENAME.start + file.len()].copy_from_slice(file.as_bytes());
        e[MOD_TIME].copy_from_slice(&0x3000_0000u32.to_be_bytes());
        e[0x2C..0x30].copy_from_slice(&0u32.to_be_bytes());
        e[0x30..0x32].copy_from_slice(&1u16.to_be_bytes());
        e[0x32..0x34].copy_from_slice(&3u16.to_be_bytes());
        e[0x34] = 0x04;
        e[COPY_COUNTER] = 0;
        set_be16(&mut e, BLOCK_COUNT.start, blocks);
        e[0x3C..0x40].copy_from_slice(&0x40u32.to_be_bytes());
        Save { dentry: e, data: vec![fill; blocks as usize * BLOCK] }
    }

    #[test]
    fn names_and_titles() {
        let s = save(b"GM8E", b"01", "MetroidPrime A", 1, 7);
        assert_eq!(s.unit_name(), "GM8E01-MetroidPrime A");
        assert_eq!(titles_of(&s.unit_name()), ["gc:GM8E01"]);
        assert!(titles_of("nonsense").is_empty());
        // Bytes after the first NUL of the file name don't change the identity.
        let mut t = s.clone();
        t.dentry[FILENAME.end - 1] = b'x';
        assert_eq!(t.identity(), s.identity());
    }

    #[test]
    fn canonical_ignores_placement_time_and_copies() {
        let a = save(b"GSWE", b"64", "RogueLeader", 3, 1);
        let mut b = a.clone();
        b.dentry[MOD_TIME].copy_from_slice(&0x3100_0000u32.to_be_bytes());
        b.dentry[COPY_COUNTER] = 4;
        set_be16(&mut b.dentry, FIRST_BLOCK.start, 77);
        b.dentry[UNUSED_1] = 0;
        assert_eq!(canonical(&a).bytes, canonical(&b).bytes);
        assert_ne!(canonical(&a).meta, canonical(&b).meta);
        let back = decode(&canonical(&b)).unwrap();
        assert_eq!((back.mod_time(), back.copy_counter()), (0x3100_0000, 4));
        assert_eq!(back.data, b.data);
        // Content changes do change the hash.
        let mut c = a.clone();
        c.data[100] = 9;
        assert_ne!(canonical(&a).bytes, canonical(&c).bytes);
    }

    #[test]
    fn decode_rejects_garbage() {
        let good = canonical(&save(b"GM8E", b"01", "MetroidPrime A", 1, 7));
        for n in [0, 8, 8 + DENTRY, good.bytes.len() - 1] {
            let c = Canonical { bytes: good.bytes[..n].to_vec(), ..good.clone() };
            assert!(decode(&c).is_err(), "truncated to {n}");
        }
        assert!(decode(&Canonical { name: "OTHER".into(), ..good.clone() }).is_err());
    }

    #[test]
    fn copy_protected_saves_hash_the_same_on_every_card() {
        for (file, blocks) in [("PSO_SYSTEM", 3), ("PSO3_SYSTEM", 3), ("f_zero.dat", 4)] {
            let base = save(b"GPOE", b"8P", file, blocks, 0x5A);
            // The serial is the header's 8-byte groups XORed together: give the two cards
            // different ones.
            let (mut head_a, mut head_b) = ([0u8; 32], [0u8; 32]);
            head_a[..8].copy_from_slice(b"serial-A");
            head_b[..8].copy_from_slice(b"SERIAL-b");
            let mut on_a = base.clone();
            let mut on_b = base.clone();
            on_a.set_serial(&head_a);
            on_b.set_serial(&head_b);
            assert_ne!(on_a.data, on_b.data, "{file}: the serial lands in the data");
            assert_eq!(canonical(&on_a).bytes, canonical(&on_b).bytes, "{file}");
            // Setting the same serial again gives the same bytes (the checksum covers them).
            let mut again = decode(&canonical(&on_a)).unwrap();
            again.set_serial(&head_a);
            assert_eq!(again.data, on_a.data, "{file}");
        }
        // Other saves are left alone.
        let mut other = save(b"GM8E", b"01", "MetroidPrime A", 1, 7);
        let before = other.data.clone();
        other.set_serial(&[3u8; 32]);
        assert_eq!(other.data, before);
    }
}

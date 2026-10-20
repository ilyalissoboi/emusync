//! `gc.card`: raw GameCube memory card images (DESIGN §25), as Dolphin keeps them
//! (`MemoryCardA.USA.raw`) and as real cards are dumped.
//!
//! Layout, in 8 KiB blocks: 0 the header, 1 and 2 two copies of the directory (127 entries of 64
//! bytes), 3 and 4 two copies of the block allocation table (BAT), then the data blocks. Each
//! directory and BAT copy has an update counter; the BIOS uses the copy with the higher one (the
//! first on a tie), and an update writes the other copy with the counter plus one, so the previous
//! state stays on the card as a valid backup. Every system block has an additive and an inverse
//! checksum over its big-endian 16-bit words.
//!
//! Validation follows Dolphin (`GCMemcard::Open`): a valid size and header; at most one of the
//! four directory and BAT copies corrupted (the BIOS then uses the other copy); and the active
//! directory agreeing with the active BAT about every save's chain of blocks.
//!
//! Applying changes works like a series of BIOS writes folded into one: saves are deleted and
//! written in the active copies (new data goes into free blocks, searched from after the last
//! allocated block like Dolphin's import), and the result is written to the inactive directory
//! and BAT with the update counters advanced. Saves not touched keep their blocks: some games
//! mark theirs "no move".

use super::{be16, set_be16, Save, BLOCK, DENTRY};
use crate::{ApplyOp, Canonical, Confidence, ContainerFormat, FormatError, Result, UnitRef};

/// Card sizes in megabits (59, 123, 251, 507, 1019 and 2043 free blocks).
pub const SIZES_MBIT: [u16; 6] = [4, 8, 16, 32, 64, 128];
/// Blocks per megabit.
const BLOCKS_PER_MBIT: usize = 16;
/// The header, two directories and two BATs.
const SYSTEM_BLOCKS: usize = 5;
/// Directory entries.
const ENTRIES: usize = 127;
/// BAT map entries (for blocks 5 onwards).
const BAT_SIZE: usize = 0xFFB;
const BAT_LAST: u16 = 0xFFFF;

const DIR_COUNTER: usize = 0x1FFA;
const DIR_SUMS: usize = 0x1FFC;
const DIR_PADDING: std::ops::Range<usize> = 0x1FC0..0x1FFA;
const BAT_COUNTER: usize = 0x4;
const BAT_FREE: usize = 0x6;
const BAT_LAST_ALLOCATED: usize = 0x8;
const BAT_MAP: usize = 0xA;
const HEADER_SIZE_FIELD: usize = 0x22;
const HEADER_SUMS: usize = 0x1FC;

pub struct GcCard;

fn invalid(msg: impl Into<String>) -> FormatError {
    FormatError::Invalid(msg.into())
}

/// Dolphin's `CalculateMemcardChecksums`: sums of the big-endian 16-bit words and of their
/// inverses, with `0xFFFF` stored as 0.
pub fn checksums(data: &[u8]) -> (u16, u16) {
    let (mut sum, mut inv) = (0u16, 0u16);
    for w in data.chunks_exact(2) {
        let d = u16::from_be_bytes([w[0], w[1]]);
        sum = sum.wrapping_add(d);
        inv = inv.wrapping_add(d ^ 0xFFFF);
    }
    (if sum == 0xFFFF { 0 } else { sum }, if inv == 0xFFFF { 0 } else { inv })
}

fn sums_match(stored: &[u8], at: usize, computed: (u16, u16)) -> bool {
    (be16(stored, at), be16(stored, at + 2)) == computed
}

/// The card's size in megabits, if `len` is a valid card size.
pub fn size_mbit(len: usize) -> Option<u16> {
    SIZES_MBIT.into_iter().find(|&m| m as usize * BLOCKS_PER_MBIT * BLOCK == len)
}

fn block(image: &[u8], n: usize) -> &[u8] {
    &image[n * BLOCK..(n + 1) * BLOCK]
}

fn header_ok(image: &[u8], mbit: u16) -> bool {
    let h = block(image, 0);
    be16(h, HEADER_SIZE_FIELD) == mbit && sums_match(h, HEADER_SUMS, checksums(&h[..HEADER_SUMS]))
}

fn dir_ok(d: &[u8]) -> bool {
    sums_match(d, DIR_SUMS, checksums(&d[..DIR_SUMS])) && d[DIR_PADDING].iter().all(|&b| b == 0xFF)
}

fn bat_ok(b: &[u8], data_blocks: usize) -> bool {
    if !sums_match(b, 0, checksums(&b[BAT_COUNTER..])) {
        return false;
    }
    let map = |i: usize| be16(b, BAT_MAP + 2 * i);
    let used = (0..data_blocks).filter(|&i| map(i) != 0).count();
    be16(b, BAT_FREE) as usize == data_blocks - used && (data_blocks..BAT_SIZE).all(|i| map(i) == 0)
}

fn counter(b: &[u8], at: usize) -> i16 {
    be16(b, at) as i16
}

/// A parsed card: which directory and BAT copies are in use, and its saves in directory order.
struct Card {
    blocks: usize,
    active_dir: usize,
    active_bat: usize,
    /// (directory slot, save)
    saves: Vec<(usize, Save)>,
}

impl Card {
    fn parse(image: &[u8]) -> Result<Card> {
        let mbit = size_mbit(image.len())
            .ok_or_else(|| invalid(format!("{} bytes is not a GameCube card size", image.len())))?;
        if !header_ok(image, mbit) {
            return Err(invalid("card header: bad checksum or size"));
        }
        let blocks = mbit as usize * BLOCKS_PER_MBIT;
        let data_blocks = blocks - SYSTEM_BLOCKS;
        let dirs = [block(image, 1), block(image, 2)];
        let bats = [block(image, 3), block(image, 4)];
        let dir_valid = [dir_ok(dirs[0]), dir_ok(dirs[1])];
        let bat_valid = [bat_ok(bats[0], data_blocks), bat_ok(bats[1], data_blocks)];
        let corrupted = dir_valid.iter().chain(&bat_valid).filter(|v| !**v).count();
        if corrupted > 1 {
            return Err(invalid(format!("{corrupted} of the card's 4 directory and allocation blocks are corrupted")));
        }
        // The copy with the higher update counter, the first on a tie; a corrupted copy never.
        let pick = |valid: [bool; 2], c0: i16, c1: i16| match valid {
            [true, false] => 0,
            [false, true] => 1,
            _ => usize::from(c0 < c1),
        };
        let active_dir = pick(dir_valid, counter(dirs[0], DIR_COUNTER), counter(dirs[1], DIR_COUNTER));
        let active_bat = pick(bat_valid, counter(bats[0], BAT_COUNTER), counter(bats[1], BAT_COUNTER));
        let (dir, bat) = (dirs[active_dir], bats[active_bat]);

        let next = |b: usize| -> Option<u16> {
            (SYSTEM_BLOCKS..blocks).contains(&b).then(|| be16(bat, BAT_MAP + 2 * (b - SYSTEM_BLOCKS)))
        };
        let mut saves = Vec::new();
        // Blocks already in a save's chain: Dolphin doesn't check that chains don't merge, but
        // deleting one of two saves that share blocks would free the other's.
        let mut owned = vec![false; blocks];
        for slot in 0..ENTRIES {
            let e = &dir[slot * DENTRY..(slot + 1) * DENTRY];
            if super::is_free(e) {
                continue;
            }
            let dentry: [u8; DENTRY] = e.try_into().unwrap();
            let count = be16(&dentry, 0x38) as usize;
            let mut b = be16(&dentry, 0x36) as usize;
            let mut data = Vec::with_capacity(count * BLOCK);
            for i in 0..count {
                let n = next(b)
                    .filter(|&n| n != 0)
                    .ok_or_else(|| invalid(format!("directory and allocation table disagree about save {slot}")))?;
                if std::mem::replace(&mut owned[b], true) {
                    return Err(invalid(format!("save {slot} shares block {b} with another save")));
                }
                data.extend_from_slice(block(image, b));
                if (n == BAT_LAST) != (i + 1 == count) {
                    return Err(invalid(format!("directory and allocation table disagree about save {slot}")));
                }
                b = n as usize;
            }
            let save = Save { dentry, data };
            save.check()?;
            if saves.iter().any(|(_, s): &(usize, Save)| s.identity() == save.identity()) {
                return Err(invalid(format!("two saves named {:?}", save.unit_name())));
            }
            saves.push((slot, save));
        }
        Ok(Card { blocks, active_dir, active_bat, saves })
    }
}

/// The active directory and BAT being rewritten.
struct Writer {
    image: Vec<u8>,
    blocks: usize,
    dir: Vec<u8>,
    bat: Vec<u8>,
}

impl Writer {
    fn map(&self, b: usize) -> u16 {
        be16(&self.bat, BAT_MAP + 2 * (b - SYSTEM_BLOCKS))
    }

    fn set_map(&mut self, b: usize, v: u16) {
        set_be16(&mut self.bat, BAT_MAP + 2 * (b - SYSTEM_BLOCKS), v);
    }

    fn free_blocks(&self) -> u16 {
        be16(&self.bat, BAT_FREE)
    }

    fn slot(&self, slot: usize) -> &[u8] {
        &self.dir[slot * DENTRY..(slot + 1) * DENTRY]
    }

    fn find(&self, name: &str) -> Option<usize> {
        (0..ENTRIES).find(|&s| {
            let e = self.slot(s);
            !super::is_free(e) && Save { dentry: e.try_into().unwrap(), data: vec![] }.unit_name() == name
        })
    }

    /// Frees a save's blocks and its directory slot.
    fn remove(&mut self, slot: usize) {
        let e: [u8; DENTRY] = self.slot(slot).try_into().unwrap();
        let count = be16(&e, 0x38);
        let mut b = be16(&e, 0x36) as usize;
        for _ in 0..count {
            if !(SYSTEM_BLOCKS..self.blocks).contains(&b) {
                break;
            }
            let n = self.map(b);
            self.set_map(b, 0);
            b = n as usize;
        }
        let free = self.free_blocks() + count;
        set_be16(&mut self.bat, BAT_FREE, free);
        self.dir[slot * DENTRY..(slot + 1) * DENTRY].fill(0xFF);
    }

    /// Dolphin's `NextFreeBlock`: the first free block from `start`, wrapping around.
    fn next_free(&self, start: usize) -> Option<usize> {
        let start = start.clamp(SYSTEM_BLOCKS, self.blocks);
        (start..self.blocks).chain(SYSTEM_BLOCKS..start).find(|&b| self.map(b) == 0)
    }

    /// Writes `save` into `slot`, its data into free blocks.
    fn put(&mut self, slot: usize, save: &Save) -> Result<()> {
        let count = save.block_count() as usize;
        if (self.free_blocks() as usize) < count {
            return Err(FormatError::NoSpace);
        }
        let mut b = self.next_free(be16(&self.bat, BAT_LAST_ALLOCATED) as usize).ok_or(FormatError::NoSpace)?;
        let first = b;
        for i in 0..count {
            self.image[b * BLOCK..(b + 1) * BLOCK].copy_from_slice(&save.data[i * BLOCK..(i + 1) * BLOCK]);
            // Mark it used before looking for the next one.
            self.set_map(b, BAT_LAST);
            let next =
                if i + 1 == count { BAT_LAST } else { self.next_free(b + 1).ok_or(FormatError::NoSpace)? as u16 };
            self.set_map(b, next);
            set_be16(&mut self.bat, BAT_LAST_ALLOCATED, b as u16);
            b = next as usize;
        }
        let free = self.free_blocks() - count as u16;
        set_be16(&mut self.bat, BAT_FREE, free);
        let entry = save.with_placement(first as u16);
        self.dir[slot * DENTRY..(slot + 1) * DENTRY].copy_from_slice(&entry);
        Ok(())
    }
}

impl ContainerFormat for GcCard {
    fn id(&self) -> &'static str {
        "gc.card"
    }

    fn supports_units(&self) -> bool {
        true
    }

    fn probe(&self, image: &[u8]) -> Confidence {
        match size_mbit(image.len()) {
            Some(mbit) if header_ok(image, mbit) => Confidence::Certain,
            Some(_) => Confidence::Maybe,
            None => Confidence::No,
        }
    }

    fn validate(&self, image: &[u8]) -> Result<()> {
        Card::parse(image).map(|_| ())
    }

    fn list(&self, image: &[u8]) -> Result<Vec<UnitRef>> {
        Ok(Card::parse(image)?.saves.iter().map(|(_, s)| UnitRef { name: s.unit_name() }).collect())
    }

    fn extract(&self, image: &[u8], unit: &UnitRef) -> Result<Canonical> {
        let card = Card::parse(image)?;
        let (_, save) = card
            .saves
            .iter()
            .find(|(_, s)| s.unit_name() == unit.name)
            .ok_or_else(|| FormatError::NotFound(unit.name.clone()))?;
        Ok(super::canonical(save))
    }

    fn extract_all(&self, image: &[u8]) -> Result<Vec<Canonical>> {
        Ok(Card::parse(image)?.saves.iter().map(|(_, s)| super::canonical(s)).collect())
    }

    fn titles(&self, unit_name: &str) -> Vec<String> {
        super::titles_of(unit_name)
    }

    /// Deletes and writes saves in the active directory and BAT, then stores them in the
    /// inactive copies with the update counters advanced (see the module docs). A replaced save
    /// keeps its directory slot; a new one takes the first free slot.
    fn apply(&self, image: &[u8], ops: &[ApplyOp]) -> Result<Vec<u8>> {
        let card = Card::parse(image)?;
        let mut w = Writer {
            image: image.to_vec(),
            blocks: card.blocks,
            dir: block(image, 1 + card.active_dir).to_vec(),
            bat: block(image, 3 + card.active_bat).to_vec(),
        };
        let header: Vec<u8> = image[..32].to_vec();
        for op in ops {
            match op {
                ApplyOp::Delete(name) => {
                    if let Some(slot) = w.find(name) {
                        w.remove(slot);
                    }
                }
                ApplyOp::Put(c) => {
                    let mut save = super::decode(c)?;
                    save.set_serial(&header);
                    let slot = match w.find(&c.name) {
                        Some(slot) => {
                            w.remove(slot);
                            slot
                        }
                        None => (0..ENTRIES).find(|&s| super::is_free(w.slot(s))).ok_or(FormatError::NoSpace)?,
                    };
                    w.put(slot, &save)?;
                }
            }
        }

        let dir_counter = counter(block(image, 1 + card.active_dir), DIR_COUNTER).wrapping_add(1);
        set_be16(&mut w.dir, DIR_COUNTER, dir_counter as u16);
        let (s, i) = checksums(&w.dir[..DIR_SUMS]);
        set_be16(&mut w.dir, DIR_SUMS, s);
        set_be16(&mut w.dir, DIR_SUMS + 2, i);
        let bat_counter = counter(block(image, 3 + card.active_bat), BAT_COUNTER).wrapping_add(1);
        set_be16(&mut w.bat, BAT_COUNTER, bat_counter as u16);
        let (s, i) = checksums(&w.bat[BAT_COUNTER..]);
        set_be16(&mut w.bat, 0, s);
        set_be16(&mut w.bat, 2, i);

        let mut out = w.image;
        let dir_at = (1 + (1 - card.active_dir)) * BLOCK;
        out[dir_at..dir_at + BLOCK].copy_from_slice(&w.dir);
        let bat_at = (3 + (1 - card.active_bat)) * BLOCK;
        out[bat_at..bat_at + BLOCK].copy_from_slice(&w.bat);
        Card::parse(&out)?;
        Ok(out)
    }
}

/// A freshly formatted card, the way Dolphin formats one: header fields from the arguments
/// (`serial` is the first 12 header bytes), empty directories and BATs. For tests and tools.
pub fn blank_card(mbit: u16, serial: [u8; 12], format_time: u64, shift_jis: bool) -> Vec<u8> {
    assert!(SIZES_MBIT.contains(&mbit));
    let blocks = mbit as usize * BLOCKS_PER_MBIT;
    let mut img = vec![0u8; blocks * BLOCK];
    let h = &mut img[..BLOCK];
    h.fill(0xFF);
    h[..12].copy_from_slice(&serial);
    h[0x0C..0x14].copy_from_slice(&format_time.to_be_bytes());
    h[0x14..0x18].fill(0);
    h[0x18..0x1C].fill(0);
    h[0x1C..0x20].fill(0);
    h[0x20..0x22].fill(0);
    set_be16(h, HEADER_SIZE_FIELD, mbit);
    set_be16(h, 0x24, u16::from(shift_jis));
    let (s, i) = checksums(&h[..HEADER_SUMS]);
    set_be16(h, HEADER_SUMS, s);
    set_be16(h, HEADER_SUMS + 2, i);
    for d in 1..=2 {
        let dir = &mut img[d * BLOCK..(d + 1) * BLOCK];
        dir.fill(0xFF);
        set_be16(dir, DIR_COUNTER, 0);
        let (s, i) = checksums(&dir[..DIR_SUMS]);
        set_be16(dir, DIR_SUMS, s);
        set_be16(dir, DIR_SUMS + 2, i);
    }
    for b in 3..=4 {
        let bat = &mut img[b * BLOCK..(b + 1) * BLOCK];
        bat.fill(0);
        set_be16(bat, BAT_FREE, (blocks - SYSTEM_BLOCKS) as u16);
        set_be16(bat, BAT_LAST_ALLOCATED, 4);
        let (s, i) = checksums(&bat[BAT_COUNTER..]);
        set_be16(bat, 0, s);
        set_be16(bat, 2, i);
    }
    img
}

#[cfg(test)]
mod tests {
    use super::super::tests::save;
    use super::super::{canonical, decode};
    use super::*;

    const SERIAL: [u8; 12] = *b"\x12\x34\x56\x78\x9A\xBC\xDE\xF0\x11\x22\x33\x44";

    fn names(img: &[u8]) -> Vec<String> {
        GcCard.list(img).unwrap().into_iter().map(|u| u.name).collect()
    }

    #[test]
    fn checksums_match_dolphins_empty_directory() {
        // Dolphin's Directory() constructor hard-codes these sums for an empty directory.
        let mut dir = vec![0xFFu8; BLOCK];
        set_be16(&mut dir, DIR_COUNTER, 0);
        assert_eq!(checksums(&dir[..DIR_SUMS]), (0xF003, 0));
    }

    #[test]
    fn blank_cards_are_valid_and_empty() {
        for mbit in SIZES_MBIT {
            let img = blank_card(mbit, SERIAL, 0x1234, false);
            assert_eq!(GcCard.probe(&img), Confidence::Certain);
            assert!(names(&img).is_empty());
        }
        let img = blank_card(4, SERIAL, 0, false);
        assert_eq!(GcCard.probe(&vec![0xFF; img.len()]), Confidence::Maybe);
        assert_eq!(GcCard.probe(b"short"), Confidence::No);
        assert!(GcCard.validate(&vec![0xFF; img.len()]).is_err());
    }

    #[test]
    fn round_trip_replace_and_delete() {
        let img = blank_card(4, SERIAL, 0, false);
        let a = canonical(&save(b"GM8E", b"01", "MetroidPrime A", 1, 0x11));
        let b = canonical(&save(b"GSWE", b"64", "RogueLeader", 3, 0x22));
        let img = GcCard.apply(&img, &[ApplyOp::Put(a.clone()), ApplyOp::Put(b.clone())]).unwrap();
        assert_eq!(names(&img), ["GM8E01-MetroidPrime A", "GSWE64-RogueLeader"]);
        assert_eq!(GcCard.extract_all(&img).unwrap(), vec![a.clone(), b.clone()]);

        // Replacing keeps the slot; deleting frees the blocks.
        let b2 = canonical(&save(b"GSWE", b"64", "RogueLeader", 2, 0x33));
        let img2 = GcCard.apply(&img, &[ApplyOp::Put(b2.clone())]).unwrap();
        assert_eq!(GcCard.extract_all(&img2).unwrap(), vec![a.clone(), b2.clone()]);
        let img3 = GcCard.apply(&img2, &[ApplyOp::Delete("GM8E01-MetroidPrime A".into())]).unwrap();
        assert_eq!(GcCard.extract_all(&img3).unwrap(), vec![b2.clone()]);
        let card = Card::parse(&img3).unwrap();
        let bat = block(&img3, 3 + card.active_bat);
        assert_eq!(be16(bat, BAT_FREE) as usize, 64 - SYSTEM_BLOCKS - 2);

        // Each write moves to the other copy with a higher counter; the old copy stays valid.
        let before = Card::parse(&img2).unwrap();
        assert_ne!(before.active_dir, card.active_dir);
        assert!(dir_ok(block(&img3, 1)) && dir_ok(block(&img3, 2)));
        assert!(bat_ok(block(&img3, 3), 59) && bat_ok(block(&img3, 4), 59));
    }

    #[test]
    fn untouched_saves_keep_their_blocks() {
        let img = blank_card(4, SERIAL, 0, false);
        let a = canonical(&save(b"AAAA", b"01", "a", 2, 1));
        let b = canonical(&save(b"BBBB", b"01", "b", 2, 2));
        let img = GcCard.apply(&img, &[ApplyOp::Put(a), ApplyOp::Put(b.clone())]).unwrap();
        let first_of = |img: &[u8], name: &str| {
            let c = Card::parse(img).unwrap();
            c.saves.iter().find(|(_, s)| s.unit_name() == name).map(|(_, s)| be16(&s.dentry, 0x36)).unwrap()
        };
        let b_at = first_of(&img, "BBBB01-b");
        let img = GcCard.apply(&img, &[ApplyOp::Delete("AAAA01-a".into())]).unwrap();
        let c = canonical(&save(b"CCCC", b"01", "c", 3, 3));
        let img = GcCard.apply(&img, &[ApplyOp::Put(c)]).unwrap();
        assert_eq!(first_of(&img, "BBBB01-b"), b_at);
        assert_eq!(names(&img), ["CCCC01-c", "BBBB01-b"], "the freed slot is reused");
    }

    #[test]
    fn full_cards() {
        let img = blank_card(4, SERIAL, 0, false);
        let big = canonical(&save(b"BIGG", b"01", "big", 60, 1));
        assert!(matches!(GcCard.apply(&img, &[ApplyOp::Put(big)]), Err(FormatError::NoSpace)));
        let fits = canonical(&save(b"BIGG", b"01", "big", 59, 1));
        let img = GcCard.apply(&img, &[ApplyOp::Put(fits)]).unwrap();
        let more = canonical(&save(b"MORE", b"01", "more", 1, 1));
        assert!(matches!(GcCard.apply(&img, &[ApplyOp::Put(more)]), Err(FormatError::NoSpace)));
    }

    #[test]
    fn copy_protected_saves_get_this_cards_serial() {
        let img = blank_card(4, SERIAL, 0, false);
        let pso = canonical(&save(b"GPOE", b"8P", "PSO_SYSTEM", 3, 0x5A));
        let img = GcCard.apply(&img, &[ApplyOp::Put(pso.clone())]).unwrap();
        let card = Card::parse(&img).unwrap();
        let mut expected = decode(&pso).unwrap();
        expected.set_serial(&img[..32]);
        assert_eq!(card.saves[0].1.data, expected.data);
        assert_ne!(card.saves[0].1.data, decode(&pso).unwrap().data);
        // Read back, it is the same canonical save again.
        assert_eq!(GcCard.extract_all(&img).unwrap(), vec![pso]);
    }

    #[test]
    fn corrupted_cards() {
        let img = GcCard
            .apply(&blank_card(4, SERIAL, 0, false), &[ApplyOp::Put(canonical(&save(b"GM8E", b"01", "m", 1, 1)))])
            .unwrap();
        // One bad copy: the card still reads, from the other copy.
        let active = Card::parse(&img).unwrap().active_dir;
        let mut one = img.clone();
        one[(1 + (1 - active)) * BLOCK + 10] ^= 1;
        assert_eq!(names(&one), ["GM8E01-m"]);
        // Two bad copies, or a bad header: invalid.
        let mut two = one.clone();
        two[3 * BLOCK + 20] ^= 1;
        two[4 * BLOCK + 20] ^= 1;
        assert!(GcCard.validate(&two).is_err());
        let mut header = img.clone();
        header[0x30] ^= 1;
        assert!(GcCard.validate(&header).is_err());
        // A directory that disagrees with the BAT: invalid.
        let card = Card::parse(&img).unwrap();
        let mut chain = img.clone();
        let bat_at = (3 + card.active_bat) * BLOCK;
        let first = be16(&card.saves[0].1.dentry, 0x36) as usize;
        set_be16(&mut chain, bat_at + BAT_MAP + 2 * (first - SYSTEM_BLOCKS), 0);
        let (s, i) = checksums(&chain[bat_at + BAT_COUNTER..bat_at + BLOCK]);
        set_be16(&mut chain, bat_at, s);
        set_be16(&mut chain, bat_at + 2, i);
        assert!(GcCard.validate(&chain).is_err());
    }

    /// Two saves whose chains share a block: deleting one would free the other's data.
    #[test]
    fn shared_blocks_are_invalid() {
        let saves = [save(b"GM8E", b"01", "a", 1, 1), save(b"GM8E", b"01", "b", 1, 2)];
        let img = GcCard.apply(&blank_card(4, SERIAL, 0, false), &saves.map(|s| ApplyOp::Put(canonical(&s)))).unwrap();
        let card = Card::parse(&img).unwrap();
        let first = be16(&card.saves[0].1.dentry, 0x36);
        let dir_at = (1 + card.active_dir) * BLOCK;
        let mut shared = img.clone();
        set_be16(&mut shared, dir_at + card.saves[1].0 * DENTRY + 0x36, first);
        let (s, i) = checksums(&shared[dir_at..dir_at + DIR_SUMS]);
        set_be16(&mut shared, dir_at + DIR_SUMS, s);
        set_be16(&mut shared, dir_at + DIR_SUMS + 2, i);
        let err = GcCard.validate(&shared).unwrap_err().to_string();
        assert!(err.contains("shares block"), "{err}");
    }
}

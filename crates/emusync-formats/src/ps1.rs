//! `ps1.card`: PlayStation memory cards (DESIGN §5.3, §29), one unit per save.
//!
//! A card is 128 KiB: 16 blocks of 8 KiB, each 64 frames of 128 bytes. Block 0 is the card's own:
//! frame 0 the header (`MC`), frames 1 to 15 the directory (one entry for each data block 1 to
//! 15), frames 16 to 35 the broken-sector list, 36 to 55 its replacement data, and frame 63 a
//! write test. A save is a chain of data blocks. The directory entry of its first block has state
//! `0x51`, the save's name and its size in bytes; the entries of its other blocks have `0x52`
//! (middle) or `0x53` (last). Each entry holds the next block of the chain (`0xFFFF` at the end).
//! Deleting a save marks its entries `0xA1`, `0xA2` and `0xA3` and leaves its data, so it can be
//! undeleted; `0xA0` is a block unused since the card was formatted. Each directory frame ends
//! with the XOR of its other 127 bytes.
//!
//! Containers are the raw image, which every emulator that keeps a PS1 card as a file uses
//! (DuckStation's `.mcd`, ePSXe's and mednafen's `.mcr`, Beetle's and SwanStation's `.srm` in
//! RetroArch), or the raw image behind a header: DexDrive's `.gme` (3904 bytes) and Connectix
//! Virtual Game Station's `.mem` (64 bytes). The header is kept on write; a `.gme` header's copies
//! of the directory states and links are updated, and the descriptions of blocks that changed are
//! cleared.
//!
//! A save is identified by its name, as on the console, whose BIOS refuses a second save with
//! the same name. The name is the game's serial behind a region code (`BASLUS-00067…`), so a
//! unit's title key is `ps1:SLUS-00067`. The canonical form is a magic, the name, the size field
//! and the data blocks in chain order: what the game wrote. Where the blocks are on the card and
//! the rest of the directory entries are rebuilt on write.

use crate::{ApplyOp, Canonical, Confidence, ContainerFormat, FormatError, Result, UnitRef};
use std::collections::BTreeMap;
use std::ops::Range;

/// Size of a raw card image.
pub const CARD_SIZE: usize = 0x20000;
/// Size of a block: a save is whole blocks.
pub const BLOCK: usize = 0x2000;
/// Size of a frame (a directory entry is one).
pub const FRAME: usize = 0x80;
/// Data blocks on a card (1 to 15).
pub const DATA_BLOCKS: usize = 15;

const USED_FIRST: u32 = 0x51;
const USED_MIDDLE: u32 = 0x52;
const USED_LAST: u32 = 0x53;
const FREE: u32 = 0xA0;
const DELETED_FIRST: u32 = 0xA1;
const DELETED_MIDDLE: u32 = 0xA2;
const DELETED_LAST: u32 = 0xA3;
const NO_NEXT: u16 = 0xFFFF;

// Directory entry fields.
const STATE: Range<usize> = 0x00..0x04;
const SIZE: Range<usize> = 0x04..0x08;
const NEXT: Range<usize> = 0x08..0x0A;
/// The name: up to 20 characters and a NUL (21 bytes, all of it name if there is no NUL).
const NAME: Range<usize> = 0x0A..0x1F;
const CHECKSUM: usize = 0x7F;

const GME_MAGIC: &[u8] = b"123-456-STD";
const GME_HEADER: usize = 0xF40;
/// A `.gme` header's copy of byte 0 of frames 0 to 15 (`M`, then the directory states).
const GME_STATES: usize = 0x15;
/// Its copy of byte 8 of frames 0 to 15 (0, then the low byte of each next-block link).
const GME_LINKS: usize = 0x25;
/// Its descriptions, one of 256 bytes for each data block.
const GME_DESCRIPTIONS: usize = 0x40;
const GME_DESCRIPTION: usize = 0x100;
const VGS_MAGIC: &[u8] = b"VgsM";
const VGS_HEADER: usize = 0x40;

const CANONICAL_MAGIC: &[u8; 8] = b"ES-P1U01";

pub struct Ps1Card;

fn invalid(msg: impl Into<String>) -> FormatError {
    FormatError::Invalid(msg.into())
}

fn le32(b: &[u8], r: Range<usize>) -> u32 {
    u32::from_le_bytes(b[r].try_into().unwrap())
}

fn le16(b: &[u8], r: Range<usize>) -> u16 {
    u16::from_le_bytes(b[r].try_into().unwrap())
}

/// XOR of a frame's first 127 bytes: its checksum.
pub fn checksum(frame: &[u8]) -> u8 {
    frame[..CHECKSUM].iter().fold(0, |x, &b| x ^ b)
}

/// The header in front of the card, by the container's size and magic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Variant {
    Raw,
    Gme,
    Vgs,
}

impl Variant {
    fn of(image: &[u8]) -> Option<Variant> {
        match image.len() {
            CARD_SIZE => Some(Variant::Raw),
            n if n == GME_HEADER + CARD_SIZE && image.starts_with(GME_MAGIC) => Some(Variant::Gme),
            n if n == VGS_HEADER + CARD_SIZE && image.starts_with(VGS_MAGIC) => Some(Variant::Vgs),
            _ => None,
        }
    }

    fn header_len(self) -> usize {
        match self {
            Variant::Raw => 0,
            Variant::Gme => GME_HEADER,
            Variant::Vgs => VGS_HEADER,
        }
    }
}

/// The card inside a container, with the container's variant.
fn split(image: &[u8]) -> Result<(Variant, &[u8])> {
    let v = Variant::of(image).ok_or_else(|| {
        invalid(format!(
            "{} bytes is not a PlayStation memory card: a raw card is {CARD_SIZE} bytes, a .gme {} and a .mem {}",
            image.len(),
            GME_HEADER + CARD_SIZE,
            VGS_HEADER + CARD_SIZE
        ))
    })?;
    Ok((v, &image[v.header_len()..]))
}

fn formatted(card: &[u8]) -> bool {
    card.starts_with(b"MC")
}

/// The directory entry of data block `b` (1 to 15).
fn entry(card: &[u8], b: usize) -> &[u8] {
    &card[b * FRAME..(b + 1) * FRAME]
}

fn state(card: &[u8], b: usize) -> u32 {
    le32(entry(card, b), STATE)
}

fn is_free(state: u32) -> bool {
    matches!(state, FREE | DELETED_FIRST | DELETED_MIDDLE | DELETED_LAST)
}

fn data(card: &[u8], b: usize) -> &[u8] {
    &card[b * BLOCK..(b + 1) * BLOCK]
}

/// A save: its name (without the NUL), the size its directory entry gives, and its data blocks
/// in chain order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Save {
    pub name: Vec<u8>,
    pub size: u32,
    pub data: Vec<u8>,
}

fn c_str(b: &[u8]) -> &[u8] {
    b.iter().position(|&c| c == 0).map_or(b, |n| &b[..n])
}

impl Save {
    /// The name, bytes mapped one to one to Latin-1, so any name round-trips.
    pub fn unit_name(&self) -> String {
        self.name.iter().map(|&c| c as char).collect()
    }

    pub fn blocks(&self) -> usize {
        self.data.len() / BLOCK
    }

    /// Checks the save can go on a card: a name that fits, 1 to 15 whole blocks.
    pub fn check(&self) -> Result<()> {
        if self.name.is_empty() || self.name.len() > NAME.len() || self.name.contains(&0) {
            return Err(invalid(format!("save name {:?} doesn't fit a directory entry", self.unit_name())));
        }
        if self.data.is_empty() || self.data.len() % BLOCK != 0 || self.blocks() > DATA_BLOCKS {
            return Err(invalid(format!(
                "save {:?} has {} bytes, not 1 to 15 blocks",
                self.unit_name(),
                self.data.len()
            )));
        }
        Ok(())
    }
}

/// A parsed card: its saves, each with the blocks of its chain, in directory order.
struct Card {
    saves: Vec<(Vec<usize>, Save)>,
}

impl Card {
    fn parse(card: &[u8]) -> Result<Card> {
        if !formatted(card) {
            return Err(invalid("the card is not formatted (its header has no MC)"));
        }
        let mut owner: [Option<usize>; DATA_BLOCKS + 1] = [None; DATA_BLOCKS + 1];
        for b in 1..=DATA_BLOCKS {
            let s = state(card, b);
            if !is_free(s) && !matches!(s, USED_FIRST | USED_MIDDLE | USED_LAST) {
                return Err(invalid(format!("block {b} has the unknown directory state {s:#x}")));
            }
        }
        let mut saves: Vec<(Vec<usize>, Save)> = Vec::new();
        for first in (1..=DATA_BLOCKS).filter(|&b| state(card, b) == USED_FIRST) {
            let broken = || invalid(format!("the save in block {first} has a broken chain of blocks"));
            let mut chain = vec![first];
            let mut b = first;
            loop {
                let next = le16(entry(card, b), NEXT);
                if next == NO_NEXT {
                    break;
                }
                let n = next as usize + 1;
                if n > DATA_BLOCKS || chain.contains(&n) || !matches!(state(card, n), USED_MIDDLE | USED_LAST) {
                    return Err(broken());
                }
                chain.push(n);
                b = n;
            }
            // Middle blocks are 0x52, the last 0x53 (a one-block save is only its 0x51).
            let last = *chain.last().unwrap();
            if (chain.len() > 1 && state(card, last) != USED_LAST)
                || chain.iter().skip(1).take(chain.len().saturating_sub(2)).any(|&m| state(card, m) != USED_MIDDLE)
            {
                return Err(broken());
            }
            for &c in &chain {
                if let Some(other) = owner[c].replace(first) {
                    return Err(invalid(format!("the saves in blocks {other} and {first} share block {c}")));
                }
            }
            let e = entry(card, first);
            let save = Save {
                name: c_str(&e[NAME]).to_vec(),
                size: le32(e, SIZE),
                data: chain.iter().flat_map(|&c| data(card, c)).copied().collect(),
            };
            if save.name.is_empty() {
                return Err(invalid(format!("the save in block {first} has no name")));
            }
            if saves.iter().any(|(_, s)| s.name == save.name) {
                return Err(invalid(format!("two saves named {:?}", save.unit_name())));
            }
            saves.push((chain, save));
        }
        if let Some(b) = (1..=DATA_BLOCKS).find(|&b| owner[b].is_none() && !is_free(state(card, b))) {
            return Err(invalid(format!("block {b} belongs to no save (its first block is missing)")));
        }
        Ok(Card { saves })
    }
}

/// Deleted saves, as the chains of `0xA1`… entries the BIOS leaves behind, plus any deleted block
/// no chain reaches, alone. A deleted save is overwritten only as a whole, so no chain that can
/// be undeleted runs into a block that now holds another save.
fn deleted_saves(card: &[u8]) -> Vec<Vec<usize>> {
    let mut seen = [false; DATA_BLOCKS + 1];
    let mut out = Vec::new();
    for first in (1..=DATA_BLOCKS).filter(|&b| state(card, b) == DELETED_FIRST) {
        let mut chain = vec![first];
        seen[first] = true;
        let mut b = first;
        loop {
            let next = le16(entry(card, b), NEXT) as usize + 1;
            if next > DATA_BLOCKS || seen[next] || !matches!(state(card, next), DELETED_MIDDLE | DELETED_LAST) {
                break;
            }
            seen[next] = true;
            chain.push(next);
            b = next;
        }
        out.push(chain);
    }
    out.extend(
        (1..=DATA_BLOCKS)
            .filter(|&b| !seen[b] && matches!(state(card, b), DELETED_MIDDLE | DELETED_LAST))
            .map(|b| vec![b]),
    );
    out.sort();
    out
}

/// The card being rewritten.
struct Writer {
    card: Vec<u8>,
    /// Data blocks whose directory entry was rewritten.
    touched: [bool; DATA_BLOCKS + 1],
}

impl Writer {
    fn find(&self, name: &str) -> Option<Vec<usize>> {
        Card::parse(&self.card).ok()?.saves.into_iter().find(|(_, s)| s.unit_name() == name).map(|(c, _)| c)
    }

    fn write_entry(&mut self, b: usize, f: impl FnOnce(&mut [u8])) {
        let e = &mut self.card[b * FRAME..(b + 1) * FRAME];
        f(e);
        e[CHECKSUM] = checksum(e);
        self.touched[b] = true;
    }

    /// An entry as freshly formatted.
    fn clear(&mut self, b: usize) {
        self.write_entry(b, |e| {
            e.fill(0);
            e[STATE].copy_from_slice(&FREE.to_le_bytes());
            e[NEXT].copy_from_slice(&NO_NEXT.to_le_bytes());
        });
    }

    /// Deletes a save the way the BIOS does: its entries are marked deleted, chain and data left.
    fn delete(&mut self, chain: &[usize]) {
        for (i, &b) in chain.iter().enumerate() {
            let s = match i {
                0 => DELETED_FIRST,
                _ if i + 1 == chain.len() => DELETED_LAST,
                _ => DELETED_MIDDLE,
            };
            self.write_entry(b, |e| e[STATE].copy_from_slice(&s.to_le_bytes()));
        }
    }

    /// Writes `save`, replacing the save with its name. Its blocks come from the save it
    /// replaces first, then from blocks unused since formatting, then from whole deleted saves.
    fn put(&mut self, save: &Save) -> Result<()> {
        let mut blocks = self.find(&save.unit_name()).unwrap_or_default();
        for &b in &blocks {
            self.clear(b);
        }
        let unused: Vec<usize> =
            (1..=DATA_BLOCKS).filter(|&b| state(&self.card, b) == FREE && !blocks.contains(&b)).collect();
        blocks.extend(unused);
        for deleted in deleted_saves(&self.card) {
            if blocks.len() >= save.blocks() {
                break;
            }
            for &b in &deleted {
                self.clear(b);
            }
            blocks.extend(deleted);
        }
        if blocks.len() < save.blocks() {
            return Err(FormatError::NoSpace);
        }
        let chain = &blocks[..save.blocks()];
        for (i, &b) in chain.iter().enumerate() {
            let next = chain.get(i + 1).map_or(NO_NEXT, |&n| (n - 1) as u16);
            let s = match i {
                0 => USED_FIRST,
                _ if i + 1 == chain.len() => USED_LAST,
                _ => USED_MIDDLE,
            };
            self.write_entry(b, |e| {
                e.fill(0);
                e[STATE].copy_from_slice(&s.to_le_bytes());
                e[NEXT].copy_from_slice(&next.to_le_bytes());
                if i == 0 {
                    e[SIZE].copy_from_slice(&save.size.to_le_bytes());
                    e[NAME.start..NAME.start + save.name.len()].copy_from_slice(&save.name);
                }
            });
            self.card[b * BLOCK..(b + 1) * BLOCK].copy_from_slice(&save.data[i * BLOCK..(i + 1) * BLOCK]);
        }
        Ok(())
    }
}

/// Title keys of a save, from its name: `BASLUS-00067…` → `ps1:SLUS-00067`. A PocketStation
/// program has `P` where the serial has `-` (`BASCPSP1000…`); its key has the `-`.
pub fn titles_of(name: &str) -> Vec<String> {
    let b = name.as_bytes();
    let serial = b.get(2..12).filter(|s| {
        s[..4].iter().all(u8::is_ascii_uppercase)
            && matches!(s[4], b'-' | b'P')
            && s[5..].iter().all(u8::is_ascii_digit)
    });
    match (b.first(), b.get(1), serial) {
        (Some(b'B'), Some(b'A' | b'E' | b'I'), Some(s)) => {
            vec![format!("ps1:{}-{}", &name[2..6], std::str::from_utf8(&s[5..]).unwrap())]
        }
        _ => vec![],
    }
}

/// The canonical unit of a save.
pub fn canonical(save: &Save) -> Canonical {
    let mut bytes = CANONICAL_MAGIC.to_vec();
    // A name too long for the card (only made by hand) is cut, and decoding refuses it.
    let mut name = [0u8; NAME.end - NAME.start];
    let n = save.name.len().min(name.len());
    name[..n].copy_from_slice(&save.name[..n]);
    bytes.extend_from_slice(&name);
    bytes.extend_from_slice(&save.size.to_le_bytes());
    bytes.extend_from_slice(&save.data);
    Canonical { name: save.unit_name(), bytes, meta: BTreeMap::new() }
}

/// Decodes a canonical unit.
pub fn decode(c: &Canonical) -> Result<Save> {
    let body = c.bytes.strip_prefix(CANONICAL_MAGIC.as_slice()).ok_or_else(|| invalid("not a PlayStation save"))?;
    let head = NAME.len() + 4;
    if body.len() < head {
        return Err(invalid("truncated PlayStation save"));
    }
    let save = Save {
        name: c_str(&body[..NAME.len()]).to_vec(),
        size: u32::from_le_bytes(body[NAME.len()..head].try_into().unwrap()),
        data: body[head..].to_vec(),
    };
    save.check()?;
    if save.unit_name() != c.name {
        return Err(invalid(format!("save is named {:?}, not {:?}", save.unit_name(), c.name)));
    }
    Ok(save)
}

impl ContainerFormat for Ps1Card {
    fn id(&self) -> &'static str {
        "ps1.card"
    }

    fn supports_units(&self) -> bool {
        true
    }

    /// `Certain` for a formatted card whose header checksum holds, in any of the three
    /// containers; `Maybe` for one of a card's sizes that isn't (an unformatted card).
    fn probe(&self, image: &[u8]) -> Confidence {
        match split(image) {
            Ok((_, card)) if formatted(card) && checksum(&card[..FRAME]) == card[CHECKSUM] => Confidence::Certain,
            Ok(_) => Confidence::Maybe,
            Err(_) => Confidence::No,
        }
    }

    fn validate(&self, image: &[u8]) -> Result<()> {
        Card::parse(split(image)?.1).map(|_| ())
    }

    fn list(&self, image: &[u8]) -> Result<Vec<UnitRef>> {
        Ok(Card::parse(split(image)?.1)?.saves.iter().map(|(_, s)| UnitRef { name: s.unit_name() }).collect())
    }

    fn extract(&self, image: &[u8], unit: &UnitRef) -> Result<Canonical> {
        Card::parse(split(image)?.1)?
            .saves
            .iter()
            .find(|(_, s)| s.unit_name() == unit.name)
            .map(|(_, s)| canonical(s))
            .ok_or_else(|| FormatError::NotFound(unit.name.clone()))
    }

    fn extract_all(&self, image: &[u8]) -> Result<Vec<Canonical>> {
        Ok(Card::parse(split(image)?.1)?.saves.iter().map(|(_, s)| canonical(s)).collect())
    }

    fn titles(&self, unit_name: &str) -> Vec<String> {
        titles_of(unit_name)
    }

    /// Deletes and writes saves (see the module docs); untouched saves keep their blocks.
    fn apply(&self, image: &[u8], ops: &[ApplyOp]) -> Result<Vec<u8>> {
        let (variant, card) = split(image)?;
        Card::parse(card)?;
        let mut w = Writer { card: card.to_vec(), touched: [false; DATA_BLOCKS + 1] };
        for op in ops {
            match op {
                ApplyOp::Delete(name) => {
                    if let Some(chain) = w.find(name) {
                        w.delete(&chain);
                    }
                }
                ApplyOp::Put(c) => w.put(&decode(c)?)?,
            }
        }
        Card::parse(&w.card)?;
        let mut out = image[..variant.header_len()].to_vec();
        if variant == Variant::Gme {
            for f in 0..=DATA_BLOCKS {
                out[GME_STATES + f] = w.card[f * FRAME];
                out[GME_LINKS + f] = w.card[f * FRAME + NEXT.start];
            }
            for b in (1..=DATA_BLOCKS).filter(|&b| w.touched[b]) {
                let at = GME_DESCRIPTIONS + (b - 1) * GME_DESCRIPTION;
                out[at..at + GME_DESCRIPTION].fill(0);
            }
        }
        out.extend_from_slice(&w.card);
        Ok(out)
    }

    /// A card as DuckStation formats one when its card file doesn't exist yet. Only for a raw
    /// card: a missing `.gme` or `.mem` stays missing.
    fn empty(&self, file_name: &str) -> Option<Vec<u8>> {
        let ext = file_name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
        (!matches!(ext.as_str(), "gme" | "mem" | "vgs")).then(blank_card)
    }
}

/// A freshly formatted raw card, as DuckStation formats one: every directory entry free, an
/// empty broken-sector list, the write-test frame a copy of the header, the data blocks `0xFF`.
pub fn blank_card() -> Vec<u8> {
    let mut card = vec![0xFFu8; CARD_SIZE];
    let frame = |card: &mut Vec<u8>, f: usize, init: &dyn Fn(&mut [u8])| {
        let fr = &mut card[f * FRAME..(f + 1) * FRAME];
        fr.fill(0);
        init(fr);
        fr[CHECKSUM] = checksum(fr);
    };
    frame(&mut card, 0, &|f| f[..2].copy_from_slice(b"MC"));
    for f in 1..=DATA_BLOCKS {
        frame(&mut card, f, &|e| {
            e[STATE].copy_from_slice(&FREE.to_le_bytes());
            e[NEXT].copy_from_slice(&NO_NEXT.to_le_bytes());
        });
    }
    for f in 16..36 {
        frame(&mut card, f, &|e| {
            e[0..4].fill(0xFF);
            e[NEXT].fill(0xFF);
        });
    }
    card[36 * FRAME..63 * FRAME].fill(0);
    card.copy_within(0..FRAME, 63 * FRAME);
    card
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A save of `blocks` blocks of `fill`, as a game writes one: a title frame first.
    pub fn save(name: &str, blocks: usize, fill: u8) -> Save {
        let mut data = vec![fill; blocks * BLOCK];
        data[..4].copy_from_slice(&[b'S', b'C', 0x11, blocks as u8]);
        Save { name: name.as_bytes().to_vec(), size: (blocks * BLOCK) as u32, data }
    }

    fn names(img: &[u8]) -> Vec<String> {
        Ps1Card.list(img).unwrap().into_iter().map(|u| u.name).collect()
    }

    fn put(img: &[u8], saves: &[&Save]) -> Vec<u8> {
        let ops: Vec<_> = saves.iter().map(|s| ApplyOp::Put(canonical(s))).collect();
        Ps1Card.apply(img, &ops).unwrap()
    }

    /// The data blocks of a save on a card, in chain order.
    fn chain(img: &[u8], name: &str) -> Vec<usize> {
        Card::parse(split(img).unwrap().1).unwrap().saves.into_iter().find(|(_, s)| s.unit_name() == name).unwrap().0
    }

    /// A `.gme` file: DexDrive's header (psx-spx) in front of `card`.
    fn gme(card: &[u8]) -> Vec<u8> {
        let mut h = vec![0u8; GME_HEADER];
        h[..12].copy_from_slice(b"123-456-STD\0");
        h[0x10..0x15].copy_from_slice(&[0, 0, 1, 0, 1]);
        for f in 0..=DATA_BLOCKS {
            h[GME_STATES + f] = card[f * FRAME];
            h[GME_LINKS + f] = card[f * FRAME + 8];
        }
        for b in 0..DATA_BLOCKS {
            let at = GME_DESCRIPTIONS + b * GME_DESCRIPTION;
            h[at..at + 12].copy_from_slice(format!("comment {b:02}\0\0").as_bytes());
        }
        h.extend_from_slice(card);
        h
    }

    fn vgs(card: &[u8]) -> Vec<u8> {
        let mut h = vec![0u8; VGS_HEADER];
        h[..4].copy_from_slice(b"VgsM");
        h[4] = 1;
        h[8] = 1;
        h[12] = 1;
        h.extend_from_slice(card);
        h
    }

    #[test]
    fn blank_cards_match_duckstation_and_the_bios() {
        let card = blank_card();
        assert_eq!(card.len(), CARD_SIZE);
        // The header checksum is M ^ C; a free directory entry's is A0 ^ FF ^ FF.
        assert_eq!(card[CHECKSUM], 0x0E);
        assert_eq!(card[FRAME + CHECKSUM], 0xA0);
        assert_eq!(&card[63 * FRAME..64 * FRAME], &card[..FRAME], "the write-test frame");
        assert_eq!(Ps1Card.probe(&card), Confidence::Certain);
        assert!(names(&card).is_empty());
        assert_eq!(Ps1Card.probe(&vec![0u8; CARD_SIZE]), Confidence::Maybe);
        assert!(Ps1Card.validate(&vec![0u8; CARD_SIZE]).is_err(), "unformatted");
        assert_eq!(Ps1Card.probe(&card[..CARD_SIZE - 1]), Confidence::No);
        assert!(Ps1Card.empty("shared_card_1.mcd").is_some());
        assert!(Ps1Card.empty("card.gme").is_none());
    }

    #[test]
    fn titles() {
        assert_eq!(titles_of("BASLUS-00067FF7"), ["ps1:SLUS-00067"]);
        assert_eq!(titles_of("BESCES-00344"), ["ps1:SCES-00344"]);
        assert_eq!(titles_of("BISLPS-01234GAME"), ["ps1:SLPS-01234"]);
        assert_eq!(titles_of("BASCPSP10001"), ["ps1:SCPS-10001"], "a PocketStation program");
        assert!(titles_of("BXSLUS-00067").is_empty());
        assert!(titles_of("homebrew").is_empty());
        assert!(titles_of("BASLUS-0006").is_empty());
    }

    #[test]
    fn round_trip_replace_and_delete() {
        let ff7 = save("BASCUS-94163FF7-S01", 1, 0x11);
        let gt = save("BASCUS-94194GT", 3, 0x22);
        let img = put(&blank_card(), &[&ff7, &gt]);
        assert_eq!(names(&img), ["BASCUS-94163FF7-S01", "BASCUS-94194GT"]);
        assert_eq!(Ps1Card.extract_all(&img).unwrap(), vec![canonical(&ff7), canonical(&gt)]);
        assert_eq!(chain(&img, "BASCUS-94194GT"), [2, 3, 4]);
        let card = split(&img).unwrap().1;
        for b in 1..=4 {
            assert_eq!(entry(card, b)[CHECKSUM], checksum(entry(card, b)), "block {b}");
        }
        assert_eq!((state(card, 2), state(card, 3), state(card, 4)), (USED_FIRST, USED_MIDDLE, USED_LAST));

        // A replaced save keeps its blocks; a longer one takes more after them.
        let gt2 = save("BASCUS-94194GT", 4, 0x33);
        let img = put(&img, &[&gt2]);
        assert_eq!(chain(&img, "BASCUS-94194GT"), [2, 3, 4, 5]);
        assert_eq!(Ps1Card.extract_all(&img).unwrap(), vec![canonical(&ff7), canonical(&gt2)]);

        // Deleting works as the BIOS does: the entries say deleted, the data stays.
        let img = Ps1Card.apply(&img, &[ApplyOp::Delete("BASCUS-94163FF7-S01".into())]).unwrap();
        assert_eq!(names(&img), ["BASCUS-94194GT"]);
        let card = split(&img).unwrap().1;
        assert_eq!(state(card, 1), DELETED_FIRST);
        assert_eq!(&entry(card, 1)[NAME][..19], b"BASCUS-94163FF7-S01");
        assert_eq!(data(card, 1), &ff7.data[..]);
        // Deleting what isn't there does nothing.
        assert_eq!(Ps1Card.apply(&img, &[ApplyOp::Delete("nothing".into())]).unwrap(), img);
    }

    #[test]
    fn untouched_saves_keep_their_blocks_and_deleted_saves_go_last() {
        let a = save("BASLUS-00001A", 2, 1);
        let b = save("BASLUS-00002B", 2, 2);
        let img = put(&blank_card(), &[&a, &b]);
        let img = Ps1Card.apply(&img, &[ApplyOp::Delete(a.unit_name())]).unwrap();
        // Blocks never used come before the deleted save's, which can still be undeleted.
        let c = save("BASLUS-00003C", 3, 3);
        let img = put(&img, &[&c]);
        assert_eq!(chain(&img, "BASLUS-00002B"), [3, 4]);
        assert_eq!(chain(&img, "BASLUS-00003C"), [5, 6, 7]);
        assert_eq!(state(split(&img).unwrap().1, 1), DELETED_FIRST);
        // A save that needs the deleted one's blocks takes the whole deleted save.
        let d = save("BASLUS-00004D", 9, 4);
        let img = put(&img, &[&d]);
        assert_eq!(chain(&img, "BASLUS-00004D"), [8, 9, 10, 11, 12, 13, 14, 15, 1]);
        assert_eq!(state(split(&img).unwrap().1, 2), FREE, "the rest of the deleted save is cleared");
    }

    #[test]
    fn full_cards() {
        let big = save("BASLUS-00001BIG", 15, 1);
        let img = put(&blank_card(), &[&big]);
        let more = save("BASLUS-00002MORE", 1, 2);
        assert!(matches!(Ps1Card.apply(&img, &[ApplyOp::Put(canonical(&more))]), Err(FormatError::NoSpace)));
        // Replacing a save that fills the card fits, in its own blocks.
        let big2 = save("BASLUS-00001BIG", 15, 3);
        let img = put(&img, &[&big2]);
        assert_eq!(Ps1Card.extract_all(&img).unwrap(), vec![canonical(&big2)]);
        // A save of 16 blocks can't be decoded at all.
        let too_big = Save { data: vec![0; 16 * BLOCK], ..big2.clone() };
        assert!(decode(&canonical(&too_big)).is_err());
    }

    #[test]
    fn headered_cards_keep_their_header() {
        let ff7 = save("BASCUS-94163FF7-S01", 2, 5);
        for (make, header) in [(gme as fn(&[u8]) -> Vec<u8>, GME_HEADER), (vgs, VGS_HEADER)] {
            let img = make(&blank_card());
            assert_eq!(Ps1Card.probe(&img), Confidence::Certain);
            let out = put(&img, &[&ff7]);
            assert_eq!(out.len(), img.len());
            assert_eq!(names(&out), ["BASCUS-94163FF7-S01"]);
            assert_eq!(&out[header..], &put(&blank_card(), &[&ff7])[..], "the same card behind the header");
            if header == VGS_HEADER {
                assert_eq!(&out[..header], &img[..header]);
            }
        }
        // A .gme header's copies of the directory follow the card, and the descriptions of the
        // blocks that changed are cleared.
        let out = put(&gme(&blank_card()), &[&ff7]);
        assert_eq!(&out[GME_STATES..GME_STATES + 4], &[b'M', 0x51, 0x53, 0xA0]);
        assert_eq!(&out[GME_LINKS..GME_LINKS + 4], &[0, 1, 0xFF, 0xFF]);
        let description = |b: usize| &out[GME_DESCRIPTIONS + b * GME_DESCRIPTION..][..12];
        assert_eq!(description(0), &[0; 12]);
        assert_eq!(description(1), &[0; 12]);
        assert_eq!(description(2), b"comment 02\0\0");
        // A .gme of a raw card's size is a raw card (DuckStation reads them so).
        assert_eq!(Ps1Card.probe(&blank_card()), Confidence::Certain);
        assert_eq!(Ps1Card.probe(&gme(&blank_card())[..GME_HEADER + 10]), Confidence::No);
    }

    #[test]
    fn canonical_form() {
        let a = save("BASLUS-00067SAVE", 2, 7);
        let c = canonical(&a);
        assert_eq!(decode(&c).unwrap(), a);
        // The same save at other blocks of another card is the same unit.
        let filler = save("BESLES-00001X", 5, 1);
        let one = put(&blank_card(), &[&a]);
        let two = put(&put(&blank_card(), &[&filler]), &[&a]);
        assert_ne!(chain(&one, "BASLUS-00067SAVE"), chain(&two, "BASLUS-00067SAVE"));
        assert_eq!(Ps1Card.extract(&one, &UnitRef { name: c.name.clone() }).unwrap(), c);
        assert_eq!(Ps1Card.extract(&two, &UnitRef { name: c.name.clone() }).unwrap(), c);
        // Garbage is refused.
        for n in [0, 8, 8 + NAME.len(), c.bytes.len() - 1] {
            assert!(decode(&Canonical { bytes: c.bytes[..n].to_vec(), ..c.clone() }).is_err(), "truncated to {n}");
        }
        assert!(decode(&Canonical { name: "other".into(), ..c.clone() }).is_err());
        // A name of 21 characters has no NUL on the card, and still round-trips.
        let long = save("BASLUS-00067ABCDEFGHI", 1, 1);
        assert_eq!(long.name.len(), 21);
        let img = put(&blank_card(), &[&long]);
        assert_eq!(names(&img), [long.unit_name()]);
        assert!(Ps1Card.apply(&img, &[ApplyOp::Put(canonical(&save("BASLUS-00067ABCDEFGHIJ", 1, 1)))]).is_err());
    }

    /// Hand-made damage: every card the reader refuses.
    #[test]
    fn corrupted_cards() {
        let a = save("BASLUS-00001A", 3, 1);
        let b = save("BASLUS-00002B", 1, 2);
        let img = put(&blank_card(), &[&a, &b]);
        let set = |img: &[u8], blk: usize, f: &dyn Fn(&mut [u8])| {
            let mut out = img.to_vec();
            let e = &mut out[blk * FRAME..(blk + 1) * FRAME];
            f(e);
            e[CHECKSUM] = checksum(e);
            out
        };
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("unformatted", {
                let mut c = img.clone();
                c[0] = 0;
                c
            }),
            ("unknown state", set(&img, 4, &|e| e[0] = 0x42)),
            ("link off the card", set(&img, 1, &|e| e[NEXT].copy_from_slice(&20u16.to_le_bytes()))),
            ("link to a free block", set(&img, 1, &|e| e[NEXT].copy_from_slice(&9u16.to_le_bytes()))),
            ("loop", set(&img, 3, &|e| e[NEXT].copy_from_slice(&1u16.to_le_bytes()))),
            ("last block not 0x53", set(&img, 3, &|e| e[0] = 0x52)),
            ("orphan block", set(&img, 1, &|e| e[NEXT].copy_from_slice(&NO_NEXT.to_le_bytes()))),
            ("no name", set(&img, 4, &|e| e[NAME].fill(0))),
            ("same name twice", set(&img, 4, &|e| e[NAME][..13].copy_from_slice(b"BASLUS-00001A"))),
            ("shared block", {
                // Block 4 (save b) also claims save a's second block.
                let c = set(&img, 4, &|e| e[NEXT].copy_from_slice(&1u16.to_le_bytes()));
                set(&c, 2, &|e| e[0] = 0x53)
            }),
        ];
        for (what, card) in cases {
            assert!(Ps1Card.validate(&card).is_err(), "{what}");
            assert!(Ps1Card.apply(&card, &[]).is_err(), "{what}: never written");
        }
        // A wrong checksum alone is accepted, as DuckStation does, and fixed on the next write.
        let mut bad_sum = img.clone();
        bad_sum[2 * FRAME + CHECKSUM] ^= 0xFF;
        assert_eq!(names(&bad_sum), ["BASLUS-00001A", "BASLUS-00002B"]);
    }

    /// The card a PS1 BIOS leaves after deleting a save with the console's memory card manager,
    /// then saving another game's two-block save over part of it.
    #[test]
    fn a_partly_overwritten_deleted_save_is_free_space() {
        let a = save("BASLUS-00001A", 3, 1);
        let img = put(&blank_card(), &[&a]);
        let mut img = Ps1Card.apply(&img, &[ApplyOp::Delete(a.unit_name())]).unwrap();
        // The BIOS reuses block 1 for a one-block save, leaving 2 and 3 marked deleted.
        let e = &mut img[FRAME..2 * FRAME];
        e.fill(0);
        e[0] = 0x51;
        e[4..8].copy_from_slice(&(BLOCK as u32).to_le_bytes());
        e[NEXT].copy_from_slice(&NO_NEXT.to_le_bytes());
        e[NAME][..13].copy_from_slice(b"BASLUS-00009Z");
        e[CHECKSUM] = checksum(e);
        assert_eq!(names(&img), ["BASLUS-00009Z"]);
        let big = save("BASLUS-00010BIG", 14, 4);
        let img = put(&img, &[&big]);
        assert_eq!(names(&img), ["BASLUS-00009Z", "BASLUS-00010BIG"]);
    }
}

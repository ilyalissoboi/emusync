//! `ps2.card`: PS2 memory card images (DESIGN §5.1). One unit per root entry: normally a save
//! directory such as `BASLUS-20062GTA3`.
//!
//! The canonical form (DESIGN §2.5) of a unit is its tree of names, modes, attributes and file
//! contents, with each directory's children sorted by name. Timestamps and on-card entry order
//! are kept in the `ps2.layout` meta field instead, so a game rewriting identical bytes, or
//! two devices storing the same save in a different order, doesn't create a new version.
//!
//! Unit names are the raw entry names decoded as Latin-1, which is lossless and is plain ASCII
//! for every real save.
//!
//! PCSX2 creates new card files unformatted (every byte `0xFF`) and leaves formatting to the
//! PS2 BIOS. Such an image is treated as an empty card, and the first apply formats it with the
//! standard layout, so a fresh card on a new device can receive saves from the cloud.

pub mod ecc;
pub mod mcfs;

use crate::{ApplyOp, Canonical, Confidence, ContainerFormat, FormatError, Result, UnitRef};
use mcfs::{Card, Node, NodeKind};
use std::collections::BTreeMap;

pub use mcfs::{blank_card, is_card_size};

/// Meta key holding entry order and timestamps.
pub const LAYOUT_META: &str = "ps2.layout";
const CANONICAL_MAGIC: &[u8; 8] = b"ES-PS2U1";
const KIND_FILE: u8 = 0;
const KIND_DIR: u8 = 1;

pub struct Ps2Card;

fn invalid(msg: impl Into<String>) -> FormatError {
    FormatError::Invalid(msg.into())
}

/// Latin-1 decoding: every byte becomes the code point of the same value.
pub fn name_to_string(name: &[u8]) -> String {
    name.iter().map(|&b| b as char).collect()
}

/// Inverse of [`name_to_string`].
pub fn name_to_bytes(name: &str) -> Result<Vec<u8>> {
    name.chars()
        .map(|c| u8::try_from(c as u32).map_err(|_| invalid(format!("unit name {name:?} is not Latin-1"))))
        .collect()
}

/// Title keys of a save directory, from its name: `BASLUS-20062GTA3` → `ps2:SLUS-20062`.
/// The first two letters are the region prefix (`BA` US, `BE` Europe, `BI` Japan, …).
pub fn titles_of(name: &str) -> Vec<String> {
    let b = name.as_bytes();
    if b.len() < 12 || b[0] != b'B' || !b[1].is_ascii_uppercase() {
        return vec![];
    }
    let serial = &b[2..12];
    let ok = serial[..4].iter().all(u8::is_ascii_uppercase)
        && serial[4] == b'-'
        && serial[5..].iter().all(u8::is_ascii_digit);
    if ok {
        vec![format!("ps2:{}", std::str::from_utf8(serial).unwrap())]
    } else {
        vec![]
    }
}

// ---- canonical form ----

fn encode_node(n: &Node, out: &mut Vec<u8>) {
    let kind = if matches!(n.kind, NodeKind::Dir(_)) { KIND_DIR } else { KIND_FILE };
    out.push(kind);
    out.push(n.name.len() as u8);
    out.extend_from_slice(&n.name);
    out.extend_from_slice(&n.mode.to_le_bytes());
    out.extend_from_slice(&n.attr.to_le_bytes());
    match &n.kind {
        NodeKind::File(data) => {
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(data);
        }
        NodeKind::Dir(children) => {
            out.extend_from_slice(&(children.len() as u32).to_le_bytes());
            let mut sorted: Vec<&Node> = children.iter().collect();
            sorted.sort_by(|a, b| a.name.cmp(&b.name));
            for c in sorted {
                encode_node(c, out);
            }
        }
    }
}

struct Decoder<'a> {
    b: &'a [u8],
    pos: usize,
}

impl Decoder<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.b.len()).ok_or_else(|| invalid("truncated save"))?;
        let s = &self.b[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn node(&mut self, depth: usize) -> Result<Node> {
        if depth > 8 {
            return Err(invalid("save nested too deeply"));
        }
        let kind = self.u8()?;
        let len = self.u8()? as usize;
        let name = self.take(len)?.to_vec();
        mcfs::check_name(&name)?;
        let mode = self.u16()?;
        let attr = self.u32()?;
        let count = self.u32()? as usize;
        let kind = match kind {
            KIND_FILE => {
                if mode & mcfs::MODE_FILE == 0 {
                    return Err(invalid("file entry without the file mode bit"));
                }
                NodeKind::File(self.take(count)?.to_vec())
            }
            KIND_DIR => {
                if mode & mcfs::MODE_DIR == 0 {
                    return Err(invalid("directory entry without the directory mode bit"));
                }
                // Each child takes at least 12 bytes, which bounds `count` by the input size.
                if count > (self.b.len() - self.pos) / 12 {
                    return Err(invalid("truncated save"));
                }
                let mut children: Vec<Node> = Vec::with_capacity(count);
                for _ in 0..count {
                    let c = self.node(depth + 1)?;
                    if children.last().is_some_and(|p| p.name >= c.name) {
                        return Err(invalid("directory entries are not sorted or not unique"));
                    }
                    children.push(c);
                }
                NodeKind::Dir(children)
            }
            other => return Err(invalid(format!("unknown entry kind {other}"))),
        };
        Ok(Node { name, mode, attr, created: [0; 8], modified: [0; 8], kind })
    }
}

/// Canonical bytes of a unit.
pub fn encode(node: &Node) -> Vec<u8> {
    let mut out = CANONICAL_MAGIC.to_vec();
    encode_node(node, &mut out);
    out
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

/// `ps2.layout`: one line per entry in on-card (pre-)order: hex path segments joined by `/`
/// (empty for the unit itself), then the created and modified timestamps in hex.
pub fn layout(node: &Node) -> String {
    fn walk(n: &Node, path: &str, out: &mut String) {
        out.push_str(&format!("{path} {} {}\n", hex(&n.created), hex(&n.modified)));
        if let NodeKind::Dir(children) = &n.kind {
            for c in children {
                let p = if path.is_empty() { hex(&c.name) } else { format!("{path}/{}", hex(&c.name)) };
                walk(c, &p, out);
            }
        }
    }
    let mut out = String::new();
    walk(node, "", &mut out);
    out
}

/// Decodes a canonical unit, restoring order and timestamps from `ps2.layout` where present.
/// Entries the layout doesn't mention keep sorted order and get `fallback_time`.
pub fn decode(c: &Canonical, fallback_time: [u8; 8]) -> Result<Node> {
    let body = c.bytes.strip_prefix(CANONICAL_MAGIC.as_slice()).ok_or_else(|| invalid("not a PS2 save"))?;
    let mut d = Decoder { b: body, pos: 0 };
    let mut node = d.node(0)?;
    if d.pos != body.len() {
        return Err(invalid("trailing bytes after save"));
    }
    if node.name != name_to_bytes(&c.name)? {
        return Err(invalid(format!("save is named {:?}, not {:?}", name_to_string(&node.name), c.name)));
    }
    let mut lines: Vec<(String, [u8; 8], [u8; 8])> = Vec::new();
    for line in c.meta.get(LAYOUT_META).map(String::as_str).unwrap_or_default().lines() {
        let mut parts = line.split(' ');
        let (Some(path), Some(cr), Some(md), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let ts = |s: &str| unhex(s).and_then(|v| <[u8; 8]>::try_from(v).ok());
        if let (Some(cr), Some(md)) = (ts(cr), ts(md)) {
            lines.push((path.to_string(), cr, md));
        }
    }
    fn restore(n: &mut Node, path: &str, lines: &[(String, [u8; 8], [u8; 8])], fallback: [u8; 8]) {
        let found = lines.iter().position(|(p, _, _)| p == path);
        (n.created, n.modified) = match found {
            Some(i) => (lines[i].1, lines[i].2),
            None => (fallback, fallback),
        };
        if let NodeKind::Dir(children) = &mut n.kind {
            let child_path = |c: &Node| if path.is_empty() { hex(&c.name) } else { format!("{path}/{}", hex(&c.name)) };
            let pos = |c: &Node| lines.iter().position(|(p, _, _)| *p == child_path(c)).unwrap_or(usize::MAX);
            children.sort_by_key(|c| pos(c));
            for c in children.iter_mut() {
                let p = child_path(c);
                restore(c, &p, lines, fallback);
            }
        }
    }
    restore(&mut node, "", &lines, fallback_time);
    Ok(node)
}

/// A standard-size image that was never formatted: every byte is `0xFF`.
pub fn is_unformatted(image: &[u8]) -> bool {
    is_card_size(image.len()) && image.iter().all(|&b| b == 0xFF)
}

/// Formats an unformatted image of the same size and ECC layout.
fn format_like(image: &[u8], time: [u8; 8]) -> Vec<u8> {
    let (clusters, with_ecc) = mcfs::CARD_CLUSTERS
        .iter()
        .find_map(|&c| {
            let pages = c as usize * 2;
            if image.len() == pages * (mcfs::PAGE + 16) {
                Some((c, true))
            } else if image.len() == pages * mcfs::PAGE {
                Some((c, false))
            } else {
                None
            }
        })
        .expect("is_unformatted checked the size");
    blank_card(clusters, with_ecc, time)
}

fn canonical(node: &Node) -> Canonical {
    let mut meta = BTreeMap::new();
    meta.insert(LAYOUT_META.to_string(), layout(node));
    Canonical { name: name_to_string(&node.name), bytes: encode(node), meta }
}

impl ContainerFormat for Ps2Card {
    fn id(&self) -> &'static str {
        "ps2.card"
    }

    fn supports_units(&self) -> bool {
        true
    }

    fn probe(&self, image: &[u8]) -> Confidence {
        if image.starts_with(b"Sony PS2 Memory Card Format ") {
            Confidence::Certain
        } else if is_card_size(image.len()) {
            Confidence::Maybe // the right size, but not formatted (yet)
        } else {
            Confidence::No
        }
    }

    fn validate(&self, image: &[u8]) -> Result<()> {
        if is_unformatted(image) {
            return Ok(());
        }
        Card::parse(image).map(|_| ())
    }

    fn list(&self, image: &[u8]) -> Result<Vec<UnitRef>> {
        if is_unformatted(image) {
            return Ok(vec![]);
        }
        Ok(Card::parse(image)?.entries.iter().map(|n| UnitRef { name: name_to_string(&n.name) }).collect())
    }

    fn extract(&self, image: &[u8], unit: &UnitRef) -> Result<Canonical> {
        if is_unformatted(image) {
            return Err(FormatError::NotFound(unit.name.clone()));
        }
        let card = Card::parse(image)?;
        let want = name_to_bytes(&unit.name)?;
        let node =
            card.entries.iter().find(|n| n.name == want).ok_or_else(|| FormatError::NotFound(unit.name.clone()))?;
        Ok(canonical(node))
    }

    fn extract_all(&self, image: &[u8]) -> Result<Vec<Canonical>> {
        if is_unformatted(image) {
            return Ok(vec![]);
        }
        Ok(Card::parse(image)?.entries.iter().map(canonical).collect())
    }

    fn titles(&self, unit_name: &str) -> Vec<String> {
        titles_of(unit_name)
    }

    /// Replaces or adds (at the end) each `Put` unit and removes each `Delete` unit, keeping
    /// the other root entries in their order, then rebuilds the card. An unformatted card is
    /// formatted first, stamped with the first incoming save's creation time.
    fn apply(&self, image: &[u8], ops: &[ApplyOp]) -> Result<Vec<u8>> {
        let formatted;
        let image = if is_unformatted(image) {
            let time = ops
                .iter()
                .find_map(|op| match op {
                    ApplyOp::Put(c) => decode(c, [0; 8]).ok().map(|n| n.created),
                    ApplyOp::Delete(_) => None,
                })
                .unwrap_or_default();
            formatted = format_like(image, time);
            &formatted
        } else {
            image
        };
        let card = Card::parse(image)?;
        let mut entries = card.entries.clone();
        // Entries without a recorded time get the card's own (root) modification time.
        let fallback = card.root_time();
        for op in ops {
            match op {
                ApplyOp::Put(c) => {
                    let node = decode(c, fallback)?;
                    match entries.iter_mut().find(|n| n.name == node.name) {
                        Some(slot) => *slot = node,
                        None => entries.push(node),
                    }
                }
                ApplyOp::Delete(name) => {
                    let want = name_to_bytes(name)?;
                    entries.retain(|n| n.name != want);
                }
            }
        }
        card.build(image, &entries)
    }
}

#[cfg(test)]
mod tests {
    use super::mcfs::{MODE_DIR_DEFAULT, MODE_FILE_DEFAULT};
    use super::*;

    const T0: [u8; 8] = [0, 1, 2, 3, 4, 5, 0xD2, 0x07];
    const T1: [u8; 8] = [0, 9, 9, 9, 9, 9, 0xEA, 0x07];

    fn file(name: &str, data: &[u8], t: [u8; 8]) -> Node {
        Node {
            name: name.into(),
            mode: MODE_FILE_DEFAULT,
            attr: 0,
            created: t,
            modified: t,
            kind: NodeKind::File(data.to_vec()),
        }
    }

    fn save(name: &str, files: Vec<Node>, t: [u8; 8]) -> Node {
        Node { name: name.into(), mode: MODE_DIR_DEFAULT, attr: 0, created: t, modified: t, kind: NodeKind::Dir(files) }
    }

    fn gta(t: [u8; 8]) -> Node {
        save("BASLUS-20062GTA3", vec![file("icon.sys", &[1; 964], t), file("GTA3SAVE", &[2; 4000], t)], t)
    }

    #[test]
    fn titles() {
        assert_eq!(titles_of("BASLUS-20062GTA3"), ["ps2:SLUS-20062"]);
        assert_eq!(titles_of("BESCES-50001"), ["ps2:SCES-50001"]);
        assert!(titles_of("BADATA-SYSTEM").is_empty());
        assert!(titles_of("icon.sys").is_empty());
    }

    #[test]
    fn canonical_ignores_time_and_order() {
        let a = gta(T0);
        let mut b = gta(T1);
        if let NodeKind::Dir(c) = &mut b.kind {
            c.reverse();
        }
        assert_eq!(encode(&a), encode(&b));
        assert_ne!(layout(&a), layout(&b));
        // Decoding restores the order and the timestamps.
        let back = decode(&canonical(&b), T0).unwrap();
        assert_eq!(back, b);
    }

    #[test]
    fn decode_rejects_garbage() {
        let good = canonical(&gta(T0));
        for n in [0, 8, 12, good.bytes.len() - 1] {
            let c = Canonical { bytes: good.bytes[..n].to_vec(), ..good.clone() };
            assert!(decode(&c, T0).is_err(), "truncated to {n}");
        }
        let renamed = Canonical { name: "OTHER".into(), ..good.clone() };
        assert!(decode(&renamed, T0).is_err());
        let mut trailing = good.clone();
        trailing.bytes.push(0);
        assert!(decode(&trailing, T0).is_err());
    }

    #[test]
    fn adapter_round_trip() {
        let img = blank_card(8192, true, T0);
        assert_eq!(Ps2Card.probe(&img), Confidence::Certain);
        assert_eq!(Ps2Card.probe(&vec![0xFF; img.len()]), Confidence::Maybe);
        assert_eq!(Ps2Card.probe(b"garbage"), Confidence::No);

        let g = canonical(&gta(T0));
        let r = canonical(&save("BASCUS-97199RAC", vec![file("data", &[3; 100], T1)], T1));
        let img = Ps2Card.apply(&img, &[ApplyOp::Put(g.clone()), ApplyOp::Put(r.clone())]).unwrap();
        let names: Vec<String> = Ps2Card.list(&img).unwrap().into_iter().map(|u| u.name).collect();
        assert_eq!(names, ["BASLUS-20062GTA3", "BASCUS-97199RAC"]);
        assert_eq!(Ps2Card.extract_all(&img).unwrap(), vec![g.clone(), r.clone()]);

        // Replacing keeps the position; deleting removes only that save.
        let g2 = canonical(&save("BASLUS-20062GTA3", vec![file("GTA3SAVE", &[9; 10], T1)], T1));
        let img2 = Ps2Card.apply(&img, &[ApplyOp::Put(g2.clone())]).unwrap();
        assert_eq!(Ps2Card.extract_all(&img2).unwrap(), vec![g2, r.clone()]);
        let img3 = Ps2Card.apply(&img2, &[ApplyOp::Delete("BASLUS-20062GTA3".into())]).unwrap();
        assert_eq!(Ps2Card.extract_all(&img3).unwrap(), vec![r]);
    }

    #[test]
    fn unformatted_cards() {
        for ecc in [true, false] {
            let blank = vec![0xFF; blank_card(8192, ecc, T0).len()];
            assert!(Ps2Card.validate(&blank).is_ok());
            assert!(Ps2Card.extract_all(&blank).unwrap().is_empty());
            let g = canonical(&gta(T1));
            let img = Ps2Card.apply(&blank, &[ApplyOp::Put(g.clone())]).unwrap();
            assert_eq!(img.len(), blank.len());
            assert_eq!(Ps2Card.extract_all(&img).unwrap(), vec![g]);
            // Formatted with the save's time, so every device produces the same image.
            assert_eq!(Card::parse(&img).unwrap().root_time(), T1);
        }
        // Wrong size, or anything written: not treated as a blank card.
        assert!(Ps2Card.validate(&vec![0xFF; 1000]).is_err());
        let mut touched = vec![0xFF; blank_card(8192, true, T0).len()];
        touched[5000] = 0;
        assert!(Ps2Card.validate(&touched).is_err());
    }
}

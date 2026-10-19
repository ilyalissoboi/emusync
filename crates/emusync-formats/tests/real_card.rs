//! Checks the card writer against a card the PS2 BIOS wrote. Set `EMUSYNC_REAL_PS2_CARD` to an
//! 8 MB `.ps2` file that PCSX2, running a real BIOS, formatted and saved to; the test is skipped
//! otherwise. The file is only read.
//!
//! The card's saves are rebuilt by emusync onto a freshly formatted card, then every byte the
//! BIOS controls is compared: the superblock, each save's directory entries (cluster numbers
//! aside, since emusync lays saves out contiguously) and every data cluster, padding included.

use emusync_formats::ps2::blank_card;
use emusync_formats::ps2::mcfs::Card;

struct Raw<'a> {
    img: &'a [u8],
    raw_page: usize,
    alloc_offset: u32,
    fat: Vec<u32>,
}

fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}

impl<'a> Raw<'a> {
    fn new(img: &'a [u8], ecc: bool) -> Self {
        let raw_page = if ecc { 528 } else { 512 };
        let mut r = Raw { img, raw_page, alloc_offset: u32_at(img, 0x34), fat: Vec::new() };
        let alloc_end = u32_at(img, 0x38) as usize;
        let ifc = r.cluster(u32_at(img, 0x50));
        for k in 0..alloc_end.div_ceil(256) {
            let fc = r.cluster(u32_at(&ifc, k * 4));
            r.fat.extend((0..256).map(|j| u32_at(&fc, j * 4)));
        }
        r
    }

    fn cluster(&self, abs: u32) -> Vec<u8> {
        (0..2)
            .flat_map(|p| {
                let off = (abs as usize * 2 + p) * self.raw_page;
                self.img[off..off + 512].to_vec()
            })
            .collect()
    }

    /// Full clusters of the chain starting at `first` (relative).
    fn chain(&self, first: u32) -> Vec<u8> {
        let mut out = Vec::new();
        let mut c = first;
        loop {
            out.extend(self.cluster(self.alloc_offset + c));
            let next = self.fat[c as usize] & 0x7FFF_FFFF;
            if next == 0x7FFF_FFFF {
                return out;
            }
            c = next;
        }
    }

    fn entries(&self, first: u32, count: u32) -> Vec<Vec<u8>> {
        self.chain(first).chunks(512).take(count as usize).map(<[u8]>::to_vec).collect()
    }
}

/// A directory entry with its cluster number blanked.
fn without_cluster(e: &[u8]) -> Vec<u8> {
    let mut e = e.to_vec();
    e[0x10..0x14].fill(0);
    e
}

fn name(e: &[u8]) -> String {
    String::from_utf8_lossy(e[0x40..0x60].split(|&b| b == 0).next().unwrap()).into_owned()
}

#[test]
fn rebuilt_saves_match_what_the_bios_wrote() {
    let Ok(path) = std::env::var("EMUSYNC_REAL_PS2_CARD") else {
        eprintln!("skipped: EMUSYNC_REAL_PS2_CARD not set");
        return;
    };
    let real = std::fs::read(&path).unwrap();
    let card = Card::parse(&real).unwrap();
    assert!(!card.entries.is_empty(), "{path} holds no saves");
    let blank = blank_card(8192, card.has_ecc(), card.root_created());
    let ours = Card::parse(&blank).unwrap().build(&blank, &card.entries).unwrap();

    assert_eq!(hex(&real[..0x184]), hex(&ours[..0x184]), "superblock");

    let (r, o) = (Raw::new(&real, card.has_ecc()), Raw::new(&ours, card.has_ecc()));
    let root_len = |raw: &Raw| u32_at(&raw.cluster(raw.alloc_offset), 4);
    let (r_root, o_root) = (r.entries(0, root_len(&r)), o.entries(0, root_len(&o)));
    // Only saves that exist count: the BIOS can leave deleted entries in a directory.
    let live = |es: &[Vec<u8>]| -> Vec<Vec<u8>> {
        es.iter().skip(2).filter(|e| u16::from_le_bytes([e[0], e[1]]) & 0x8000 != 0).cloned().collect()
    };
    let (r_saves, o_saves) = (live(&r_root), live(&o_root));
    assert_eq!(r_saves.len(), o_saves.len());
    let mut checked = 0;
    for (re, oe) in r_saves.iter().zip(&o_saves) {
        let save = name(re);
        assert_eq!(without_cluster(re), without_cluster(oe), "root entry of {save}");
        if u16::from_le_bytes([re[0], re[1]]) & 0x20 == 0 {
            continue;
        }
        let rd = r.entries(u32_at(re, 0x10), u32_at(re, 4));
        let od = o.entries(u32_at(oe, 0x10), u32_at(oe, 4));
        for (i, (rc, oc)) in rd.iter().zip(&od).enumerate() {
            assert_eq!(hex(&without_cluster(rc)), hex(&without_cluster(oc)), "{save} entry {i} ({})", name(rc));
            let mode = u16::from_le_bytes([rc[0], rc[1]]);
            let len = u32_at(rc, 4);
            if i >= 2 && mode & 0x10 != 0 && len > 0 {
                let (rdata, odata) = (r.chain(u32_at(rc, 0x10)), o.chain(u32_at(oc, 0x10)));
                let used = (len as usize).div_ceil(1024) * 1024;
                assert_eq!(rdata[..used], odata[..used], "{save}/{}: data and padding", name(rc));
                checked += 1;
            }
        }
    }
    eprintln!("{} saves and {checked} files match what the BIOS wrote", r_saves.len());
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

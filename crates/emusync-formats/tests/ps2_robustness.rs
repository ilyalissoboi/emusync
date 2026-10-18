//! Randomised tests for the PS2 card adapter: model-checked apply sequences, and corrupted
//! inputs that must be rejected without panicking (DESIGN §13). The fuzz target in
//! `fuzz/` explores the same parsers without a time budget.

use emusync_formats::ps2::mcfs::{Node, NodeKind, MODE_DIR_DEFAULT, MODE_FILE_DEFAULT};
use emusync_formats::ps2::{blank_card, encode, layout, name_to_string, LAYOUT_META};
use emusync_formats::{ApplyOp, Canonical, ContainerFormat, FormatError, Ps2Card};
use std::collections::BTreeMap;

/// xorshift64*: small, deterministic, good enough for test inputs.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

fn time(rng: &mut Rng) -> [u8; 8] {
    [
        0,
        rng.below(60) as u8,
        rng.below(60) as u8,
        rng.below(24) as u8,
        1 + rng.below(28) as u8,
        1 + rng.below(12) as u8,
        0xE0,
        0x07,
    ]
}

fn random_save(rng: &mut Rng, name: &str) -> Node {
    let n_files = rng.below(5) as usize;
    let mut files: Vec<Node> = Vec::new();
    for i in 0..n_files {
        let size = match rng.below(4) {
            0 => 0,
            1 => rng.below(1024) as usize,
            2 => 1024 * (1 + rng.below(3) as usize),
            _ => rng.below(40_000) as usize,
        };
        let t = time(rng);
        files.push(Node {
            name: format!("f{i}_{}", rng.below(1000)).into_bytes(),
            mode: MODE_FILE_DEFAULT,
            attr: 0,
            created: t,
            modified: t,
            kind: NodeKind::File(rng.bytes(size)),
        });
    }
    if rng.below(5) == 0 {
        let t = time(rng);
        files.push(Node {
            name: b"subdir".to_vec(),
            mode: MODE_DIR_DEFAULT,
            attr: 0,
            created: t,
            modified: t,
            kind: NodeKind::Dir(vec![Node {
                name: b"inner".to_vec(),
                mode: MODE_FILE_DEFAULT,
                attr: 0,
                created: t,
                modified: t,
                kind: NodeKind::File(rng.bytes(100)),
            }]),
        });
    }
    let t = time(rng);
    Node {
        name: name.as_bytes().to_vec(),
        mode: MODE_DIR_DEFAULT,
        attr: 0,
        created: t,
        modified: t,
        kind: NodeKind::Dir(files),
    }
}

fn canonical(n: &Node) -> Canonical {
    Canonical { name: name_to_string(&n.name), bytes: encode(n), meta: [(LAYOUT_META.to_string(), layout(n))].into() }
}

#[test]
fn apply_sequences_match_a_model() {
    const NAMES: [&str; 6] =
        ["BASLUS-20062GTA3", "BASCUS-97199RAC", "BESLES-50330", "BISLPM-65000", "BADATA-SYSTEM", "X"];
    for seed in 1..=12u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let mut img = blank_card(8192, seed % 2 == 0, [0; 8]);
        // Expected content, plus the order units first appeared in (replacements keep their slot).
        let mut model: BTreeMap<String, Canonical> = BTreeMap::new();
        let mut order: Vec<String> = Vec::new();
        for step in 0..15 {
            let mut ops = Vec::new();
            for _ in 0..1 + rng.below(3) {
                let name = NAMES[rng.below(NAMES.len() as u64) as usize];
                if rng.below(4) == 0 {
                    ops.push(ApplyOp::Delete(name.to_string()));
                } else {
                    ops.push(ApplyOp::Put(canonical(&random_save(&mut rng, name))));
                }
            }
            img = Ps2Card.apply(&img, &ops).unwrap_or_else(|e| panic!("seed {seed} step {step}: {e}"));
            for op in ops {
                match op {
                    ApplyOp::Put(c) => {
                        if !order.contains(&c.name) {
                            order.push(c.name.clone());
                        }
                        model.insert(c.name.clone(), c);
                    }
                    ApplyOp::Delete(n) => {
                        model.remove(&n);
                        order.retain(|x| *x != n);
                    }
                }
            }
            let got = Ps2Card.extract_all(&img).unwrap();
            let want: Vec<&Canonical> = order.iter().map(|n| &model[n]).collect();
            assert_eq!(got.iter().collect::<Vec<_>>(), want, "seed {seed} step {step}");
        }
    }
}

#[test]
fn corrupted_cards_never_panic() {
    let mut rng = Rng(0xC0FFEE);
    let base = blank_card(8192, false, [0; 8]);
    let saves: Vec<ApplyOp> =
        (0..4).map(|i| ApplyOp::Put(canonical(&random_save(&mut rng, &format!("BASLUS-2000{i}SAVE"))))).collect();
    let good = Ps2Card.apply(&base, &saves).unwrap();
    // The superblock, indirect FAT, FAT and the first data clusters live in the first ~64 KiB.
    let mut rejected = 0;
    for _ in 0..400 {
        let mut img = good.clone();
        for _ in 0..1 + rng.below(4) {
            let off = rng.below(64 * 1024) as usize;
            img[off] = rng.next() as u8;
        }
        match Ps2Card.extract_all(&img) {
            Ok(units) => {
                // Accepted: rebuilding must still work or fail cleanly.
                let ops: Vec<ApplyOp> = units.into_iter().map(ApplyOp::Put).collect();
                let _ = Ps2Card.apply(&img, &ops);
            }
            Err(FormatError::Invalid(_) | FormatError::Unsupported(_)) => rejected += 1,
            Err(e) => panic!("unexpected error {e}"),
        }
    }
    assert!(rejected > 0, "some corruptions must be detected");
}

#[test]
fn corrupted_saves_never_panic() {
    let mut rng = Rng(7);
    let img = blank_card(8192, false, [0; 8]);
    let save = canonical(&random_save(&mut rng, "BASLUS-20062GTA3"));
    for _ in 0..2000 {
        let mut c = save.clone();
        match rng.below(3) {
            0 => {
                let off = rng.below(c.bytes.len() as u64) as usize;
                c.bytes[off] = rng.next() as u8;
            }
            1 => c.bytes.truncate(rng.below(c.bytes.len() as u64) as usize),
            _ => {
                let mut layout = c.meta[LAYOUT_META].clone().into_bytes();
                if !layout.is_empty() {
                    let off = rng.below(layout.len() as u64) as usize;
                    layout[off] = b"0123456789abcdef /\nz"[rng.below(20) as usize];
                }
                c.meta.insert(LAYOUT_META.into(), String::from_utf8_lossy(&layout).into_owned());
            }
        }
        if let Ok(out) = Ps2Card.apply(&img, &[ApplyOp::Put(c.clone())]) {
            // Whatever was accepted must read back as exactly what was written.
            assert_eq!(Ps2Card.extract_all(&out).unwrap()[0].bytes, c.bytes);
        }
    }
}

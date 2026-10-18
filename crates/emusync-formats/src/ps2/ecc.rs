//! Hamming ECC of PS2 memory card pages (DESIGN §5.1).
//!
//! Each 512-byte page has a 16-byte spare area holding three ECC bytes for each 128-byte
//! chunk, followed by four zero bytes. This is the algorithm used by mymc and by PCSX2's
//! folder memory cards.

const fn parity(b: u8) -> u8 {
    (b.count_ones() & 1) as u8
}

const PARITY: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut b = 0;
    while b < 256 {
        t[b] = parity(b as u8);
        b += 1;
    }
    t
};

const COLUMN_PARITY_MASKS: [u8; 256] = {
    const MASKS: [u8; 7] = [0x55, 0x33, 0x0F, 0x00, 0xAA, 0xCC, 0xF0];
    let mut t = [0u8; 256];
    let mut b = 0;
    while b < 256 {
        let mut mask = 0u8;
        let mut i = 0;
        while i < MASKS.len() {
            mask |= parity(b as u8 & MASKS[i]) << i;
            i += 1;
        }
        t[b] = mask;
        b += 1;
    }
    t
};

/// ECC of one 128-byte chunk: column parity, then the two line parities.
pub fn chunk_ecc(chunk: &[u8]) -> [u8; 3] {
    debug_assert_eq!(chunk.len(), 128);
    let (mut column, mut line0, mut line1) = (0x77u8, 0x7Fu8, 0x7Fu8);
    for (i, &b) in chunk.iter().enumerate() {
        column ^= COLUMN_PARITY_MASKS[b as usize];
        if PARITY[b as usize] != 0 {
            line0 ^= !(i as u8);
            line1 ^= i as u8;
        }
    }
    [column, line0 & 0x7F, line1]
}

/// Spare area of a 512-byte page: four chunk ECCs, then zero padding.
pub fn page_spare(page: &[u8]) -> [u8; 16] {
    debug_assert_eq!(page.len(), 512);
    let mut spare = [0u8; 16];
    for (i, chunk) in page.chunks_exact(128).enumerate() {
        spare[i * 3..i * 3 + 3].copy_from_slice(&chunk_ecc(chunk));
    }
    spare
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_chunks() {
        // Bytes with even parity leave the line parities alone; 0x00 and 0xFF also have
        // even parity under every column mask.
        assert_eq!(chunk_ecc(&[0u8; 128]), [0x77, 0x7F, 0x7F]);
        assert_eq!(chunk_ecc(&[0xFFu8; 128]), [0x77, 0x7F, 0x7F]);
    }

    #[test]
    fn single_bit_is_located() {
        // A Hamming code pins down a single flipped bit: the line parities encode the byte
        // index, the column parity the bit index. Check that every position gives a distinct code.
        let base = chunk_ecc(&[0u8; 128]);
        let mut seen = std::collections::HashSet::new();
        for byte in 0..128 {
            for bit in 0..8 {
                let mut c = [0u8; 128];
                c[byte] = 1 << bit;
                let e = chunk_ecc(&c);
                let syndrome = [e[0] ^ base[0], e[1] ^ base[1], e[2] ^ base[2]];
                assert!(seen.insert(syndrome), "byte {byte} bit {bit} collides");
                assert_eq!(syndrome[2], byte as u8, "line parity 1 is the byte index");
                assert_eq!(syndrome[1], !(byte as u8) & 0x7F, "line parity 0 is its complement");
            }
        }
    }

    #[test]
    fn spare_layout() {
        let mut page = [0u8; 512];
        page[200] = 0x01;
        let spare = page_spare(&page);
        assert_eq!(&spare[0..3], &chunk_ecc(&page[0..128]));
        assert_eq!(&spare[3..6], &chunk_ecc(&page[128..256]));
        assert_eq!(&spare[12..], &[0, 0, 0, 0]);
    }
}

//! Chunking and blob encoding (DESIGN §7).
//!
//! Content up to 256 KiB is one chunk. Larger content (whole memory card images, later save
//! states) is split with FastCDC (16/64/256 KiB min/avg/max), so a changed card re-uploads only
//! the chunks around the change. Each chunk is stored as one immutable blob named by its hash.

use crate::error::{Error, Result};
use crate::model::content_id;

pub const MAX_CHUNK: usize = 256 * 1024;
const MIN_CHUNK: u32 = 16 * 1024;
const AVG_CHUNK: u32 = 64 * 1024;

const ENC_RAW: u8 = 0;
const ENC_ZSTD: u8 = 1;

/// Splits content into chunks.
pub fn split(data: &[u8]) -> Vec<&[u8]> {
    if data.len() <= MAX_CHUNK {
        return vec![data];
    }
    fastcdc::v2020::FastCDC::new(data, MIN_CHUNK, AVG_CHUNK, MAX_CHUNK as u32)
        .map(|c| &data[c.offset..c.offset + c.length])
        .collect()
}

/// Remote key of a blob: `blobs/<hh>/<hex>`.
///
/// Ids from remote logs are validated by [`crate::model::is_hash_id`] on ingest; a malformed id
/// still yields a key (one that won't exist) rather than a panic.
pub fn blob_key(id: &str) -> String {
    let hex = id.strip_prefix("b3:").unwrap_or(id);
    format!("blobs/{}/{hex}", hex.get(..2).unwrap_or("00"))
}

/// Encodes a chunk for storage: one format byte, then the bytes, zstd-compressed (level 3)
/// unless that saves less than 5 %.
pub fn encode_blob(data: &[u8]) -> Vec<u8> {
    if let Ok(z) = zstd::bulk::compress(data, 3) {
        if z.len() < data.len() - data.len() / 20 {
            let mut out = Vec::with_capacity(z.len() + 1);
            out.push(ENC_ZSTD);
            out.extend_from_slice(&z);
            return out;
        }
    }
    let mut out = Vec::with_capacity(data.len() + 1);
    out.push(ENC_RAW);
    out.extend_from_slice(data);
    out
}

/// Decodes a blob and verifies it hashes to `id`. A mismatch is treated as a missing blob.
pub fn decode_blob(blob: &[u8], id: &str) -> Result<Vec<u8>> {
    let data = match blob.split_first() {
        Some((&ENC_RAW, rest)) => rest.to_vec(),
        Some((&ENC_ZSTD, rest)) => zstd::bulk::decompress(rest, MAX_CHUNK)
            .map_err(|e| Error::Integrity(format!("blob {id}: cannot decompress: {e}")))?,
        _ => return Err(Error::Integrity(format!("blob {id}: unknown encoding"))),
    };
    let actual = content_id(&data);
    if actual != id {
        return Err(Error::Integrity(format!("blob {id}: contents hash to {actual}")));
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pseudo_random(n: usize, seed: u64) -> Vec<u8> {
        let mut x = seed;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    #[test]
    fn small_content_is_one_chunk() {
        assert_eq!(split(b"abc").len(), 1);
        assert_eq!(split(b"").len(), 1);
    }

    #[test]
    fn local_edit_changes_few_chunks() {
        let a = pseudo_random(8 * 1024 * 1024, 1);
        let mut b = a.clone();
        b[4_000_000..4_000_100].copy_from_slice(&[0xAA; 100]);
        let ca: Vec<String> = split(&a).iter().map(|c| content_id(c)).collect();
        let cb: Vec<String> = split(&b).iter().map(|c| content_id(c)).collect();
        assert!(ca.len() > 16);
        assert_eq!(split(&a).concat(), a);
        let changed = cb.iter().filter(|c| !ca.contains(c)).count();
        assert!(changed <= 2, "{changed} chunks changed");
        assert!(split(&a).iter().all(|c| c.len() <= MAX_CHUNK));
    }

    #[test]
    fn blob_round_trip_and_verification() {
        for data in [vec![0u8; 100_000], pseudo_random(100_000, 7), vec![]] {
            let id = content_id(&data);
            let enc = encode_blob(&data);
            assert_eq!(decode_blob(&enc, &id).unwrap(), data);
        }
        let enc = encode_blob(b"hello");
        assert!(decode_blob(&enc, &content_id(b"other")).is_err());
        assert!(decode_blob(&[9, 1, 2], &content_id(b"")).is_err());
        assert_eq!(blob_key("b3:abcdef"), "blobs/ab/abcdef");
    }
}

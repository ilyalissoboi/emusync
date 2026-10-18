//! Repository data model and log records (DESIGN §4, §6.1).

use crate::hlc::Hlc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Sync mode of a logical card (DESIGN §2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CardMode {
    Units,
    WholeFile,
}

impl CardMode {
    pub fn as_str(self) -> &'static str {
        match self {
            CardMode::Units => "units",
            CardMode::WholeFile => "whole-file",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "units" => Some(CardMode::Units),
            "whole-file" => Some(CardMode::WholeFile),
            _ => None,
        }
    }
}

/// `card` log record: publishes a logical card and its mode. Highest HLC wins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardRecord {
    pub card: String,
    pub mode: CardMode,
    pub hlc: Hlc,
}

/// `version` log record: one node of a unit's version DAG.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionRecord {
    pub unit: String,
    /// `b3:` + BLAKE3 of the canonical JSON of every other field (see [`VersionRecord::compute_id`]).
    pub id: String,
    pub parents: Vec<String>,
    pub content: String,
    pub chunks: Vec<String>,
    pub size: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub meta: BTreeMap<String, String>,
    pub hlc: Hlc,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub playtime_ms: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub titles: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tombstone: bool,
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

impl VersionRecord {
    /// Hash of the record's header: canonical JSON (sorted keys, no whitespace) of every field
    /// except `id`. Fields added by later versions of the format are covered too, because the
    /// hash is computed over the JSON object as received.
    pub fn compute_id(value: &Value) -> String {
        let mut v = value.clone();
        if let Value::Object(map) = &mut v {
            map.remove("id");
        }
        let mut out = String::new();
        canonical_json(&v, &mut out);
        format!("b3:{}", blake3::hash(out.as_bytes()).to_hex())
    }

    /// Fills in `id` from the other fields.
    pub fn seal(mut self) -> Self {
        self.id = String::new();
        let value = serde_json::to_value(&self).expect("version record serializes");
        self.id = Self::compute_id(&value);
        self
    }
}

/// A record in a device log segment. Unknown ops (`state`, `lease`, … from later versions) are
/// skipped by [`parse_record`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum LogRecord {
    Card(CardRecord),
    Version(VersionRecord),
}

impl LogRecord {
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).expect("log record serializes")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    #[error("malformed record: {0}")]
    Malformed(String),
    #[error("version id mismatch: record says {claimed}, contents hash to {actual}")]
    BadId { claimed: String, actual: String },
}

/// Parses one log line. `Ok(None)` for ops this version doesn't know.
pub fn parse_record(line: &[u8]) -> Result<Option<LogRecord>, RecordError> {
    let value: Value = serde_json::from_slice(line).map_err(|e| RecordError::Malformed(e.to_string()))?;
    let op = value.get("op").and_then(Value::as_str).unwrap_or_default().to_string();
    match op.as_str() {
        "card" | "version" => {}
        _ => return Ok(None),
    }
    if op == "version" {
        let mut header = value.clone();
        if let Value::Object(m) = &mut header {
            m.remove("op");
        }
        let actual = VersionRecord::compute_id(&header);
        let claimed = value.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        if actual != claimed {
            return Err(RecordError::BadId { claimed, actual });
        }
    }
    let rec: LogRecord = serde_json::from_value(value).map_err(|e| RecordError::Malformed(e.to_string()))?;
    if let LogRecord::Version(v) = &rec {
        // Ids become remote keys and local file names; anything but a canonical hash is corrupt.
        let mut ids = std::iter::once(&v.content).chain(&v.chunks).chain(&v.parents);
        if let Some(bad) = ids.find(|id| !is_hash_id(id)) {
            return Err(RecordError::Malformed(format!("invalid hash id {bad:?}")));
        }
    }
    Ok(Some(rec))
}

/// Whether `id` is a canonical content address: `b3:` + 64 lowercase hex digits.
pub fn is_hash_id(id: &str) -> bool {
    id.strip_prefix("b3:")
        .is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
}

/// JSON with object keys sorted and no insignificant whitespace.
fn canonical_json(v: &Value, out: &mut String) {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(k).unwrap());
                out.push(':');
                canonical_json(&map[k], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical_json(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&serde_json::to_string(other).unwrap()),
    }
}

/// `b3:<hex>` content address of a byte string.
pub fn content_id(data: &[u8]) -> String {
    format!("b3:{}", blake3::hash(data).to_hex())
}

/// The unit id of a card in whole-file mode: `ps2:Mcd001:@card`.
pub fn whole_file_unit(card_id: &str) -> String {
    format!("{card_id}:{}", emusync_formats::WHOLE_FILE_UNIT)
}

/// Short form of a `b3:` id for display.
pub fn short_id(id: &str) -> &str {
    let hex = id.strip_prefix("b3:").unwrap_or(id);
    &hex[..hex.len().min(12)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> VersionRecord {
        VersionRecord {
            unit: "ps2:Mcd001:@card".into(),
            id: String::new(),
            parents: vec![],
            content: content_id(b"x"),
            chunks: vec![content_id(b"x")],
            size: 1,
            meta: BTreeMap::new(),
            hlc: Hlc { ms: 1, counter: 0 },
            playtime_ms: 0,
            titles: vec![],
            tombstone: false,
        }
        .seal()
    }

    #[test]
    fn version_ids_verify() {
        let v = sample();
        let line = LogRecord::Version(v.clone()).to_line();
        assert!(line.starts_with(r#"{"op":"version""#));
        assert_eq!(parse_record(line.as_bytes()).unwrap(), Some(LogRecord::Version(v)));
        let tampered = line.replace(r#""size":1"#, r#""size":2"#);
        assert!(matches!(parse_record(tampered.as_bytes()), Err(RecordError::BadId { .. })));
    }

    #[test]
    fn non_canonical_hash_ids_are_rejected() {
        let upper = format!("b3:{}", "A".repeat(64));
        for bad in ["b3:", "b3:\u{e9}", "b3:../../etc", upper.as_str()] {
            let mut v = sample();
            v.chunks = vec![bad.to_string()];
            let line = LogRecord::Version(v.seal()).to_line();
            assert!(matches!(parse_record(line.as_bytes()), Err(RecordError::Malformed(_))), "{bad}");
        }
        assert!(is_hash_id(&content_id(b"x")));
    }

    #[test]
    fn unknown_fields_and_ops() {
        // A future writer adds a field: the id still verifies because it covers the whole object.
        let v = sample();
        let mut value = serde_json::to_value(LogRecord::Version(v)).unwrap();
        value.as_object_mut().unwrap().insert("future".into(), Value::from(7));
        let mut header = value.clone();
        header.as_object_mut().unwrap().remove("op");
        value["id"] = Value::from(VersionRecord::compute_id(&header));
        assert!(parse_record(value.to_string().as_bytes()).unwrap().is_some());

        assert_eq!(parse_record(br#"{"op":"lease","title":"ps2:SLUS-20062"}"#).unwrap(), None);
        let card = parse_record(
            br#"{"op":"card","card":"ps2:Mcd001","mode":"whole-file","hlc":"2026-10-12T19:40:00.000Z/0000"}"#,
        )
        .unwrap()
        .unwrap();
        assert!(matches!(card, LogRecord::Card(CardRecord { mode: CardMode::WholeFile, .. })));
    }
}

//! Hybrid logical clock (DESIGN §4). Used for display and as a tie-break only; ordering
//! comes from the version DAG.
//!
//! Text form: `2026-10-18T11:02:07.120Z/0003` (UTC wall time in milliseconds, then a counter).

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hlc {
    pub ms: i64,
    pub counter: u32,
}

impl Hlc {
    pub fn wall_ms() -> i64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
    }

    /// Next timestamp for a local event, given the last one this device issued or observed.
    pub fn tick(last: Hlc, wall_ms: i64) -> Hlc {
        if wall_ms > last.ms {
            Hlc { ms: wall_ms, counter: 0 }
        } else {
            Hlc { ms: last.ms, counter: last.counter + 1 }
        }
    }

    /// The timestamp immediately after `self`.
    pub fn successor(self) -> Hlc {
        Hlc { ms: self.ms, counter: self.counter + 1 }
    }

    pub fn parse(s: &str) -> Option<Hlc> {
        let (ts, counter) = s.split_once('/')?;
        let counter = counter.parse().ok()?;
        // YYYY-MM-DDTHH:MM:SS.mmmZ
        let b = ts.as_bytes();
        if b.len() != 24 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[19] != b'.' || b[23] != b'Z' {
            return None;
        }
        let n = |r: std::ops::Range<usize>| ts.get(r)?.parse::<i64>().ok();
        let (y, mo, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
        let (h, mi, s, ms) = (n(11..13)?, n(14..16)?, n(17..19)?, n(20..23)?);
        if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 60 {
            return None;
        }
        let days = days_from_civil(y, mo as u32, d as u32);
        Some(Hlc { ms: ((days * 24 + h) * 60 + mi) * 60_000 + s * 1000 + ms, counter })
    }

    /// Wall-clock part as `YYYY-MM-DD HH:MM:SS` UTC, for display.
    pub fn display_time(&self) -> String {
        let s = self.to_string();
        format!("{} {}", &s[0..10], &s[11..19])
    }
}

impl fmt::Display for Hlc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let days = self.ms.div_euclid(86_400_000);
        let rem = self.ms.rem_euclid(86_400_000);
        let (y, m, d) = civil_from_days(days);
        let (h, mi, s, ms) = (rem / 3_600_000, rem / 60_000 % 60, rem / 1000 % 60, rem % 1000);
        write!(f, "{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{ms:03}Z/{:04}", self.counter)
    }
}

impl Serialize for Hlc {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Hlc {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Hlc::parse(&s).ok_or_else(|| serde::de::Error::custom(format!("invalid HLC {s:?}")))
    }
}

// Howard Hinnant's civil calendar algorithms.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let h = Hlc::parse("2026-10-18T11:02:07.120Z/0003").unwrap();
        assert_eq!(h.counter, 3);
        assert_eq!(h.to_string(), "2026-10-18T11:02:07.120Z/0003");
        assert_eq!(Hlc { ms: 0, counter: 0 }.to_string(), "1970-01-01T00:00:00.000Z/0000");
        for ms in [0i64, 951_782_400_000, 1_709_164_800_123, 4_102_444_800_000] {
            let h = Hlc { ms, counter: 12_345 };
            assert_eq!(Hlc::parse(&h.to_string()), Some(h));
        }
        assert!(Hlc::parse("2026-13-18T11:02:07.120Z/0003").is_none());
        assert!(Hlc::parse("garbage").is_none());
    }

    #[test]
    fn monotonic() {
        let a = Hlc { ms: 1000, counter: 5 };
        assert_eq!(Hlc::tick(a, 900), Hlc { ms: 1000, counter: 6 });
        assert_eq!(Hlc::tick(a, 2000), Hlc { ms: 2000, counter: 0 });
        assert!(Hlc::tick(a, 900) > a);
    }
}

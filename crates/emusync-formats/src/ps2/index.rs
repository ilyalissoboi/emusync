//! PCSX2's `_pcsx2_index` files: a YAML mapping from host file name to `order`, `timeCreated`
//! and `timeModified` (Unix seconds), plus `$ROOT` (`%ROOT` in old versions) for the
//! directory's own timestamps.
//!
//! PCSX2 writes them with rapidyaml, normally in block style; an index created by
//! `FlushFileEntries` starts out in flow style (`{$ROOT: {timeCreated: 0, timeModified: 0}}`).
//! This module reads the subset of YAML those two styles produce: nested mappings with plain,
//! single- or double-quoted keys and scalar values. Anything else is an error, and the caller
//! then treats the index as missing, as PCSX2 does with an index it can't parse.

use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IndexEntry {
    pub order: Option<i64>,
    pub created: Option<i64>,
    pub modified: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Index {
    /// The directory's own timestamps (`$ROOT`).
    pub root: IndexEntry,
    /// Per host file name.
    pub entries: BTreeMap<String, IndexEntry>,
}

#[derive(Debug, Clone, PartialEq)]
enum Value {
    Scalar(String),
    Map(Vec<(String, Value)>),
}

type ParseResult<T> = std::result::Result<T, ()>;

/// Parses an index. `None` means PCSX2 couldn't use it either.
pub fn parse(text: &str) -> Option<Index> {
    let Ok(Value::Map(items)) = parse_document(text) else { return None };
    let mut index = Index::default();
    for (key, v) in items {
        // PCSX2 repairs keys that an old rapidyaml bug saved with trailing NULs.
        let key = key.trim_end_matches('\0').to_string();
        let Value::Map(fields) = v else { continue };
        let mut e = IndexEntry::default();
        for (k, v) in fields {
            let Value::Scalar(s) = v else { continue };
            let n = s.trim().parse::<i64>().ok();
            match k.as_str() {
                "order" => e.order = n,
                "timeCreated" => e.created = n,
                "timeModified" => e.modified = n,
                _ => {}
            }
        }
        if key == "$ROOT" || key == "%ROOT" {
            index.root = e;
        } else {
            index.entries.insert(key, e);
        }
    }
    Some(index)
}

fn parse_document(text: &str) -> ParseResult<Value> {
    let lines: Vec<(usize, &str)> = text
        .lines()
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with('#') && t != "---" && t != "..."
        })
        .map(|l| (l.len() - l.trim_start_matches(' ').len(), l.trim_start_matches(' ')))
        .collect();
    if lines.is_empty() {
        return Ok(Value::Map(vec![]));
    }
    if lines[0].1.starts_with('{') {
        let joined: String = lines.iter().map(|(_, l)| *l).collect::<Vec<_>>().join(" ");
        let mut p = Flow { s: joined.as_bytes(), pos: 0 };
        let v = p.value()?;
        p.ws();
        return if p.pos == p.s.len() { Ok(v) } else { Err(()) };
    }
    let mut i = 0;
    let v = block(&lines, &mut i, lines[0].0)?;
    if i != lines.len() {
        return Err(());
    }
    Ok(v)
}

/// A block mapping whose keys are at `indent`.
fn block(lines: &[(usize, &str)], i: &mut usize, indent: usize) -> ParseResult<Value> {
    let mut items = Vec::new();
    while *i < lines.len() && lines[*i].0 == indent {
        let (key, rest) = split_key(lines[*i].1)?;
        *i += 1;
        let rest = rest.trim();
        let value = if rest.is_empty() {
            match lines.get(*i) {
                Some(&(next, _)) if next > indent => block(lines, i, next)?,
                _ => Value::Scalar(String::new()),
            }
        } else if rest.starts_with('{') {
            let mut p = Flow { s: rest.as_bytes(), pos: 0 };
            let v = p.value()?;
            p.ws();
            if p.pos != p.s.len() {
                return Err(());
            }
            v
        } else {
            Value::Scalar(unquote(rest)?)
        };
        items.push((key, value));
    }
    if *i < lines.len() && lines[*i].0 > indent {
        return Err(());
    }
    Ok(Value::Map(items))
}

/// Splits `key: rest` (block style).
fn split_key(line: &str) -> ParseResult<(String, &str)> {
    let b = line.as_bytes();
    if b.first() == Some(&b'\'') || b.first() == Some(&b'"') {
        let (key, used) = quoted(line)?;
        let rest = line[used..].trim_start().strip_prefix(':').ok_or(())?;
        return Ok((key, rest));
    }
    let colon = (0..b.len()).find(|&j| b[j] == b':' && (j + 1 == b.len() || b[j + 1] == b' ')).ok_or(())?;
    Ok((line[..colon].trim_end().to_string(), &line[colon + 1..]))
}

/// A quoted scalar at the start of `s`: its value and the bytes it used.
fn quoted(s: &str) -> ParseResult<(String, usize)> {
    let q = s.chars().next().ok_or(())?;
    let mut out = String::new();
    let mut chars = s.char_indices().skip(1).peekable();
    while let Some((j, c)) = chars.next() {
        if c == q {
            if q == '\'' && chars.peek().map(|&(_, c)| c) == Some('\'') {
                chars.next();
                out.push('\'');
                continue;
            }
            return Ok((out, j + 1));
        }
        if q == '"' && c == '\\' {
            let (_, e) = chars.next().ok_or(())?;
            out.push(match e {
                'n' => '\n',
                't' => '\t',
                '0' => '\0',
                other => other,
            });
            continue;
        }
        out.push(c);
    }
    Err(())
}

fn unquote(s: &str) -> ParseResult<String> {
    if s.starts_with('\'') || s.starts_with('"') {
        let (v, used) = quoted(s)?;
        if s[used..].trim().is_empty() {
            return Ok(v);
        }
        return Err(());
    }
    Ok(s.to_string())
}

/// Flow-style mapping parser (`{a: 1, b: {c: 2}}`).
struct Flow<'a> {
    s: &'a [u8],
    pos: usize,
}

impl Flow<'_> {
    fn ws(&mut self) {
        while self.pos < self.s.len() && self.s[self.pos] == b' ' {
            self.pos += 1;
        }
    }

    fn value(&mut self) -> ParseResult<Value> {
        self.ws();
        match self.s.get(self.pos) {
            Some(b'{') => {
                self.pos += 1;
                let mut items = Vec::new();
                loop {
                    self.ws();
                    if self.s.get(self.pos) == Some(&b'}') {
                        self.pos += 1;
                        return Ok(Value::Map(items));
                    }
                    let key = self.scalar(true)?;
                    self.ws();
                    if self.s.get(self.pos) != Some(&b':') {
                        return Err(());
                    }
                    self.pos += 1;
                    let v = self.value()?;
                    items.push((key, v));
                    self.ws();
                    match self.s.get(self.pos) {
                        Some(b',') => self.pos += 1,
                        Some(b'}') => {}
                        _ => return Err(()),
                    }
                }
            }
            Some(_) => Ok(Value::Scalar(self.scalar(false)?)),
            None => Err(()),
        }
    }

    fn scalar(&mut self, key: bool) -> ParseResult<String> {
        let rest = std::str::from_utf8(&self.s[self.pos..]).map_err(|_| ())?;
        if rest.starts_with('\'') || rest.starts_with('"') {
            let (v, used) = quoted(rest)?;
            self.pos += used;
            return Ok(v);
        }
        let b = rest.as_bytes();
        let end = (0..b.len())
            .find(|&j| {
                b[j] == b','
                    || b[j] == b'}'
                    || (key && b[j] == b':' && (j + 1 == b.len() || b"  ,}".contains(&b[j + 1])))
            })
            .unwrap_or(b.len());
        self.pos += end;
        let v = rest[..end].trim();
        if v.is_empty() {
            return Err(());
        }
        Ok(v.to_string())
    }
}

/// Whether a key can be written without quotes.
fn plain_ok(key: &str) -> bool {
    let first = key.chars().next();
    !key.is_empty()
        && !first.is_some_and(|c| "-?:,[]{}#&*!|>'\"%@` ".contains(c))
        && !key.ends_with([' ', ':'])
        && !key.contains(": ")
        && !key.contains(" #")
        && !key.chars().any(|c| c.is_control() || "{}[],".contains(c))
}

fn key_text(key: &str) -> String {
    if plain_ok(key) {
        key.to_string()
    } else {
        format!("'{}'", key.replace('\'', "''"))
    }
}

/// Writes an index in block style, entries in the given order.
pub fn write(root: (i64, i64), entries: &[(String, i64, i64, i64)]) -> String {
    let mut out = format!("$ROOT:\n  timeCreated: {}\n  timeModified: {}\n", root.0, root.1);
    for (name, order, created, modified) in entries {
        out.push_str(&format!(
            "{}:\n  order: {order}\n  timeCreated: {created}\n  timeModified: {modified}\n",
            key_text(name)
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_style() {
        let text = "$ROOT:\n  timeCreated: 1700000000\n  timeModified: 1700000100\nicon.sys:\n  order: 1\n  \
                    timeCreated: 10\n  timeModified: 20\n'it''s: odd':\n  order: 2\n  timeModified: 30\n";
        let ix = parse(text).unwrap();
        assert_eq!(ix.root, IndexEntry { order: None, created: Some(1700000000), modified: Some(1700000100) });
        assert_eq!(ix.entries["icon.sys"], IndexEntry { order: Some(1), created: Some(10), modified: Some(20) });
        assert_eq!(ix.entries["it's: odd"].modified, Some(30));
    }

    #[test]
    fn flow_style_and_legacy_root() {
        let ix = parse("{$ROOT: {timeCreated: 5, timeModified: 6}}\n").unwrap();
        assert_eq!(ix.root.modified, Some(6));
        let ix = parse("{%ROOT: {timeCreated: 1, timeModified: 2}, a: {order: 3}}").unwrap();
        assert_eq!(ix.root.created, Some(1));
        assert_eq!(ix.entries["a"].order, Some(3));
        let ix = parse("a:\n  order: 1\nb: {order: 2, timeCreated: 3}\n").unwrap();
        assert_eq!(ix.entries["b"].created, Some(3));
        // Keys an old rapidyaml saved with trailing NULs.
        let ix = parse("\"data\\0\\0\":\n  order: 4\n").unwrap();
        assert_eq!(ix.entries["data"].order, Some(4));
    }

    #[test]
    fn write_then_parse() {
        let names = ["icon.sys", "-dash", "#hash", "a: b", "it's", "BASLUS-20062GTA3", "with space", "[x]"];
        let entries: Vec<(String, i64, i64, i64)> =
            names.iter().enumerate().map(|(i, n)| (n.to_string(), i as i64 + 1, -5, 7)).collect();
        let ix = parse(&write((1, 2), &entries)).unwrap();
        assert_eq!(ix.root, IndexEntry { order: None, created: Some(1), modified: Some(2) });
        for (i, n) in names.iter().enumerate() {
            assert_eq!(ix.entries[*n], IndexEntry { order: Some(i as i64 + 1), created: Some(-5), modified: Some(7) });
        }
    }

    #[test]
    fn rejects_garbage_without_panicking() {
        for bad in ["{", "{a: {b: 1}", "a:\n    b: 1\n  c: 2\n", "'open", "{a 1}", "a: 'x' y", "- a\n- b"] {
            assert!(parse(bad).is_none(), "{bad:?}");
        }
        assert_eq!(parse("").unwrap(), Index::default());
        // Every prefix of a valid index either parses or fails cleanly.
        let text = write((1, 2), &[("a".into(), 1, 2, 3), ("'q'".into(), 2, 3, 4)]);
        for n in 0..text.len() {
            let _ = parse(&text[..n]);
        }
    }
}

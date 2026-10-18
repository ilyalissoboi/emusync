//! Local state DB (DESIGN §4.1): SQLite in `data_dir/state.db`.

use crate::error::Result;
use crate::hlc::Hlc;
use crate::model::{CardMode, VersionRecord};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS devices (id TEXT PRIMARY KEY, name TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS cards (card_id TEXT PRIMARY KEY, mode TEXT NOT NULL, hlc TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS containers (
    id INTEGER PRIMARY KEY,
    location TEXT NOT NULL UNIQUE,
    format TEXT NOT NULL,
    card_id TEXT NOT NULL,
    title_scope TEXT,
    flags INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS local_units (
    unit_id TEXT NOT NULL,
    container_id INTEGER NOT NULL,
    content_hash TEXT NOT NULL,
    base_version TEXT,
    PRIMARY KEY (unit_id, container_id)
);
CREATE TABLE IF NOT EXISTS versions (
    id TEXT PRIMARY KEY,
    unit_id TEXT NOT NULL,
    origin TEXT NOT NULL,
    hlc TEXT NOT NULL,
    record TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS versions_by_unit ON versions (unit_id);
CREATE TABLE IF NOT EXISTS known_blobs (id TEXT PRIMARY KEY);
CREATE TABLE IF NOT EXISTS outbox (id INTEGER PRIMARY KEY AUTOINCREMENT, record TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS inbox (
    unit_id TEXT NOT NULL,
    container_id INTEGER NOT NULL,
    version_id TEXT NOT NULL,
    reason TEXT NOT NULL,
    PRIMARY KEY (unit_id, container_id)
);
CREATE TABLE IF NOT EXISTS cursors (device_id TEXT PRIMARY KEY, last_key TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS conflicts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    unit_id TEXT NOT NULL,
    container_id INTEGER NOT NULL,
    local_version TEXT NOT NULL,
    tips TEXT NOT NULL,
    open INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE IF NOT EXISTS snapshots (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    unit_id TEXT NOT NULL,
    container_id INTEGER NOT NULL,
    content_hash TEXT NOT NULL,
    version_id TEXT,
    size INTEGER NOT NULL,
    taken_ms INTEGER NOT NULL
);
"#;

/// Container flags (mirrors `es_container_flags`).
pub const CF_READ_ONLY: i64 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerRow {
    pub id: i64,
    pub location: String,
    pub format: String,
    pub card_id: String,
    pub flags: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalUnit {
    pub content_hash: String,
    pub base: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredVersion {
    pub rec: VersionRecord,
    pub origin: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictRow {
    pub id: i64,
    pub unit_id: String,
    pub container_id: i64,
    pub local_version: String,
    pub tips: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotRow {
    pub id: i64,
    pub unit_id: String,
    pub content_hash: String,
    pub version_id: Option<String>,
    pub size: u64,
    pub taken_ms: i64,
}

pub struct Store {
    pub(crate) conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// Runs `f` in one transaction.
    pub fn tx<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        let tx = self.conn.unchecked_transaction()?;
        let out = f()?;
        tx.commit()?;
        Ok(out)
    }

    // ---- meta ----

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0)).optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [key, value],
        )?;
        Ok(())
    }

    pub fn delete_meta(&self, key: &str) -> Result<()> {
        self.conn.execute("DELETE FROM meta WHERE key = ?1", [key])?;
        Ok(())
    }

    // ---- devices ----

    pub fn device_name(&self, id: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT name FROM devices WHERE id = ?1", [id], |r| r.get(0)).optional()?)
    }

    pub fn set_device_name(&self, id: &str, name: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO devices (id, name) VALUES (?1, ?2) ON CONFLICT(id) DO UPDATE SET name = excluded.name",
            [id, name],
        )?;
        Ok(())
    }

    // ---- cards ----

    pub fn card(&self, card_id: &str) -> Result<Option<(CardMode, Hlc)>> {
        let row: Option<(String, String)> = self
            .conn
            .query_row("SELECT mode, hlc FROM cards WHERE card_id = ?1", [card_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        Ok(row.and_then(|(m, h)| Some((CardMode::parse(&m)?, Hlc::parse(&h)?))))
    }

    pub fn set_card(&self, card_id: &str, mode: CardMode, hlc: Hlc) -> Result<()> {
        self.conn.execute(
            "INSERT INTO cards (card_id, mode, hlc) VALUES (?1, ?2, ?3)
             ON CONFLICT(card_id) DO UPDATE SET mode = excluded.mode, hlc = excluded.hlc",
            params![card_id, mode.as_str(), hlc.to_string()],
        )?;
        Ok(())
    }

    pub fn cards(&self) -> Result<Vec<(String, CardMode)>> {
        let mut st = self.conn.prepare("SELECT card_id, mode FROM cards ORDER BY card_id")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (id, mode) = row?;
            out.push((id, CardMode::parse(&mode).unwrap_or(CardMode::Units)));
        }
        Ok(out)
    }

    // ---- containers ----

    pub fn insert_container(&self, location: &str, format: &str, card_id: &str, flags: i64) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO containers (location, format, card_id, flags) VALUES (?1, ?2, ?3, ?4)",
            params![location, format, card_id, flags],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn delete_container(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM containers WHERE id = ?1", [id])?;
        self.conn.execute("DELETE FROM local_units WHERE container_id = ?1", [id])?;
        self.conn.execute("DELETE FROM inbox WHERE container_id = ?1", [id])?;
        self.conn.execute("UPDATE conflicts SET open = 0 WHERE container_id = ?1", [id])?;
        Ok(())
    }

    pub fn containers(&self) -> Result<Vec<ContainerRow>> {
        let mut st = self.conn.prepare("SELECT id, location, format, card_id, flags FROM containers ORDER BY id")?;
        let rows = st.query_map([], |r| {
            Ok(ContainerRow {
                id: r.get(0)?,
                location: r.get(1)?,
                format: r.get(2)?,
                card_id: r.get(3)?,
                flags: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ---- local units ----

    pub fn local_unit(&self, unit: &str, container: i64) -> Result<Option<LocalUnit>> {
        Ok(self
            .conn
            .query_row(
                "SELECT content_hash, base_version FROM local_units WHERE unit_id = ?1 AND container_id = ?2",
                params![unit, container],
                |r| Ok(LocalUnit { content_hash: r.get(0)?, base: r.get(1)? }),
            )
            .optional()?)
    }

    pub fn set_local_unit(&self, unit: &str, container: i64, content_hash: &str, base: Option<&str>) -> Result<()> {
        self.conn.execute(
            "INSERT INTO local_units (unit_id, container_id, content_hash, base_version) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(unit_id, container_id) DO UPDATE SET content_hash = excluded.content_hash,
                                                              base_version = excluded.base_version",
            params![unit, container, content_hash, base],
        )?;
        Ok(())
    }

    // ---- versions ----

    /// Inserts a version record. Returns false if it was already known.
    pub fn insert_version(&self, rec: &VersionRecord, origin: &str) -> Result<bool> {
        let n = self.conn.execute(
            "INSERT OR IGNORE INTO versions (id, unit_id, origin, hlc, record) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![rec.id, rec.unit, origin, rec.hlc.to_string(), serde_json::to_string(rec)?],
        )?;
        Ok(n > 0)
    }

    pub fn version(&self, id: &str) -> Result<Option<StoredVersion>> {
        let row: Option<(String, String)> = self
            .conn
            .query_row("SELECT record, origin FROM versions WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        row.map(|(rec, origin)| Ok(StoredVersion { rec: serde_json::from_str(&rec)?, origin })).transpose()
    }

    pub fn versions_of(&self, unit: &str) -> Result<Vec<StoredVersion>> {
        let mut st = self.conn.prepare("SELECT record, origin FROM versions WHERE unit_id = ?1")?;
        let rows = st.query_map([unit], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (rec, origin) = row?;
            out.push(StoredVersion { rec: serde_json::from_str(&rec)?, origin });
        }
        Ok(out)
    }

    pub fn all_version_ids(&self) -> Result<Vec<String>> {
        let mut st = self.conn.prepare("SELECT id FROM versions ORDER BY id")?;
        let rows = st.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ---- blobs ----

    pub fn blob_known(&self, id: &str) -> Result<bool> {
        Ok(self.conn.query_row("SELECT 1 FROM known_blobs WHERE id = ?1", [id], |_| Ok(())).optional()?.is_some())
    }

    pub fn mark_blob_known(&self, id: &str) -> Result<()> {
        self.conn.execute("INSERT OR IGNORE INTO known_blobs (id) VALUES (?1)", [id])?;
        Ok(())
    }

    // ---- outbox ----

    pub fn outbox_push(&self, line: &str) -> Result<()> {
        self.conn.execute("INSERT INTO outbox (record) VALUES (?1)", [line])?;
        Ok(())
    }

    pub fn outbox(&self) -> Result<Vec<(i64, String)>> {
        let mut st = self.conn.prepare("SELECT id, record FROM outbox ORDER BY id")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn outbox_delete_upto(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM outbox WHERE id <= ?1", [id])?;
        Ok(())
    }

    // ---- inbox ----

    pub fn inbox_put(&self, unit: &str, container: i64, version: &str, reason: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO inbox (unit_id, container_id, version_id, reason) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(unit_id, container_id) DO UPDATE SET version_id = excluded.version_id, reason = excluded.reason",
            params![unit, container, version, reason],
        )?;
        Ok(())
    }

    pub fn inbox_remove(&self, unit: &str, container: i64) -> Result<()> {
        self.conn.execute("DELETE FROM inbox WHERE unit_id = ?1 AND container_id = ?2", params![unit, container])?;
        Ok(())
    }

    pub fn inbox(&self) -> Result<Vec<(String, i64, String, String)>> {
        let mut st =
            self.conn.prepare("SELECT unit_id, container_id, version_id, reason FROM inbox ORDER BY unit_id")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ---- cursors ----

    pub fn cursor(&self, device: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT last_key FROM cursors WHERE device_id = ?1", [device], |r| r.get(0))
            .optional()?)
    }

    pub fn set_cursor(&self, device: &str, key: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO cursors (device_id, last_key) VALUES (?1, ?2)
             ON CONFLICT(device_id) DO UPDATE SET last_key = excluded.last_key",
            [device, key],
        )?;
        Ok(())
    }

    // ---- conflicts ----

    /// Opens a conflict for (unit, container), or refreshes the open one. Returns its id.
    pub fn upsert_conflict(&self, unit: &str, container: i64, local: &str, tips: &[String]) -> Result<i64> {
        let tips = serde_json::to_string(tips)?;
        let existing: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM conflicts WHERE unit_id = ?1 AND container_id = ?2 AND open = 1",
                params![unit, container],
                |r| r.get(0),
            )
            .optional()?;
        match existing {
            Some(id) => {
                self.conn.execute(
                    "UPDATE conflicts SET local_version = ?2, tips = ?3 WHERE id = ?1",
                    params![id, local, tips],
                )?;
                Ok(id)
            }
            None => {
                self.conn.execute(
                    "INSERT INTO conflicts (unit_id, container_id, local_version, tips) VALUES (?1, ?2, ?3, ?4)",
                    params![unit, container, local, tips],
                )?;
                Ok(self.conn.last_insert_rowid())
            }
        }
    }

    pub fn close_conflicts(&self, unit: &str) -> Result<()> {
        self.conn.execute("UPDATE conflicts SET open = 0 WHERE unit_id = ?1 AND open = 1", [unit])?;
        Ok(())
    }

    pub fn open_conflicts(&self) -> Result<Vec<ConflictRow>> {
        let mut st = self.conn.prepare(
            "SELECT id, unit_id, container_id, local_version, tips FROM conflicts WHERE open = 1 ORDER BY id",
        )?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get::<_, String>(4)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (id, unit_id, container_id, local_version, tips) = row?;
            out.push(ConflictRow { id, unit_id, container_id, local_version, tips: serde_json::from_str(&tips)? });
        }
        Ok(out)
    }

    // ---- snapshots ----

    pub fn add_snapshot(
        &self,
        unit: &str,
        container: i64,
        content_hash: &str,
        version: Option<&str>,
        size: u64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO snapshots (unit_id, container_id, content_hash, version_id, size, taken_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![unit, container, content_hash, version, size as i64, Hlc::wall_ms()],
        )?;
        Ok(())
    }

    pub fn snapshots(&self) -> Result<Vec<SnapshotRow>> {
        let mut st = self
            .conn
            .prepare("SELECT id, unit_id, content_hash, version_id, size, taken_ms FROM snapshots ORDER BY id")?;
        let rows = st.query_map([], |r| {
            Ok(SnapshotRow {
                id: r.get(0)?,
                unit_id: r.get(1)?,
                content_hash: r.get(2)?,
                version_id: r.get(3)?,
                size: r.get::<_, i64>(4)? as u64,
                taken_ms: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn delete_snapshot(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM snapshots WHERE id = ?1", [id])?;
        Ok(())
    }
}

//! SQLite read cache. One table, JSON payload per account plus the columns we
//! filter on. The chain is the source of truth; this exists so the frontend
//! reads hit a local file instead of RPC on every page load.
//!
//! ponytail: one denormalised table with a `json` blob, not a column-per-field
//! schema per account type. The frontend wants whole objects, the indexer has
//! them as json already, and we only ever filter by (kind, pool, owner). Split
//! into typed tables if you ever need to query on a field inside the blob.

use std::sync::Mutex;

use rusqlite::{params, Connection};

pub struct Db(pub Mutex<Connection>);

impl Db {
    pub fn open(path: &str) -> rusqlite::Result<Db> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS cache (
                pubkey     TEXT PRIMARY KEY,
                kind       TEXT NOT NULL,
                pool       TEXT,
                owner      TEXT,
                json       TEXT NOT NULL,
                updated_ts INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS cache_kind_pool ON cache(kind, pool);
            CREATE INDEX IF NOT EXISTS cache_owner ON cache(owner);",
        )?;
        Ok(Db(Mutex::new(conn)))
    }

    pub fn upsert(
        &self,
        pubkey: &str,
        kind: &str,
        pool: Option<&str>,
        owner: Option<&str>,
        json: &str,
        ts: i64,
    ) -> rusqlite::Result<()> {
        self.0.lock().unwrap().execute(
            "INSERT INTO cache (pubkey, kind, pool, owner, json, updated_ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(pubkey) DO UPDATE SET
               json = excluded.json, updated_ts = excluded.updated_ts,
               pool = excluded.pool, owner = excluded.owner",
            params![pubkey, kind, pool, owner, json, ts],
        )?;
        Ok(())
    }

    pub fn get(&self, pubkey: &str) -> rusqlite::Result<Option<String>> {
        self.0
            .lock()
            .unwrap()
            .query_row(
                "SELECT json FROM cache WHERE pubkey = ?1",
                params![pubkey],
                |r| r.get(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })
    }

    /// Rows of a kind, optionally narrowed by pool and/or owner. A `None` filter
    /// is a NULL bind that the `?N IS NULL OR ...` guard passes through.
    pub fn list(
        &self,
        kind: &str,
        pool: Option<&str>,
        owner: Option<&str>,
    ) -> rusqlite::Result<Vec<String>> {
        let conn = self.0.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT json FROM cache
             WHERE kind = ?1 AND (?2 IS NULL OR pool = ?2) AND (?3 IS NULL OR owner = ?3)",
        )?;
        let rows = stmt.query_map(params![kind, pool, owner], |r| r.get::<_, String>(0))?;
        rows.collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_get_list_roundtrip() {
        let db = Db::open(":memory:").unwrap();
        db.upsert("P1", "pool", None, None, r#"{"epoch":1}"#, 10).unwrap();
        db.upsert("X1", "position", Some("P1"), Some("alice"), r#"{"id":1}"#, 10).unwrap();
        db.upsert("X2", "position", Some("P1"), Some("bob"), r#"{"id":2}"#, 10).unwrap();
        // update in place
        db.upsert("P1", "pool", None, None, r#"{"epoch":2}"#, 20).unwrap();

        assert_eq!(db.get("P1").unwrap().as_deref(), Some(r#"{"epoch":2}"#));
        assert_eq!(db.get("nope").unwrap(), None);
        assert_eq!(db.list("position", Some("P1"), None).unwrap().len(), 2);
        assert_eq!(db.list("position", Some("P1"), Some("alice")).unwrap().len(), 1);
        assert_eq!(db.list("pool", None, None).unwrap().len(), 1);
    }
}

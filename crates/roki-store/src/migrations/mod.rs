//! Forward-only embedded migrations.
//!
//! Each migration is a `(version, name, sql)` triple. Versions are dense and
//! must increase monotonically. The runner records applied versions in a
//! `schema_migrations` table; rerunning the same SQL is rejected.
//!
//! Backward migrations are intentionally absent: roki is operator-deployed
//! single-binary; downgrade story is "restore the previous binary against a
//! backup of `roki.db`". This keeps the runner small and the audit trail
//! linear.

use rusqlite::Connection;

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy)]
pub struct Migration {
    pub version: i64,
    pub name: &'static str,
    pub sql: &'static str,
}

pub const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "init",
    sql: include_str!("0001_init.sql"),
}];

pub fn latest_version() -> i64 {
    MIGRATIONS.last().map(|m| m.version).unwrap_or(0)
}

/// Apply every migration whose version is greater than the current
/// `user_version`. Fails fast if the DB is ahead of this binary's known
/// `MIGRATIONS` (forbids silent downgrades that would corrupt newer rows).
pub fn run(conn: &mut Connection) -> Result<()> {
    ensure_meta(conn)?;

    let current: i64 = conn.query_row("PRAGMA user_version;", [], |row| row.get(0))?;
    let target = latest_version();

    if current > target {
        return Err(Error::SchemaTooNew {
            found: current,
            max_supported: target,
        });
    }
    if current == target {
        return Ok(());
    }

    for m in MIGRATIONS.iter().filter(|m| m.version > current) {
        let tx = conn.transaction()?;
        tx.execute_batch(m.sql).map_err(|source| Error::Migration {
            version: m.version,
            name: m.name,
            source,
        })?;
        tx.execute(
            "INSERT INTO schema_migrations(version, name, applied_at) VALUES (?1, ?2, ?3);",
            rusqlite::params![m.version, m.name, now_millis()],
        )?;
        tx.pragma_update(None, "user_version", m.version)?;
        tx.commit()?;
        tracing::info!(version = m.version, name = m.name, "applied migration");
    }

    Ok(())
}

fn ensure_meta(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version    INTEGER PRIMARY KEY,
            name       TEXT NOT NULL,
            applied_at INTEGER NOT NULL
        ) STRICT;",
    )?;
    Ok(())
}

fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_db_reaches_latest() {
        let mut conn = Connection::open_in_memory().unwrap();
        run(&mut conn).unwrap();
        let v: i64 = conn
            .query_row("PRAGMA user_version;", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, latest_version());
    }

    #[test]
    fn second_run_is_noop() {
        let mut conn = Connection::open_in_memory().unwrap();
        run(&mut conn).unwrap();
        run(&mut conn).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_migrations;", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, latest_version());
    }

    #[test]
    fn newer_db_is_rejected() {
        let mut conn = Connection::open_in_memory().unwrap();
        run(&mut conn).unwrap();
        conn.pragma_update(None, "user_version", latest_version() + 1)
            .unwrap();
        let err = run(&mut conn).unwrap_err();
        assert!(matches!(err, Error::SchemaTooNew { .. }));
    }
}

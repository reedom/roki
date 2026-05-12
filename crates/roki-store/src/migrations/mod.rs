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

pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "init",
        sql: include_str!("0001_init.sql"),
    },
    Migration {
        version: 2,
        name: "cycles_uuid",
        sql: include_str!("0002_cycles_uuid.sql"),
    },
];

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

    // FK enforcement must be disabled while the migration runs: SQLite cannot
    // ALTER COLUMN type in place, so any retype goes through DROP + RENAME on
    // tables that may already carry child rows. `PRAGMA foreign_keys` is a
    // no-op inside a transaction, so we flip it here at the connection scope
    // before opening the migration tx, then run `foreign_key_check` after the
    // tx commits to surface any orphan FK left behind by a buggy migration.
    let fk_was_on: bool = conn.query_row("PRAGMA foreign_keys;", [], |r| r.get(0))?;
    if fk_was_on {
        conn.pragma_update(None, "foreign_keys", false)?;
    }

    let result = (|| -> Result<()> {
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
    })();

    if fk_was_on {
        conn.pragma_update(None, "foreign_keys", true)?;
        // Surface any orphaned FK left behind by a migration. `foreign_key_check`
        // returns one row per offending child-row; treat its first row as an
        // error so the daemon refuses to start on a corrupt schema.
        let mut stmt = conn.prepare("PRAGMA foreign_key_check;")?;
        let mut rows = stmt.query([])?;
        if let Some(row) = rows.next()? {
            let table: String = row.get(0)?;
            let rowid: Option<i64> = row.get(1)?;
            let parent: String = row.get(2)?;
            return Err(Error::Sqlite(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY),
                Some(format!(
                    "post-migration FK violation: {table}(rowid={rowid:?}) -> {parent}"
                )),
            )));
        }
    }

    result
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
    fn v1_db_with_rows_upgrades_cleanly_to_v2() {
        // Build a fresh DB at v1 by applying only the v1 migration, then
        // INSERT a ticket + event row, then run the full migration runner.
        let mut conn = Connection::open_in_memory().unwrap();
        ensure_meta(&conn).unwrap();
        {
            let tx = conn.transaction().unwrap();
            tx.execute_batch(MIGRATIONS[0].sql).unwrap();
            tx.execute(
                "INSERT INTO schema_migrations(version, name, applied_at) VALUES (?1, ?2, ?3);",
                rusqlite::params![MIGRATIONS[0].version, MIGRATIONS[0].name, 0_i64],
            )
            .unwrap();
            tx.pragma_update(None, "user_version", MIGRATIONS[0].version)
                .unwrap();
            tx.commit().unwrap();
        }
        conn.execute(
            "INSERT INTO tickets(id, repo, admitted_at, evicted_at) VALUES('T1','r',1,NULL);",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO events(ticket_id, cycle_id, ts, kind, payload)
             VALUES('T1', NULL, 1, 'k', '{}');",
            [],
        )
        .unwrap();

        run(&mut conn).unwrap();

        let v: i64 = conn
            .query_row("PRAGMA user_version;", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, latest_version());

        // Ticket survives.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM tickets;", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);

        // Event row survives with the same seq.
        let (seq, cycle_id): (i64, Option<String>) = conn
            .query_row(
                "SELECT seq, cycle_id FROM events WHERE ticket_id='T1';",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(seq, 1);
        assert!(cycle_id.is_none());

        // events.cycle_id column type is now TEXT (affinity).
        let mut stmt = conn.prepare("PRAGMA table_info(events);").unwrap();
        let cols: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, String>(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        let cycle_col = cols
            .iter()
            .find(|(name, _)| name == "cycle_id")
            .expect("cycle_id column present");
        assert_eq!(cycle_col.1, "TEXT");

        // cycles.id column is TEXT.
        let mut stmt = conn.prepare("PRAGMA table_info(cycles);").unwrap();
        let cols: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, String>(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        let id_col = cols
            .iter()
            .find(|(name, _)| name == "id")
            .expect("cycles.id present");
        assert_eq!(id_col.1, "TEXT");
    }

    #[test]
    fn v1_to_v2_preserves_child_fk_rows() {
        // Stress the FK path: a v1 DB whose `cycles` row is referenced by
        // `state_visits`, `subprocess_runs`, and `events`. With
        // `foreign_keys = ON` (set by SqliteStore::configure / the run() path
        // when invoked through it), the DROP TABLE inside 0002 would fail
        // without `PRAGMA defer_foreign_keys = 1`. This test catches a
        // regression on that pragma.
        let mut conn = Connection::open_in_memory().unwrap();
        // Mirror the production setup: FKs ON before migrations run.
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        ensure_meta(&conn).unwrap();
        {
            let tx = conn.transaction().unwrap();
            tx.execute_batch(MIGRATIONS[0].sql).unwrap();
            tx.execute(
                "INSERT INTO schema_migrations(version, name, applied_at) VALUES (?1, ?2, ?3);",
                rusqlite::params![MIGRATIONS[0].version, MIGRATIONS[0].name, 0_i64],
            )
            .unwrap();
            tx.pragma_update(None, "user_version", MIGRATIONS[0].version)
                .unwrap();
            tx.commit().unwrap();
        }

        conn.execute(
            "INSERT INTO tickets(id, repo, admitted_at, evicted_at) VALUES('T1','r',1,NULL);",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO cycles(ticket_id, kind, entry_name, started_at)
             VALUES('T1','rule','e',1);",
            [],
        )
        .unwrap();
        let old_cycle_rowid: i64 = conn
            .query_row("SELECT id FROM cycles WHERE ticket_id='T1';", [], |r| {
                r.get(0)
            })
            .unwrap();
        conn.execute(
            "INSERT INTO state_visits(cycle_id, state_id, visits) VALUES(?1,'s1',1);",
            rusqlite::params![old_cycle_rowid],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO subprocess_runs
                (cycle_id, state_id, visit, started_at, ended_at, exit_code, capture_dir)
             VALUES(?1,'s1',1,1,2,0,'/tmp/cap');",
            rusqlite::params![old_cycle_rowid],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO events(ticket_id, cycle_id, ts, kind, payload)
             VALUES('T1', ?1, 1, 'k', '{}');",
            rusqlite::params![old_cycle_rowid],
        )
        .unwrap();

        run(&mut conn).unwrap();

        // All four child rows survive with the CAST(id AS TEXT) value.
        let new_cycle_id: String = conn
            .query_row("SELECT id FROM cycles WHERE ticket_id='T1';", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(new_cycle_id, old_cycle_rowid.to_string());
        for tbl in &["state_visits", "subprocess_runs", "events"] {
            let n: i64 = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM {tbl} WHERE cycle_id = ?1;"),
                    rusqlite::params![&new_cycle_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "{tbl} row should reference the migrated cycle id");
        }
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

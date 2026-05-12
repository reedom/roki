//! SQLite-backed [`Store`] implementation.
//!
//! Single-connection, single-writer. WAL mode lets external readers
//! (TUI tail, `roki events`) open the same file in their own processes
//! without blocking the daemon writer. Inside the daemon, the connection is
//! wrapped in a `Mutex` so the trait can be `Sync` without surprising the
//! caller with `&mut self` requirements.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{Error, Result};
use crate::migrations;
use crate::models::{
    Cycle, CycleKind, CycleOutcome, Escalation, Event, NewCycle, NewEscalation, NewEvent,
    StateVisit, SubprocessRun, Ticket, UnixMillis,
};
use crate::store::Store;

pub struct SqliteStore {
    conn: Mutex<Connection>,
}

impl SqliteStore {
    /// Open or create the store file at `path`, run pending migrations, and
    /// configure WAL + `synchronous=NORMAL` (durable across crashes for
    /// committed transactions; trades a final-flush window for ~5x write
    /// throughput vs FULL — acceptable for an event log).
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path: PathBuf = path.as_ref().to_path_buf();
        let mut conn = Connection::open(&path).map_err(|source| Error::Open {
            path: path.clone(),
            source,
        })?;
        Self::configure(&conn)?;
        migrations::run(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// In-memory store for tests. Each call returns an independent DB.
    pub fn open_memory() -> Result<Self> {
        let mut conn = Connection::open_in_memory()?;
        Self::configure(&conn)?;
        migrations::run(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn configure(conn: &Connection) -> Result<()> {
        // STRICT tables require modern SQLite; bundled feature pins one.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "busy_timeout", 5_000)?;
        Ok(())
    }

    fn with_conn<R>(&self, f: impl FnOnce(&mut Connection) -> Result<R>) -> Result<R> {
        let mut guard = self.conn.lock().expect("store mutex poisoned");
        f(&mut guard)
    }
}

fn map_cycle(row: &rusqlite::Row<'_>) -> rusqlite::Result<Cycle> {
    let kind_s: String = row.get("kind")?;
    let outcome_s: Option<String> = row.get("outcome")?;
    Ok(Cycle {
        id: row.get("id")?,
        ticket_id: row.get("ticket_id")?,
        kind: parse_kind(&kind_s).map_err(to_sqlite_err)?,
        entry_name: row.get("entry_name")?,
        started_at: row.get("started_at")?,
        ended_at: row.get("ended_at")?,
        outcome: outcome_s
            .as_deref()
            .map(parse_outcome)
            .transpose()
            .map_err(to_sqlite_err)?,
        current_state: row.get("current_state")?,
        iter: row.get::<_, i64>("iter")? as u32,
    })
}

fn parse_kind(s: &str) -> Result<CycleKind> {
    Ok(match s {
        "rule" => CycleKind::Rule,
        "cleanup" => CycleKind::Cleanup,
        "failure" => CycleKind::Failure,
        other => {
            return Err(Error::Sqlite(rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                format!("unknown cycle kind: {other}").into(),
            )));
        }
    })
}

fn parse_outcome(s: &str) -> Result<CycleOutcome> {
    Ok(match s {
        "success" => CycleOutcome::Success,
        "failure" => CycleOutcome::Failure,
        "no_action" => CycleOutcome::NoAction,
        "cancelled" => CycleOutcome::Cancelled,
        other => {
            return Err(Error::Sqlite(rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                format!("unknown cycle outcome: {other}").into(),
            )));
        }
    })
}

fn to_sqlite_err(e: Error) -> rusqlite::Error {
    match e {
        Error::Sqlite(s) => s,
        other => rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            other.to_string().into(),
        ),
    }
}

impl Store for SqliteStore {
    // --- tickets ---------------------------------------------------------

    fn admit_ticket(&self, id: &str, repo: &str, at: UnixMillis) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO tickets(id, repo, admitted_at, evicted_at)
                 VALUES(?1, ?2, ?3, NULL)
                 ON CONFLICT(id) DO UPDATE SET
                    repo = excluded.repo,
                    admitted_at = excluded.admitted_at,
                    evicted_at = NULL;",
                params![id, repo, at],
            )?;
            Ok(())
        })
    }

    fn evict_ticket(&self, id: &str, at: UnixMillis) -> Result<()> {
        self.with_conn(|c| {
            let n = c.execute(
                "UPDATE tickets SET evicted_at = ?2 WHERE id = ?1 AND evicted_at IS NULL;",
                params![id, at],
            )?;
            if n == 0 {
                return Err(Error::NotFound);
            }
            Ok(())
        })
    }

    fn list_admitted(&self) -> Result<Vec<Ticket>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, repo, admitted_at, evicted_at
                 FROM tickets WHERE evicted_at IS NULL ORDER BY admitted_at;",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(Ticket {
                        id: r.get("id")?,
                        repo: r.get("repo")?,
                        admitted_at: r.get("admitted_at")?,
                        evicted_at: r.get("evicted_at")?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    fn get_ticket(&self, id: &str) -> Result<Option<Ticket>> {
        self.with_conn(|c| {
            let row = c
                .query_row(
                    "SELECT id, repo, admitted_at, evicted_at FROM tickets WHERE id = ?1;",
                    params![id],
                    |r| {
                        Ok(Ticket {
                            id: r.get("id")?,
                            repo: r.get("repo")?,
                            admitted_at: r.get("admitted_at")?,
                            evicted_at: r.get("evicted_at")?,
                        })
                    },
                )
                .optional()?;
            Ok(row)
        })
    }

    // --- cycles ----------------------------------------------------------

    fn open_cycle(&self, c: NewCycle) -> Result<Cycle> {
        self.with_conn(|conn| {
            let tx = conn.transaction()?;
            tx.execute(
                "INSERT INTO cycles(id, ticket_id, kind, entry_name, started_at, iter)
                 VALUES(?1, ?2, ?3, ?4, ?5, 0);",
                params![
                    c.id,
                    c.ticket_id,
                    c.kind.as_str(),
                    c.entry_name,
                    c.started_at
                ],
            )?;
            let cycle = tx.query_row(
                "SELECT id, ticket_id, kind, entry_name, started_at, ended_at,
                        outcome, current_state, iter
                 FROM cycles WHERE id = ?1;",
                params![c.id],
                map_cycle,
            )?;
            tx.commit()?;
            Ok(cycle)
        })
    }

    fn set_current_state(&self, cycle_id: &str, state_id: &str, iter: u32) -> Result<()> {
        self.with_conn(|c| {
            let n = c.execute(
                "UPDATE cycles SET current_state = ?2, iter = ?3 WHERE id = ?1;",
                params![cycle_id, state_id, iter as i64],
            )?;
            if n == 0 {
                return Err(Error::NotFound);
            }
            Ok(())
        })
    }

    fn bump_visit(&self, cycle_id: &str, state_id: &str) -> Result<u32> {
        self.with_conn(|c| {
            let tx = c.transaction()?;
            tx.execute(
                "INSERT INTO state_visits(cycle_id, state_id, visits) VALUES(?1, ?2, 1)
                 ON CONFLICT(cycle_id, state_id) DO UPDATE SET visits = visits + 1;",
                params![cycle_id, state_id],
            )?;
            let v: i64 = tx.query_row(
                "SELECT visits FROM state_visits WHERE cycle_id = ?1 AND state_id = ?2;",
                params![cycle_id, state_id],
                |r| r.get(0),
            )?;
            tx.commit()?;
            Ok(v as u32)
        })
    }

    fn close_cycle(
        &self,
        cycle_id: &str,
        outcome: CycleOutcome,
        ended_at: UnixMillis,
    ) -> Result<()> {
        self.with_conn(|c| {
            let n = c.execute(
                "UPDATE cycles SET outcome = ?2, ended_at = ?3
                 WHERE id = ?1 AND ended_at IS NULL;",
                params![cycle_id, outcome.as_str(), ended_at],
            )?;
            if n == 0 {
                return Err(Error::NotFound);
            }
            Ok(())
        })
    }

    fn get_cycle(&self, cycle_id: &str) -> Result<Option<Cycle>> {
        self.with_conn(|c| {
            let row = c
                .query_row(
                    "SELECT id, ticket_id, kind, entry_name, started_at, ended_at,
                            outcome, current_state, iter
                     FROM cycles WHERE id = ?1;",
                    params![cycle_id],
                    map_cycle,
                )
                .optional()?;
            Ok(row)
        })
    }

    fn list_inflight_cycles(&self) -> Result<Vec<Cycle>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, ticket_id, kind, entry_name, started_at, ended_at,
                        outcome, current_state, iter
                 FROM cycles WHERE ended_at IS NULL ORDER BY started_at;",
            )?;
            let rows = stmt
                .query_map([], map_cycle)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    fn visits_for_cycle(&self, cycle_id: &str) -> Result<Vec<StateVisit>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT cycle_id, state_id, visits FROM state_visits
                 WHERE cycle_id = ?1 ORDER BY state_id;",
            )?;
            let rows = stmt
                .query_map(params![cycle_id], |r| {
                    Ok(StateVisit {
                        cycle_id: r.get("cycle_id")?,
                        state_id: r.get("state_id")?,
                        visits: r.get::<_, i64>("visits")? as u32,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    // --- events ----------------------------------------------------------

    fn append_event(&self, e: NewEvent) -> Result<Event> {
        let payload_text = serde_json::to_string(&e.payload)?;
        self.with_conn(|c| {
            let tx = c.transaction()?;
            tx.execute(
                "INSERT INTO events(ticket_id, cycle_id, ts, kind, payload)
                 VALUES(?1, ?2, ?3, ?4, ?5);",
                params![e.ticket_id, e.cycle_id, e.ts, e.kind, payload_text],
            )?;
            let seq = tx.last_insert_rowid();
            tx.commit()?;
            Ok(Event {
                seq,
                ticket_id: e.ticket_id,
                cycle_id: e.cycle_id,
                ts: e.ts,
                kind: e.kind,
                payload: e.payload,
            })
        })
    }

    fn events_since(
        &self,
        ticket_id: &str,
        since_seq: i64,
        limit: usize,
    ) -> Result<Vec<Event>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT seq, ticket_id, cycle_id, ts, kind, payload
                 FROM events
                 WHERE ticket_id = ?1 AND seq > ?2
                 ORDER BY seq ASC LIMIT ?3;",
            )?;
            let rows = stmt
                .query_map(params![ticket_id, since_seq, limit as i64], |r| {
                    let payload_text: String = r.get("payload")?;
                    let payload: serde_json::Value =
                        serde_json::from_str(&payload_text).map_err(|err| {
                            rusqlite::Error::FromSqlConversionFailure(
                                0,
                                rusqlite::types::Type::Text,
                                Box::new(err),
                            )
                        })?;
                    Ok(Event {
                        seq: r.get("seq")?,
                        ticket_id: r.get("ticket_id")?,
                        cycle_id: r.get("cycle_id")?,
                        ts: r.get("ts")?,
                        kind: r.get("kind")?,
                        payload,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    fn latest_event_seq(&self, ticket_id: &str) -> Result<Option<i64>> {
        self.with_conn(|c| {
            let v: Option<i64> = c
                .query_row(
                    "SELECT MAX(seq) FROM events WHERE ticket_id = ?1;",
                    params![ticket_id],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            Ok(v)
        })
    }

    // --- subprocess registry --------------------------------------------

    fn register_subprocess(&self, run: SubprocessRun) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO subprocess_runs
                    (cycle_id, state_id, visit, started_at, ended_at, exit_code, capture_dir)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7);",
                params![
                    run.cycle_id,
                    run.state_id,
                    run.visit as i64,
                    run.started_at,
                    run.ended_at,
                    run.exit_code,
                    run.capture_dir,
                ],
            )?;
            Ok(())
        })
    }

    fn finish_subprocess(
        &self,
        cycle_id: &str,
        state_id: &str,
        visit: u32,
        exit_code: i32,
        ended_at: UnixMillis,
    ) -> Result<()> {
        self.with_conn(|c| {
            let n = c.execute(
                "UPDATE subprocess_runs SET ended_at = ?4, exit_code = ?5
                 WHERE cycle_id = ?1 AND state_id = ?2 AND visit = ?3 AND ended_at IS NULL;",
                params![cycle_id, state_id, visit as i64, ended_at, exit_code],
            )?;
            if n == 0 {
                return Err(Error::NotFound);
            }
            Ok(())
        })
    }

    fn list_subprocesses(&self, cycle_id: &str) -> Result<Vec<SubprocessRun>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT cycle_id, state_id, visit, started_at, ended_at, exit_code, capture_dir
                 FROM subprocess_runs WHERE cycle_id = ?1 ORDER BY started_at;",
            )?;
            let rows = stmt
                .query_map(params![cycle_id], |r| {
                    Ok(SubprocessRun {
                        cycle_id: r.get("cycle_id")?,
                        state_id: r.get("state_id")?,
                        visit: r.get::<_, i64>("visit")? as u32,
                        started_at: r.get("started_at")?,
                        ended_at: r.get("ended_at")?,
                        exit_code: r.get("exit_code")?,
                        capture_dir: r.get("capture_dir")?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    // --- escalations -----------------------------------------------------

    fn enqueue_escalation(&self, e: NewEscalation) -> Result<Escalation> {
        self.with_conn(|c| {
            let tx = c.transaction()?;
            tx.execute(
                "INSERT INTO escalations(ticket_id, reason, created_at) VALUES(?1, ?2, ?3);",
                params![e.ticket_id, e.reason, e.created_at],
            )?;
            let id = tx.last_insert_rowid();
            tx.commit()?;
            Ok(Escalation {
                id,
                ticket_id: e.ticket_id,
                reason: e.reason,
                created_at: e.created_at,
                ack_at: None,
            })
        })
    }

    fn ack_escalation(&self, id: i64, at: UnixMillis) -> Result<()> {
        self.with_conn(|c| {
            let n = c.execute(
                "UPDATE escalations SET ack_at = ?2 WHERE id = ?1 AND ack_at IS NULL;",
                params![id, at],
            )?;
            if n == 0 {
                return Err(Error::NotFound);
            }
            Ok(())
        })
    }

    fn list_open_escalations(&self) -> Result<Vec<Escalation>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, ticket_id, reason, created_at, ack_at
                 FROM escalations WHERE ack_at IS NULL ORDER BY created_at;",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(Escalation {
                        id: r.get("id")?,
                        ticket_id: r.get("ticket_id")?,
                        reason: r.get("reason")?,
                        created_at: r.get("created_at")?,
                        ack_at: r.get("ack_at")?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{CycleKind, NewCycle, NewEvent};
    use serde_json::json;

    fn store() -> SqliteStore {
        SqliteStore::open_memory().unwrap()
    }

    #[test]
    fn admit_and_evict_ticket() {
        let s = store();
        s.admit_ticket("OPS-1", "github.com/x/y", 100).unwrap();
        assert_eq!(s.list_admitted().unwrap().len(), 1);
        s.evict_ticket("OPS-1", 200).unwrap();
        assert!(s.list_admitted().unwrap().is_empty());
        let t = s.get_ticket("OPS-1").unwrap().unwrap();
        assert_eq!(t.evicted_at, Some(200));
    }

    #[test]
    fn re_admit_clears_eviction() {
        let s = store();
        s.admit_ticket("OPS-1", "r", 100).unwrap();
        s.evict_ticket("OPS-1", 200).unwrap();
        s.admit_ticket("OPS-1", "r", 300).unwrap();
        let t = s.get_ticket("OPS-1").unwrap().unwrap();
        assert_eq!(t.evicted_at, None);
        assert_eq!(t.admitted_at, 300);
    }

    #[test]
    fn cycle_fsm_round_trip() {
        let s = store();
        s.admit_ticket("OPS-1", "r", 0).unwrap();
        let cycle_id = "11111111-1111-1111-1111-111111111111";
        let c = s
            .open_cycle(NewCycle {
                id: cycle_id.into(),
                ticket_id: "OPS-1".into(),
                kind: CycleKind::Rule,
                entry_name: "first-rule".into(),
                started_at: 1,
            })
            .unwrap();
        assert_eq!(c.id, cycle_id);
        s.set_current_state(&c.id, "running", 1).unwrap();
        assert_eq!(s.bump_visit(&c.id, "running").unwrap(), 1);
        assert_eq!(s.bump_visit(&c.id, "running").unwrap(), 2);
        s.close_cycle(&c.id, CycleOutcome::Success, 9).unwrap();
        let got = s.get_cycle(&c.id).unwrap().unwrap();
        assert_eq!(got.outcome, Some(CycleOutcome::Success));
        assert!(s.list_inflight_cycles().unwrap().is_empty());
    }

    #[test]
    fn events_append_and_tail() {
        let s = store();
        s.admit_ticket("OPS-1", "r", 0).unwrap();
        for i in 0..3 {
            s.append_event(NewEvent {
                ticket_id: "OPS-1".into(),
                cycle_id: None,
                ts: i,
                kind: "tick".into(),
                payload: json!({ "i": i }),
            })
            .unwrap();
        }
        let first_two = s.events_since("OPS-1", 0, 2).unwrap();
        assert_eq!(first_two.len(), 2);
        assert_eq!(first_two[0].payload, json!({ "i": 0 }));
        let last = s
            .events_since("OPS-1", first_two[1].seq, 10)
            .unwrap();
        assert_eq!(last.len(), 1);
        assert_eq!(last[0].payload, json!({ "i": 2 }));
        assert_eq!(s.latest_event_seq("OPS-1").unwrap(), Some(3));
    }

    #[test]
    fn escalation_ack_flow() {
        let s = store();
        let e = s
            .enqueue_escalation(NewEscalation {
                ticket_id: Some("OPS-1".into()),
                reason: "fs_poison".into(),
                created_at: 1,
            })
            .unwrap();
        assert_eq!(s.list_open_escalations().unwrap().len(), 1);
        s.ack_escalation(e.id, 5).unwrap();
        assert!(s.list_open_escalations().unwrap().is_empty());
    }

    #[test]
    fn reopen_persists_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("roki.db");
        {
            let s = SqliteStore::open(&path).unwrap();
            s.admit_ticket("OPS-1", "r", 1).unwrap();
        }
        let s = SqliteStore::open(&path).unwrap();
        assert_eq!(s.list_admitted().unwrap().len(), 1);
    }
}

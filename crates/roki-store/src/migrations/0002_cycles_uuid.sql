-- roki-store schema v2: switch cycles.id and events.cycle_id from INTEGER to TEXT.
--
-- Phase-3 needs the daemon to own the cycle UUID (so the routing-key UUID on
-- emitted events matches the row in `cycles` without a second roundtrip).
-- SQLite cannot ALTER COLUMN type in place, so this migration follows the
-- canonical create-new / INSERT SELECT / drop / rename pattern. The runner
-- (`migrations::run`) flips `PRAGMA foreign_keys = OFF` at the connection
-- scope before applying every migration and runs `foreign_key_check` after
-- COMMIT so a buggy migration that leaves an orphan FK cannot silently make
-- it past startup.

CREATE TABLE cycles_new (
    id            TEXT    PRIMARY KEY,
    ticket_id     TEXT    NOT NULL REFERENCES tickets(id),
    kind          TEXT    NOT NULL CHECK (kind IN ('rule','cleanup','failure')),
    entry_name    TEXT    NOT NULL,
    started_at    INTEGER NOT NULL,
    ended_at      INTEGER,
    outcome       TEXT    CHECK (outcome IN ('success','failure','no_action','cancelled')),
    current_state TEXT,
    iter          INTEGER NOT NULL DEFAULT 0
) STRICT;

-- Existing rows (in practice zero — phase-1/2 never INSERT into cycles) are
-- copied with the integer id stringified so any test fixture survives the bump.
INSERT INTO cycles_new (id, ticket_id, kind, entry_name, started_at, ended_at,
                        outcome, current_state, iter)
SELECT CAST(id AS TEXT), ticket_id, kind, entry_name, started_at, ended_at,
       outcome, current_state, iter
FROM cycles;

DROP TABLE cycles;
ALTER TABLE cycles_new RENAME TO cycles;

CREATE INDEX cycles_ticket_idx ON cycles(ticket_id);
CREATE INDEX cycles_inflight_idx ON cycles(ended_at) WHERE ended_at IS NULL;

CREATE TABLE events_new (
    seq        INTEGER PRIMARY KEY AUTOINCREMENT,
    ticket_id  TEXT    NOT NULL,
    cycle_id   TEXT    REFERENCES cycles(id),
    ts         INTEGER NOT NULL,
    kind       TEXT    NOT NULL,
    payload    TEXT    NOT NULL
) STRICT;

-- Carry the existing seq forward so external readers tailing events don't see
-- the counter rewind. Phase-1/2 only emit rows with cycle_id IS NULL, so the
-- CAST is a no-op in practice — kept for safety.
INSERT INTO events_new (seq, ticket_id, cycle_id, ts, kind, payload)
SELECT seq, ticket_id,
       CASE WHEN cycle_id IS NULL THEN NULL ELSE CAST(cycle_id AS TEXT) END,
       ts, kind, payload
FROM events;

DROP TABLE events;
ALTER TABLE events_new RENAME TO events;

CREATE INDEX events_ticket_seq_idx ON events(ticket_id, seq);

-- state_visits and subprocess_runs FK cycles(id); rebuild them so the FK
-- points at the new TEXT-typed parent.

CREATE TABLE state_visits_new (
    cycle_id  TEXT    NOT NULL REFERENCES cycles(id) ON DELETE CASCADE,
    state_id  TEXT    NOT NULL,
    visits    INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (cycle_id, state_id)
) STRICT;

INSERT INTO state_visits_new (cycle_id, state_id, visits)
SELECT CAST(cycle_id AS TEXT), state_id, visits FROM state_visits;

DROP TABLE state_visits;
ALTER TABLE state_visits_new RENAME TO state_visits;

CREATE TABLE subprocess_runs_new (
    cycle_id    TEXT    NOT NULL REFERENCES cycles(id) ON DELETE CASCADE,
    state_id    TEXT    NOT NULL,
    visit       INTEGER NOT NULL,
    started_at  INTEGER NOT NULL,
    ended_at    INTEGER,
    exit_code   INTEGER,
    capture_dir TEXT    NOT NULL,
    PRIMARY KEY (cycle_id, state_id, visit)
) STRICT;

INSERT INTO subprocess_runs_new (cycle_id, state_id, visit, started_at,
                                 ended_at, exit_code, capture_dir)
SELECT CAST(cycle_id AS TEXT), state_id, visit, started_at,
       ended_at, exit_code, capture_dir
FROM subprocess_runs;

DROP TABLE subprocess_runs;
ALTER TABLE subprocess_runs_new RENAME TO subprocess_runs;

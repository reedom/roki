-- roki-store schema v1
-- Single transaction; the migration runner wraps this file in BEGIN/COMMIT.

CREATE TABLE tickets (
    id            TEXT PRIMARY KEY,
    repo          TEXT NOT NULL,
    admitted_at   INTEGER NOT NULL,
    evicted_at    INTEGER
) STRICT;

CREATE INDEX tickets_admitted_idx
    ON tickets(evicted_at) WHERE evicted_at IS NULL;

CREATE TABLE cycles (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    ticket_id     TEXT    NOT NULL REFERENCES tickets(id),
    kind          TEXT    NOT NULL CHECK (kind IN ('rule','cleanup','failure')),
    entry_name    TEXT    NOT NULL,
    started_at    INTEGER NOT NULL,
    ended_at      INTEGER,
    outcome       TEXT    CHECK (outcome IN ('success','failure','no_action','cancelled')),
    current_state TEXT,
    iter          INTEGER NOT NULL DEFAULT 0
) STRICT;

CREATE INDEX cycles_ticket_idx ON cycles(ticket_id);
CREATE INDEX cycles_inflight_idx ON cycles(ended_at) WHERE ended_at IS NULL;

CREATE TABLE state_visits (
    cycle_id  INTEGER NOT NULL REFERENCES cycles(id) ON DELETE CASCADE,
    state_id  TEXT    NOT NULL,
    visits    INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (cycle_id, state_id)
) STRICT;

CREATE TABLE events (
    seq        INTEGER PRIMARY KEY AUTOINCREMENT,
    ticket_id  TEXT    NOT NULL,
    cycle_id   INTEGER REFERENCES cycles(id),
    ts         INTEGER NOT NULL,
    kind       TEXT    NOT NULL,
    payload    TEXT    NOT NULL  -- JSON; validated at application boundary
) STRICT;

CREATE INDEX events_ticket_seq_idx ON events(ticket_id, seq);

CREATE TABLE subprocess_runs (
    cycle_id    INTEGER NOT NULL REFERENCES cycles(id) ON DELETE CASCADE,
    state_id    TEXT    NOT NULL,
    visit       INTEGER NOT NULL,
    started_at  INTEGER NOT NULL,
    ended_at    INTEGER,
    exit_code   INTEGER,
    capture_dir TEXT    NOT NULL,
    PRIMARY KEY (cycle_id, state_id, visit)
) STRICT;

CREATE TABLE escalations (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    ticket_id   TEXT,
    reason      TEXT    NOT NULL,
    created_at  INTEGER NOT NULL,
    ack_at      INTEGER
) STRICT;

CREATE INDEX escalations_open_idx ON escalations(ack_at) WHERE ack_at IS NULL;

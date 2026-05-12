//! Embedded SQLite store for the roki daemon control plane.
//!
//! Owns the *queryable* slice of session data: cycle FSM, event log, admission
//! cache, escalation queue, subprocess-run registry. Subprocess captures
//! (stdout/stderr/sentinel) and git worktrees stay on the filesystem; this
//! crate stores only their pointers.
//!
//! Layout:
//! - [`Store`]  — object-safe trait the daemon depends on. Allows in-memory
//!   fakes in tests without dragging SQLite into every unit test.
//! - [`SqliteStore`] — the production implementation backed by a single
//!   `roki.db` file in WAL mode.
//! - [`migrations`] — embedded forward-only migration runner.
//! - [`models`] — plain data types crossing the trait boundary.

#![forbid(unsafe_code)]

pub mod error;
pub mod migrations;
pub mod models;
mod sqlite;
mod store;

pub use error::{Error, Result};
pub use sqlite::SqliteStore;
pub use store::Store;

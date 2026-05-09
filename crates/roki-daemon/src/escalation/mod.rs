//! Escalation queue (fr:06 §Escalation queue). In-memory bounded ring of
//! daemon-stuck failures. See `docs/superpowers/specs/2026-05-09-slice7-
//! escalation-queue-design.md`.

pub mod ring;

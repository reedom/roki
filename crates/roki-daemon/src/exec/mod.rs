//! External-tool shellout adapters used by the daemon.
//!
//! Boundary: every adapter in this module is **daemon-internal**. It must
//! never be reachable from a phase subprocess (agent shell-tool surface);
//! the orchestrator session and worktree manager call these directly via
//! Rust, not via spawned shell commands.
//!
//! Spec refs: requirements.md Req 4.6, 10.1.

pub mod ghq;
pub mod wt;

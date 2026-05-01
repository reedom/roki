//! Workspace path derivation.
//!
//! Pre-task-6.1 this module owned the sandbox-dir sanitization rules. Those
//! rules moved to [`crate::tools::wt::sanitize_branch`] and
//! [`crate::tools::wt::worktree_path_for`] when the daemon adopted the
//! worktree workspace model. The file is retained as a stub so the module
//! tree stays stable for any future helper that needs to live inside the
//! workspace boundary; today it has no callers.

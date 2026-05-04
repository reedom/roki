//! Workspace-level `WORKFLOW.md` policy loader.
//!
//! Splits responsibilities across four submodules to keep each file focused
//! and short:
//! - `parse`: front-matter (YAML/TOML) detection, Liquid render, named
//!   template-block extraction.
//! - `schema`: JSON-Schema validation of the parsed front matter, applies
//!   canonical defaults, refuses legacy keys, round-trips reserved unknowns.
//! - `watcher`: filesystem-watch + debounced re-validate; retains last-known
//!   good policy on failure.
//! - `render`: orchestrator and per-phase template render contexts plus the
//!   deterministic fallback orchestrator prompt.
//!
//! Spec refs: requirements.md Req 2.15, Req 6.1-6.7, Req 13.4; design.md File
//! Structure Plan workflow/.

pub mod parse;
pub mod render;
pub mod schema;
pub mod watcher;

//! `[[repos]]` allowlist entries.
//!
//! Per `docs/reference/config.md`, each entry is identified by its `ghq`
//! repository identifier (`owner/repo` or `host/owner/repo`). The local clone
//! path is resolved at runtime by `ghq list -p`; the loader only validates that
//! identifiers do not repeat.

use serde::Deserialize;

/// A single repository allowlist entry. The `ghq` field is the canonical
/// identifier; the daemon never stores the resolved local path here because
/// that path is resolved lazily and is environment-dependent.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RepoEntry {
    pub ghq: String,
}

/// Refuse duplicates per roki-mvp Req 2.2. Returns the offending identifier
/// alongside both originating zero-based indices so the loader can render an
/// actionable error naming both `[[repos]]` entries.
pub fn find_duplicate_ghq(entries: &[RepoEntry]) -> Option<DuplicateRepo> {
    for (i, entry) in entries.iter().enumerate() {
        if let Some((j, _)) = entries
            .iter()
            .enumerate()
            .skip(i + 1)
            .find(|(_, candidate)| candidate.ghq == entry.ghq)
        {
            return Some(DuplicateRepo {
                ghq: entry.ghq.clone(),
                first_index: i,
                second_index: j,
            });
        }
    }
    None
}

/// Detail for a duplicate `[[repos]].ghq` finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateRepo {
    pub ghq: String,
    pub first_index: usize,
    pub second_index: usize,
}

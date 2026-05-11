//! Resolve `visit-NNN` directory paths within a cycle directory.
//!
//! Supports absolute (`Some(3)`), negative (`Some(-1)` = latest,
//! `Some(-2)` = second-to-last), and `None` (= latest) addressing
//! against the on-disk layout under `session_root/<ticket>/<cycle>/`.

use std::path::{Path, PathBuf};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum VisitError {
    #[error("visit-{0:03} not found under {1:?}")]
    Missing(u32, PathBuf),
    #[error("relative iter {0} past the start of the cycle (only {1} visit(s))")]
    OffStart(i32, usize),
    #[error("cycle directory {0:?} contains no visit-NNN entries")]
    Empty(PathBuf),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Enumerate every `visit-NNN` subdirectory of `cycle_dir`, returning
/// their numeric suffixes sorted ascending. Non-matching entries
/// (files, oddly named dirs) are silently skipped.
pub fn list_visits(cycle_dir: &Path) -> Result<Vec<u32>, VisitError> {
    let mut out: Vec<u32> = Vec::new();
    for entry in std::fs::read_dir(cycle_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if let Some(name) = entry.file_name().to_str()
            && let Some(rest) = name.strip_prefix("visit-")
            && let Ok(n) = rest.parse::<u32>()
        {
            out.push(n);
        }
    }
    out.sort_unstable();
    Ok(out)
}

/// Resolve a CLI iter argument against the visits present in
/// `cycle_dir`. `None` and `Some(n<0)` are relative addressing.
pub fn resolve_iter(cycle_dir: &Path, iter: Option<i32>) -> Result<u32, VisitError> {
    let visits = list_visits(cycle_dir)?;
    if visits.is_empty() {
        return Err(VisitError::Empty(cycle_dir.to_path_buf()));
    }
    match iter {
        None => Ok(*visits.last().unwrap()),
        Some(n) if n > 0 => {
            let n = n as u32;
            if visits.contains(&n) {
                Ok(n)
            } else {
                Err(VisitError::Missing(n, cycle_dir.to_path_buf()))
            }
        }
        Some(n) if n < 0 => {
            let back = (-n) as usize;
            if back > visits.len() {
                Err(VisitError::OffStart(n, visits.len()))
            } else {
                Ok(visits[visits.len() - back])
            }
        }
        Some(_) => Err(VisitError::Missing(0, cycle_dir.to_path_buf())),
    }
}

/// Build the on-disk path for `visit-NNN` (zero-padded to 3 digits).
pub fn visit_dir(cycle_dir: &Path, n: u32) -> PathBuf {
    cycle_dir.join(format!("visit-{n:03}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_cycle(visits: &[u32]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for n in visits {
            std::fs::create_dir_all(dir.path().join(format!("visit-{n:03}"))).unwrap();
        }
        dir
    }

    #[test]
    fn lists_visits_sorted_ascending() {
        let d = fixture_cycle(&[3, 1, 2]);
        let v = list_visits(d.path()).unwrap();
        assert_eq!(v, vec![1, 2, 3]);
    }

    #[test]
    fn resolve_absolute_iter() {
        let d = fixture_cycle(&[1, 2, 3]);
        assert_eq!(resolve_iter(d.path(), Some(2)).unwrap(), 2);
    }

    #[test]
    fn resolve_negative_iter_takes_n_back_from_last() {
        let d = fixture_cycle(&[1, 2, 3]);
        assert_eq!(resolve_iter(d.path(), Some(-1)).unwrap(), 3);
        assert_eq!(resolve_iter(d.path(), Some(-2)).unwrap(), 2);
    }

    #[test]
    fn resolve_iter_off_the_start_errors() {
        let d = fixture_cycle(&[1, 2]);
        assert!(resolve_iter(d.path(), Some(-5)).is_err());
    }

    #[test]
    fn resolve_iter_none_returns_latest() {
        let d = fixture_cycle(&[5, 1, 9]);
        assert_eq!(resolve_iter(d.path(), None).unwrap(), 9);
    }

    #[test]
    fn missing_absolute_iter_errors() {
        let d = fixture_cycle(&[1, 2]);
        assert!(resolve_iter(d.path(), Some(7)).is_err());
    }
}

//! Startup PATH probe for required external CLIs.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingDependency {
    pub binary: &'static str,
    pub hint: &'static str,
}

const REQUIRED: &[(&str, &str)] = &[
    ("wt", "install worktree manager 'wt' and put it on PATH"),
    (
        "ghq",
        "install ghq (https://github.com/x-motemen/ghq) and put it on PATH",
    ),
];

pub fn check() -> Result<(), Vec<MissingDependency>> {
    check_with(|bin| which::which(bin).is_ok())
}

fn check_with<F: Fn(&str) -> bool>(found: F) -> Result<(), Vec<MissingDependency>> {
    let missing: Vec<MissingDependency> = REQUIRED
        .iter()
        .copied()
        .filter(|(bin, _)| !found(bin))
        .map(|(binary, hint)| MissingDependency { binary, hint })
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(missing)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_present_returns_ok() {
        assert!(check_with(|_| true).is_ok());
    }

    #[test]
    fn only_wt_missing() {
        let res = check_with(|bin| bin != "wt");
        let missing = res.unwrap_err();
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].binary, "wt");
        assert!(!missing[0].hint.is_empty());
    }

    #[test]
    fn only_ghq_missing() {
        let res = check_with(|bin| bin != "ghq");
        let missing = res.unwrap_err();
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].binary, "ghq");
    }

    #[test]
    fn both_missing_lists_both_in_order() {
        let res = check_with(|_| false);
        let missing = res.unwrap_err();
        assert_eq!(missing.len(), 2);
        assert_eq!(missing[0].binary, "wt");
        assert_eq!(missing[1].binary, "ghq");
    }
}

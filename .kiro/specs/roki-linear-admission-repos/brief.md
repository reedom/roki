---
refs:
  id: brief:roki-linear-admission-repos
  kind: brief
  title: "roki-linear-admission-repos Brief"
  spec: roki-linear-admission-repos
---

# Brief: roki-linear-admission-repos

## Problem

Skeleton resolves repo from the first `[[admission.repos]]` entry only; production needs the full matcher set.

## Desired Outcome

`[[admission.repos]]` first-match against the design §3.5 condition vocabulary scoped to admission: `when.labels.has_all|has_any|has_none`, `when.title.regex|starts_with|contains`, `when.body.*`, plus a fallback no-`when` entry. Resolves the ticket's repo and optional per-repo workflow path. No match = silent eviction logged as `repo_unresolvable`.

## Scope

- **In**: condition vocabulary at admission scope; first-match semantics; AND-within-entry, OR-via-entries.
- **Out**: per-repo TOML body (`roki-config-per-repo-toml`); rule-side `when.repo` matchers (lives with rule eval).

## Dependencies

- roki-skeleton

## Critical FR references

- fr:03-linear-admission

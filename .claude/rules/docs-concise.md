---
paths: **/*.md
---

# Docs concise

## Rules

1. **Write concisely.** Shortest wording that conveys the rule, contract, or decision. No filler, no restating the obvious, no scene-setting.
2. **Background only when needed.** Include rationale, history, or motivation only when a reader cannot apply the rule correctly without it. Default: omit.

## When background IS needed

- Non-obvious constraint (regulatory, perf budget, prior incident).
- Counter-intuitive choice that future edits will second-guess.
- Decision among competing alternatives that future readers will want to revisit.

## When background is NOT needed

- Restating what the rule itself already says.
- Explaining standard project conventions readers can find elsewhere.
- Narrating how the doc was authored or what it replaces.

## Spec / design / FR docs

1. **Facts and decisions only.** State the current contract. No historical narration ("previously", "used to", "was renamed from", "after the rewrite"), no comparison to prior designs, no migration commentary.
2. **Out of scope: only when load-bearing.** Include an out-of-scope item only when a reader would otherwise assume it is in scope. Drop the rest.

## Test

Cut every sentence. If the rule still applies correctly without it, leave it cut.

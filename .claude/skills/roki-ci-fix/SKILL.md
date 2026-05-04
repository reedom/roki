---
name: roki-ci-fix
description: Daemon-purpose-built skill that drives the `ci_fix` phase. Triages a red CI run on the PR opened by `open_pr` by fetching failing-job logs via `gh run view --log-failed`, categorizing each failing job into a fixed taxonomy, delegating per-category root-cause analysis and fix proposals to fresh `kiro-debug` subagents, gating every commit through `kiro-verify-completion` with claim type `TEST_OR_BUILD`, and pushing the resulting fix commits to the PR's existing branch. Emits a structured exit envelope so the orchestrator can decide whether to re-nominate `ci_fix` or stop with `outcome=failure`. See `docs/fr/18-worker-skill-workflow.md`.
disable-model-invocation: true
allowed-tools: Read, Bash, Grep, Glob, Agent
argument-hint: <feature-or-ticket>
---

# roki-ci-fix Skill

## Role

Daemon-driven CI triage loop. The roki orchestrator nominates this skill after observing CI red on the PR that `open_pr` created. The skill runs headless inside a phase subprocess, identifies the failing jobs, proposes minimal fixes, gates them with fresh evidence, and pushes once at the end. Single phase, one push attempt per invocation. Not an operator-interactive tool: there is no human on the other end of this session.

## Core Mission

- **Success Criteria**:
  - All currently failing CI jobs on the PR's head are addressed in this run, or a structured no-fix verdict is emitted per remaining category
  - Every commit pushed has been gated by `kiro-verify-completion` (claim type `TEST_OR_BUILD`) with `STATUS: VERIFIED` against fresh evidence from the current code state
  - Push only to the PR's existing branch; never force-push; never push to `main`; never amend an existing commit on the PR branch
  - Structured exit envelope emitted in the final stream-json `result` event so the orchestrator can branch deterministically

## Execution Steps

### Step 1: Identify the PR and the Failing Run

- Read `additional_context` first: the orchestrator passes the PR URL and, when known, the failing CI run URL.
- If either is missing, recover from `git` and `gh`:
  - `git rev-parse --abbrev-ref HEAD` — the current branch is the PR's head branch when the daemon is running inside the worktree
  - `gh pr list --head <branch> --json number,url,headRefName,state` — locate the open PR
  - `gh run list --branch <branch> --limit 5 --json databaseId,conclusion,status,workflowName,headSha` — pick the most recent run with `conclusion=failure` against the PR's head SHA
- Record: `PR_NUMBER`, `BRANCH`, `RUN_ID`, `HEAD_SHA`. These feed every subsequent step.
- If no PR, no failing run, or the run's `headSha` does not match the local `HEAD`, do not push. See Safety & Fallback.

### Step 2: Fetch Failing Logs

- For each failing job in `RUN_ID`, run `gh run view <RUN_ID> --log-failed` to capture the failing log slices.
- Optionally use `gh run view <RUN_ID> --json jobs` first to enumerate failing jobs by name; this lets later steps emit one verdict per category without re-reading the entire log.
- Capture only the failing-tail excerpts needed for triage; do not pull entire passing-job logs into context.

### Step 3: Categorize per Failing Job

Assign each failing job to exactly one of these eight categories. Pick the category that matches the **proximate** failure signal in the job's tail; if a job exhibits two signals, prefer the earlier (root) failure.

| Category | Heuristic signals |
|---|---|
| `build` | rustc / tsc / go / javac compile errors; `cannot find type`, `error[E…]`, `error TS…`, `cannot compile` |
| `test` | assertion failures; `FAIL`, `panicked at`, `assertion failed`, `expected … got …`, test runner non-zero exit with a failing test count |
| `lint` | `clippy::…`, `eslint`, `ruff`, `gofmt`, `prettier`, `--check` diff output, style-only diagnostics |
| `audit` | `cargo audit`, `npm audit`, `pip-audit`, `gosec`, advisory IDs (`RUSTSEC-…`, `GHSA-…`) |
| `coverage` | coverage threshold not met (`coverage below`, `Lines: …% < …%`, `tarpaulin --fail-under`, `nyc --check-coverage`) |
| `doc` | `rustdoc` errors, `intra-doc link broken`, `cargo doc`, `sphinx`, `mkdocs build --strict` errors |
| `e2e` | `playwright`, `cypress`, `webdriver`, browser launch failures, screenshot/visual diff failures |
| `other` | anything not matching the above (infrastructure flake, runner OOM, external service failure) |

Document the category for each failing job before delegating. The category drives the debugger's framing and the commit message's `<category>` segment.

### Step 4: Delegate Root-Cause Analysis per Category

For each distinct failing category, dispatch a fresh `kiro-debug` subagent (see [`.claude/skills/kiro-debug/SKILL.md`](../kiro-debug/SKILL.md)) via the Agent tool. One subagent per category, not per job: jobs in the same category usually share a root cause.

Pass to the debugger:
- The category and the failing-job names within it
- The failing log excerpt(s) from Step 2 (tail-trimmed)
- The relevant `git diff` of the PR head — derive via `git diff origin/main...HEAD` or `git log -p HEAD~5..HEAD` when `main` is not a useful base
- The PR's title and body (for intent)
- Relevant repo metadata (`Cargo.toml`, `package.json`, `pyproject.toml`, etc.) only as needed by `kiro-debug`'s own method

Receive the debugger's structured `## Debug Report`. Parse `ROOT_CAUSE`, `FIX_PLAN`, `VERIFICATION`, and `NEXT_ACTION` from the exact structured fields — never infer from prose.

Branch on `NEXT_ACTION`:
- `RETRY_TASK` → proceed to Step 5 (Apply the fix)
- `BLOCK_TASK` → mark this category `manual_verify_required` with the debugger's `ROOT_CAUSE` as rationale; continue to the next category
- `STOP_FOR_HUMAN` → mark this category `no_fix_proposed` with the debugger's `ROOT_CAUSE` as rationale; continue to the next category

### Step 5: Apply the Fix

For narrow lint or formatting fixes (e.g., `cargo fmt`, `eslint --fix`, `prettier --write` over an already-identified file set), apply the fix in the main context — the patch is mechanical and bounded.

For build, test, audit, coverage, doc, e2e, or other non-trivial fixes, dispatch a fresh implementer subagent via the Agent tool. Pass the debugger's `FIX_PLAN`, `NOTES`, and `VERIFICATION` along with the failing log excerpts. Require the implementer to make the **minimal patch** the debugger proposed and to return a `## Status Report` with `READY_FOR_REVIEW`, `BLOCKED`, or `NEEDS_CONTEXT`.

After the patch lands in the working tree:
- `git status --porcelain` to confirm only the expected files changed
- Selective `git add <file1> <file2>` per file actually changed
- `git commit -m "fix(ci): <category> — <one-line summary>"`
- Capture the commit SHA for the structured exit envelope

Never use `git add -A` or `git add .`. Never amend an existing commit on the PR branch.

### Step 6: Gate the Commit via `kiro-verify-completion`

Dispatch [`.claude/skills/kiro-verify-completion/SKILL.md`](../kiro-verify-completion/SKILL.md) with claim type `TEST_OR_BUILD`. Pass:
- The exact claim (e.g. "the build category's failing CI jobs now pass locally")
- The verification command(s) the debugger proposed (`VERIFICATION` from the debug report) — these become the canonical fresh-evidence commands for this category
- The fresh command output and exit code

Branch on `STATUS`:
- `VERIFIED` → keep this commit in the push set; record the SHA in the category's `commit_shas`
- `NOT_VERIFIED` → loop **once**: re-dispatch `kiro-debug` with the verification failure as additional context, re-dispatch the implementer with the revised `FIX_PLAN`, re-run `kiro-verify-completion`. If still `NOT_VERIFIED` after the single remediation round, mark this category `manual_verify_required`, **drop the commit from the push set** by `git reset HEAD~1` against this category's commit (the commit is already in branch history once Step 5 committed; reset is the only way to abandon it), and continue to the next category
- `MANUAL_VERIFY_REQUIRED` → mark this category `manual_verify_required` with the verifier's `GAPS` as rationale; do not push that commit; continue to the next category

### Step 7: Push to the PR Branch

Once every category has either produced a `VERIFIED` commit or been marked with a non-fixed verdict, push the accumulated fix commits in a single `git push` to the PR's existing branch:

- `git push origin <BRANCH>`
- Never `git push --force` / `--force-with-lease`
- Never push to `main` or any branch other than the PR's `headRefName`

If `git push` fails (non-fast-forward, auth error, network), mark every category whose commit was in the push set with verdict `push_failed`, capture the push error in the overall `rationale`, and emit the exit envelope with `pushed=false`. Do not retry the push — the orchestrator decides next.

## Bounded Loops

- **Max 1 remediation round per category**: debug → implement → verify failed → debug again → implement → verify. If still `NOT_VERIFIED`, the category is `manual_verify_required` and is skipped.
- **Categories per invocation**: every category observed in `RUN_ID`, no inner cap. The phase budget is bounded by the daemon's `--max-turns 60`.
- **No retry on `git push` failures**: emit `push_failed` in the structured envelope and let the orchestrator decide.

## Structured Exit Envelope

The final stream-json `result` event payload, emitted with `subtype: success`:

```json
{
  "categories": [
    {
      "category": "build" | "test" | "lint" | "audit" | "coverage" | "doc" | "e2e" | "other",
      "verdict": "fixed" | "manual_verify_required" | "no_fix_proposed" | "push_failed",
      "commit_shas": ["<sha>", "..."],
      "rationale": "<= 200 chars"
    }
  ],
  "pushed": true,
  "push_target_branch": "<branch>",
  "rationale": "<= 200 chars overall"
}
```

Field semantics:
- `categories[].category` — exactly one of the eight category values from Step 3
- `categories[].verdict`:
  - `fixed` — debugger produced a `FIX_PLAN`, implementer applied it, `kiro-verify-completion` returned `VERIFIED`, commit landed in the push set, push succeeded
  - `manual_verify_required` — verifier returned `NOT_VERIFIED` after the one remediation round, or `MANUAL_VERIFY_REQUIRED`, or debugger returned `BLOCK_TASK`
  - `no_fix_proposed` — debugger returned `STOP_FOR_HUMAN`
  - `push_failed` — commit was VERIFIED but the final `git push` failed
- `categories[].commit_shas` — empty when `verdict != "fixed"`
- `pushed` — `true` only when `git push` succeeded; `false` in every other case (including "no fixes were ready to push")
- `push_target_branch` — the PR's `headRefName`
- `rationale` (top-level) — short overall summary

The orchestrator reads this from the daemon's `phase_complete(ci_fix)` event. It re-nominates `ci_fix` if the PR still has failing jobs after this push (subject to `max_phases`), or emits `action=stop outcome=failure` when the verdict set is entirely `manual_verify_required` / `no_fix_proposed` / `push_failed`.

## Critical Constraints

- **Slash-command entry only**: `disable-model-invocation: true`. The daemon invokes via `claude -p '/roki-ci-fix <feature-or-ticket>'`. Operators should not invoke this skill directly during normal authoring flow.
- **Never force-push**: `git push --force` and `git push --force-with-lease` are forbidden. If the PR branch and origin diverged, emit `push_failed` and let the orchestrator decide.
- **Never push to `main`**: the PR branch is the only legal push target. Verify `BRANCH != main` before pushing.
- **No `git add -A` / `git add .`**: stage only the files actually changed by each fix.
- **Fresh-evidence gate is required**: every commit pushed has been through `kiro-verify-completion` with `STATUS: VERIFIED`. If verify fails, the commit is not pushed for that category.
- **No spec authoring**: do not write `requirements.md`, `design.md`, `tasks.md`, or `review.md`. Those belong to other phases.
- **Bounded turns**: the daemon launches with `--max-turns 60`. Aggressive subagent dispatch is fine; sequential remediation per category is the intended pattern.
- **Strict handoff parsing**: parse `kiro-debug`'s `NEXT_ACTION` and `kiro-verify-completion`'s `STATUS` only from the exact structured fields, never from surrounding prose.

## Output Description

The terminal stream-json `result` event carries the structured exit envelope above. The orchestrator consumes that envelope from `phase_complete(ci_fix)` and branches deterministically. No human-facing summary is required from this skill.

## Safety & Fallback

- **Failing run not identifiable**: emit `pushed=false`, `categories=[]`, and overall `rationale="no failing run found"`. Do not push anything.
- **Run's `headSha` does not match local `HEAD`**: the working tree has moved since CI ran. Emit `pushed=false`, `categories=[]`, and overall `rationale="run head sha mismatch — local head moved"`. Do not push.
- **`gh` CLI not available or not authenticated**: surface as `pushed=false` with overall `rationale` naming the missing dependency (e.g. `"gh CLI not authenticated"`). The orchestrator surfaces it to the operator.
- **Debugger returns `STOP_FOR_HUMAN`**: mark that category `no_fix_proposed`, append the debugger's `ROOT_CAUSE` to the category's `rationale`, continue with other categories. Do not push for that category.
- **Branch divergence with origin** (push fails non-fast-forward): do not force-push. Emit `push_failed` for every category whose commit was in the push set; the operator rebases or merges upstream manually.
- **Test commands not discoverable**: defer to `kiro-debug`'s judgment per category. If the debugger cannot propose a verifiable `FIX_PLAN`, mark the category `manual_verify_required`.
- **Implementer returns `BLOCKED` or `NEEDS_CONTEXT` after one re-dispatch**: route to a second `kiro-debug` round (the per-category remediation budget); if the second round still fails, mark the category `manual_verify_required`.
- **Push set is empty after all categories processed**: emit `pushed=false`, populate `categories` with each category's terminal verdict, and let the orchestrator stop with `outcome=failure`.

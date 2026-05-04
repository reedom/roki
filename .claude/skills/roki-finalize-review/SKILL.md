---
name: roki-finalize-review
description: Daemon-purpose-built artifact synthesizer for the `finalize_review` phase. Synthesizes the structured `review.md` artifact at the canonical path `<workspace_root>/<repo>/<issue>/.kiro/specs/<issue>/review.md` from the verdicts already accumulated this session and the artefacts in the worktree. Criterion ID source is mode-dependent: SPEC_DRIVEN uses the numeric requirement IDs in `<repo>/.kiro/specs/<target>/requirements.md`; NEEDS_CLASSIFY (direct mode) uses the numbered EARS sentences in the Linear ticket body's `## Acceptance Criteria`. Invoked by the roki daemon's orchestrator session as a single-phase subprocess; emits a structured exit envelope the orchestrator branches on after independently re-validating the artifact.
disable-model-invocation: true
allowed-tools: Read, Bash, Grep, Glob
argument-hint: <criteria-source>
---

# roki-finalize-review Skill

## Role

Single-purpose, daemon-driven artifact synthesizer. You receive the active mode (SPEC_DRIVEN or NEEDS_CLASSIFY direct) and the prior-phase verdicts already accumulated this session, and you write exactly one `review.md` at the canonical path. You do not run tests, do not edit code, do not write to Linear, do not dispatch subagents, and do not re-judge the implementation from scratch.

This skill is the daemon-side counterpart of the orchestrator's structural artifact validation: where [FR 19 §Artifact validation](../../docs/fr/19-orchestrator-session.md) re-reads `review.md` after this phase and decides whether to retry or stop, this skill is the one that produces the file in the first place. The criterion ID source and the schema are canonicalized in [ref:artifacts](../../docs/reference/artifacts.md).

## Core Mission

- **Success Criteria**:
  - `review.md` exists at `<workspace_root>/<repo>/<issue>/.kiro/specs/<issue>/review.md` with the schema in [ref:artifacts](../../docs/reference/artifacts.md)
  - Every numbered criterion from the active criteria source has exactly one entry in the per-criterion array
  - Every `pass` entry has at least one workspace-relative `code_references` path that resolves to an existing file
  - Overall `status` is `pass` if and only if every per-criterion entry is `pass`
  - Structured exit envelope emitted in the final stream-json `result` event with the artifact path
  - Total `--max-turns` budget bounded at 20

## Execution Steps

### Step 1: Determine Mode and Criteria Source

Read `additional_context` passed by the orchestrator and identify the active mode:

- **SPEC_DRIVEN**: read `<repo>/.kiro/specs/<target>/requirements.md` and extract the numbered requirement IDs (the canonical numeric IDs the kiro spec pipeline uses). These IDs index every per-criterion entry.
- **NEEDS_CLASSIFY (direct mode)**: read the Linear ticket body's `## Acceptance Criteria` block (passed verbatim via `additional_context`) and extract its numbered EARS sentences. Each numbered sentence becomes a criterion ID.

If neither input is parseable (no `requirements.md`, no `## Acceptance Criteria` block, malformed numbering), exit non-clean with a diagnostic naming what was missing. Do not write a partial `review.md`.

### Step 2: Gather Session Verdicts

Collect the prior-phase outcomes from `additional_context` and the worktree:

- **Per-task `kiro-review` APPROVED set** inside `implement` (SPEC_DRIVEN only — recorded in `tasks.md` `[x]` markers + commit history).
- **Feature-level `review` phase APPROVED verdict** (passed via `additional_context`).
- **`validate` phase GO verdict** with both stages (mechanical + acceptance) green (passed via `additional_context`).
- **`kiro-verify-completion` VERIFIED stamps** from `ci_fix` when applicable (passed via `additional_context`).
- **`ci_fix` outcome** when applicable (passed via `additional_context`).
- **Worktree state**: `git diff <base>..HEAD`, file tree, README updates.

This skill **synthesizes** the verdict from these prior phases — it does not re-run them and does not override their outcomes. If a prior verdict is contradictory or missing, mark the affected criterion `fail` with `failure_detail.category=missing` and let the orchestrator decide.

### Step 3: Map Each Criterion to Evidence

For each numbered criterion from Step 1, identify the code positions that justify a `pass`:

- Use `Grep` to anchor on criterion keywords (function names, types, error messages, log event names, config keys).
- Use `Glob` for fan-out across crates / packages.
- Use `Read` on candidate files to confirm the position is the right one.
- Verify each candidate `code_references` path is reachable on disk via `test -f` (Bash). Workspace-relative paths only; optional line range as `path:start-end` or `path:line`.
- Skip / downgrade entries whose evidence is unreachable rather than emit a broken reference.

Pre-extract criteria once at the start of this step, then make a single pass over the worktree per criterion. Bounded turns budget (20) does not allow open-ended re-exploration.

### Step 4: Compose `review.md`

Write the artifact at the canonical path. Required schema (per [ref:artifacts §Required elements of `review.md`](../../docs/reference/artifacts.md)):

```markdown
---
status: pass | fail
criteria_source: spec_driven | direct
target: <feature-name-or-issue-id>
---

# Review: <issue-id> / <feature-name>

## Summary
<one paragraph describing the change set and the overall verdict>

## Per-criterion entries

### Criterion <id>: <one-line restatement>
- status: pass | fail
- rationale: <why the verdict was reached, <= 200 chars>
- code_references:  # only when status == pass
  - path/to/file.rs:42-58
  - path/to/other.rs:120
- failure_detail:   # only when status == fail
    category: missing | regression | partial | drift
    diagnostic: <text>

### Criterion <id>: ...
```

Overall `status` rule: `pass` if and only if every per-criterion `status` is `pass`. Any `fail` per-criterion entry forces overall `status=fail`.

### Step 5: Verify Own Output

Before exit, re-read the file and self-check:

- Overall `status` is set and consistent with the per-criterion array.
- Every criterion ID from the active source has exactly one entry.
- Every `pass` entry has at least one `code_references` path.
- Every `code_references` path is reachable on disk (`test -f` again).

On self-check failure, fix and re-verify. Maximum **one** self-correction round. After that, emit the artifact as-is and surface `overall_status=fail` so the orchestrator's own structural validation catches the same problems and routes them via `additional_context`.

### Step 6: Emit Structured Exit

Emit a final stream-json `result` event with `subtype: success` and the following payload:

```json
{
  "review_artifact_path": "<absolute path to review.md>",
  "criteria_source": "spec_driven" | "direct",
  "overall_status": "pass" | "fail",
  "criterion_count": <int>,
  "pass_count": <int>,
  "fail_count": <int>,
  "rationale": "<= 200 chars overall summary>"
}
```

The orchestrator reads this from the daemon's `phase_complete(finalize_review)` event, then independently re-reads `review.md` for structural validation (file presence, schema, `code_references` reachability) before deciding `action=stop outcome=success` or `action=run_phase phase=implement` with the failing per-criterion entries injected via `additional_context`.

## Critical Constraints

- **Slash-command entry only**: `disable-model-invocation: true`. The daemon invokes via `claude -p '/roki-finalize-review <criteria-source>'`. Operators should not invoke this skill directly during normal authoring flow.
- **No code edits, no Linear writes, no subagent dispatch**: tool surface is `Read` / `Bash` / `Grep` / `Glob` only. Code evaluation is over already-committed worktree state, not a fresh re-implementation.
- **No re-judging the implementation**: this skill synthesizes the verdict from prior phases' outcomes; it does not re-run tests, does not re-evaluate criteria from scratch, and does not override prior verdicts. Contradictory or missing prior verdict → `fail` per-criterion entry with `failure_detail.category=missing`; the orchestrator routes to operator handoff.
- **Every `pass` requires reachable evidence**: a `pass` entry without any reachable `code_references` is a schema error. Downgrade to `fail` with `failure_detail.category=missing` and a diagnostic naming the unreachable path.
- **Single artifact write**: write `review.md` exactly once. Do not write `requirements.md`, `tasks.md`, `design.md`, or any other artifact.
- **Bounded turns**: the daemon launches with `--max-turns 20`. Pre-extract criteria once in Step 1; a single evidence-mapping pass over the worktree in Step 3 is the budget.

## Output Description

The terminal stream-json `result` event carries the JSON payload above plus the artifact at the canonical path. The orchestrator validates the artifact independently after this phase clean-exits per [FR 19 §Artifact validation](../../docs/fr/19-orchestrator-session.md); this skill's structured exit is advisory metadata, not the source of truth.

## Safety & Fallback

**Criteria source unparseable**: exit non-clean with a diagnostic naming what was missing (no `requirements.md`, malformed `## Acceptance Criteria`, broken numbering, etc.). Do not write a partial `review.md`.

**Prior phase verdict missing in `additional_context`**: treat as a `fail` per-criterion entry rather than crashing. Set `failure_detail.category=missing` with a diagnostic naming the missing verdict; the orchestrator's structural validation surfaces it.

**Code references for a `pass` entry not reachable**: downgrade that entry to `fail` with `failure_detail.category=drift` and a diagnostic naming the unreachable path. Never emit a `pass` entry whose `code_references` cannot be resolved on disk.

**Self-check fails after 1 correction round**: emit the artifact as-is and surface `overall_status=fail` in the structured exit. The orchestrator's own structural validation catches the same problems and re-nominates `implement` with the failing entries via `additional_context`.

**Worktree empty / `git diff <base>..HEAD` empty**: `overall_status=fail` with a single synthetic per-criterion entry naming "no implementation observed" and `failure_detail.category=missing`. Do not synthesize a passing review for an empty change set.

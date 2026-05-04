---
refs:
  id: fr:18-worker-skill-workflow
  kind: fr
  title: "Worker Skill Workflow"
  related:
    - fr:07-worker-execution
    - fr:08-pre-implementation-gate
    - fr:09-pre-pr-gate
    - fr:11-agent-tool-boundary
---

# FR 18: Worker Skill Workflow

> The internal workflow the agent-side kiro skill runs inside one bounded `claude --print --output-format stream-json` worker invocation: phase plan, single-step subagent catalog, loop budgets, the `review.md` artifact produced before clean exit, and the structured terminal contract the daemon parses back.

## Purpose

[FR 07: Worker Execution](07-worker-execution.md) deliberately keeps the daemon a thin observer that only reacts to subprocess exit, the artifacts left in the session tempdir, and Linear state. The actual code-changing work — implement, self-review, fix, lint/test, open PR, fix CI, and finally write a `review.md` artifact — happens **inside one Claude Code session**, driven by a kiro skill the operator has installed under `~/.claude/skills/kiro-*`. That session is structurally better at iterating with full context than a daemon dispatching N short-lived subprocesses with `format!`-built prompts ever could be.

This FR specifies the **expected shape of that internal workflow**: what phases the skill runs, what subagents it dispatches via the `Agent` tool, what budgets it honors, and what it must emit on stdout / leave on disk so the daemon can derive a terminal outcome and the review-gate can evaluate it without looking at agent message contents (per [FR 07: Worker Execution](07-worker-execution.md) §Boundaries).

It is the agent-side counterpart to FR 07 — together they describe one worker invocation — and the inside-the-session counterpart to the daemon-enforced gates in [FR 08: Pre-Implementation Gate](08-pre-implementation-gate.md) and [FR 09: Pre-PR Gate](09-pre-pr-gate.md).

## User-visible Behavior

### Phase plan (single bounded invocation)

The skill executes the following phases sequentially within one worker subprocess. Phases are skill-internal; the daemon does not observe them granularly. The pre-implementation gate ([FR 08](08-pre-implementation-gate.md)) runs **before** this invocation starts (on the `Judging → Active` transition); the pre-PR gate ([FR 09](09-pre-pr-gate.md)) runs **after** this invocation exits cleanly (on the `Active → Inactive` transition). The skill does not self-check those gates — the daemon side owns the veto.

1. **Context load** — read the ticket description (with `## Acceptance Criteria` per §Acceptance criteria convention), the relevant `requirements.md` / `design.md` / `tasks.md`, and the project's `WORKFLOW.md`. Discover the verify command (see §Verify-cmd discovery).
2. **Implement** — dispatch `kiro-implement` subagent: TDD-first (tests then code), one sub-task per dispatch when working from `tasks.md`. Returns a summary diff.
3. **Self-review loop (max 5)** — dispatch `kiro-self-review` to enumerate findings, then `kiro-fix-finding` per actionable finding. Repeat until a pass produces no applied fix, or the budget is exhausted.
4. **Lint/test loop (max 5)** — dispatch `kiro-lint-test` to run the discovered verify command and fix failures internally. Returns `green | red`. Escalate if `red` after the budget.
5. **Open PR** — dispatch `kiro-open-pr`. The PR description embeds a brief change summary (one paragraph + the verify-command outcome).
6. **CI fix loop (max 3)** — dispatch `kiro-ci-fix`. Polls GitHub Actions until checks settle, fixes failures, pushes. Escalate if still red after the budget; the PR remains open per [FR 07: Worker Execution](07-worker-execution.md).
7. **Produce `review.md`** — dispatch `kiro-review` (the skill packaged as `.claude/skills/kiro-review/`). Reads the PR diff, the ticket's `## Acceptance Criteria`, the verify-command outcome, and writes a structured `review.md` to the session tempdir's documented review-artifact path ([ref:artifacts](../reference/artifacts.md)). Each criterion gets one entry with `code_evidence` (file/line) and `test_evidence` (file/line) plus an overall verdict. The pre-PR gate ([FR 09](09-pre-pr-gate.md)) parses this artifact structurally; if it is missing or malformed the gate denies the transition.
8. **Clean exit** — emit a final `ROKI_RESULT:` line on stdout (see §Skill → daemon terminal contract) and exit 0.

A Type B (with-human-planning) ticket inserts one phase before Phase 2:

- **Plan with human** — dispatch `kiro-plan-with-human`. Uses the operator-installed Linear MCP (per [FR 11: Agent Tool Boundary](11-agent-tool-boundary.md)) to post questions as ticket comments and poll for replies. When the human approves a plan, write it to the ticket body as a `## kiro Plan` YAML section, then proceed.

### Subagent catalog

Each subagent is a single-step worker dispatched via the `Agent` tool. Each has a tight system prompt and tool scope; subagents do **not** know about each other — the skill is the only coordinator. Fresh context per invocation.

| Subagent | Inputs | Output | Tool scope |
|---|---|---|---|
| `kiro-implement` | worktree path, ticket key, sub-task or instructions | summary text | Read, Edit, Write, Bash, Grep, Glob |
| `kiro-self-review` | worktree path, ticket key | JSON list of findings (id, file, line, severity, message) | Read, Grep, Bash(`git diff`) |
| `kiro-fix-finding` | worktree path, finding object | `{ applied: bool, reason: string }` | Read, Edit, Write, Bash |
| `kiro-lint-test` | worktree path, prior failure log (optional) | `{ outcome: "green" \| "red", log: string }` | Read, Edit, Write, Bash |
| `kiro-open-pr` | worktree path, ticket key, summary | `{ pr_url: string }` | Bash(`gh pr create`, `git push`) |
| `kiro-ci-fix` | worktree path, ticket key, pr_url | `{ outcome: "green" \| "red", attempts: int }` | Read, Edit, Write, Bash(`gh`) |
| `kiro-review` | worktree path, ticket key, pr_url, verify-cmd outcome | writes `review.md`; returns `{ artifact_path, all_satisfied }` | Read, Grep, Bash(`git diff`), Linear MCP (read-only) |
| `kiro-plan-with-human` | ticket key | `{ plan_yaml: string, approved: bool }` | Read, Bash, Linear MCP tools |

### Acceptance criteria convention (EARS)

Every Linear ticket eligible for roki dispatch MUST include an `## Acceptance Criteria` section in its body, written in EARS style — preferably the Ubiquitous (`The X shall Y.`) and Event-driven (`When <trigger>, the X shall Y.`) patterns. Without explicit, written criteria, there is no objective basis for `kiro-review` to populate the per-criterion entries in `review.md`; the setup judge ([FR 05: Setup Judge](05-setup-judge.md)) is expected to refuse such tickets at admission.

### `review.md` artifact contract

`kiro-review` writes one Markdown file with YAML front matter at the path documented in [ref:artifacts](../reference/artifacts.md). Front-matter keys (the structurally-parsed surface):

```yaml
---
schema: roki-review/v1
verdict: pass | fail
verify_cmd_outcome: green | red
all_criteria_satisfied: true | false
criteria:
  - id: AC-1
    text: "<criterion text>"
    satisfied: yes | partial | no
    code_evidence: "src/foo.rs:42"
    test_evidence: "tests/foo_test.rs:10"
---
```

The body is free-form prose summarizing the change for human reviewers. The pre-PR gate parses front matter only; prose drift never causes a deny by itself.

### Verify-cmd discovery

`kiro-lint-test` (and `kiro-ci-fix` to a lesser extent) needs the project's verify command. Discovery, in priority order:

1. `WORKFLOW.md` at the worktree root — explicit verify command if declared.
2. `CLAUDE.md` / `AGENTS.md` at the worktree root.
3. `Makefile` / `justfile` target named `verify` / `test` / `check`.
4. `package.json` scripts (`test`, `lint`, `typecheck`).
5. `Cargo.toml` → `cargo test && cargo clippy -- -D warnings`.
6. `pyproject.toml` → `pytest` / `ruff` / `mypy` per project hints.

The skill reads what already exists for AI consumers; no per-repo roki-specific config is required.

### Skill → daemon terminal contract

The daemon does not interpret subprocess output (per [FR 07: Worker Execution](07-worker-execution.md) §Boundaries). To convey a structured outcome without violating that boundary, the skill's final stdout line MUST start with the literal prefix `ROKI_RESULT:` followed by a single-line JSON object:

```json
{
  "outcome": "pr_opened" | "escalated" | "failed",
  "phase":   "context_load" | "plan" | "implement" |
             "self_review" | "lint_test" | "open_pr" |
             "ci_fix" | "review" | null,
  "pr_url":  "https://github.com/..." | null,
  "review_artifact_path": "<session-tempdir>/review.md" | null,
  "summary": "human-readable single-paragraph summary",
  "reason":  "non-null only when outcome ∈ {escalated, failed}",
  "attempts": { "self_review": 2, "lint_test": 1, "ci_fix": 0 }
}
```

`phase` is the phase the skill was *in* when it terminated. The daemon parses the **last** `ROKI_RESULT:` line; earlier lines are advisory progress markers. This stays compatible with [FR 07: Worker Execution](07-worker-execution.md) §Termination handling — the daemon derives state-machine transitions from subprocess exit and Linear state, treating `ROKI_RESULT` as a notification surface ([FR 14: Operator Notifications](14-operator-notifications.md)) and a `ref:log-events` payload, not as a state-machine input. The pre-PR gate's structural verdict comes from `review.md`, not from `ROKI_RESULT`.

### Skill cancellation / timeout

If the daemon SIGTERMs the worker (per [FR 07: Worker Execution](07-worker-execution.md) stall handling), the skill SHOULD treat it as cancellation: stop dispatching new subagents, allow the current subagent to wind down briefly, and exit. The worktree and session tempdir are preserved so a human can inspect intermediate state ([FR 06: Worktree and Session](06-worktree-and-session.md)).

## Capabilities

- **Single-entry orchestration**: one skill drives the entire phase plan; the daemon dispatches once, parses one terminal `ROKI_RESULT:` line, and reads one `review.md`.
- **Bounded loops with skill-owned counters**: self-review (5), lint/test (5), CI fix (3). Counters live in skill state, not in any daemon-side persistent column.
- **Skill-first review.md production**: the gate-relevant artifact is produced inside the worker before clean exit, keeping the boundary "one bounded `claude` invocation per ticket" intact.
- **Subagent isolation**: each subagent has a fresh context and a tight tool scope, decoupling phase prompts from each other.
- **Operator-installed tools, no daemon proxy**: every Bash / `gh` / Linear-MCP call goes through the operator's Claude Code tool surface unchanged ([FR 11: Agent Tool Boundary](11-agent-tool-boundary.md)).

## Boundaries

- **The daemon does not drive these phases** — it observes subprocess lifecycle, the `review.md` artifact, and Linear state only. State-machine transitions come from those observations, not from `ROKI_RESULT.phase` ([FR 07: Worker Execution](07-worker-execution.md)).
- **The daemon does not enforce the phase plan** — a skill that runs a different phase order is the operator's choice. The daemon's only structural expectations are (a) one bounded invocation eventually exits, (b) a `review.md` matching the documented schema is left at the documented path on a clean exit that intends to enter the review gate, and (c) optionally a final `ROKI_RESULT:` line.
- **Loop budgets are advisory** — they describe the bundled skill's intended behavior. An operator-modified skill may set its own budgets; `attempts` in `ROKI_RESULT` reports whatever was used.
- **Subagent definitions are skill-internal** — they live alongside `~/.claude/skills/kiro-*` (or this repo's `.claude/agents/` for project-bundled ones). The daemon registers no subagent and proxies no subagent call.
- **No daemon-driven retry of the inner phases** — retries inside the skill (self-review, lint-test, ci-fix loops) are the skill's responsibility; the daemon-level retry budget ([FR 07: Worker Execution](07-worker-execution.md)) only covers non-clean exits of the whole subprocess. The review-gate's `Deny+RetryWithContext` re-launches the whole worker once with `additional_context`, which is a separate mechanism owned by [FR 09](09-pre-pr-gate.md).
- **Auto-merge is out of scope here** — Phase 5 ends at "PR opened". Whether the daemon then auto-merges on CI green is governed elsewhere (currently out of MVP).
- **Multi-engine portability is out of scope** — the contract assumes one Claude Code session per worker invocation. If non-Claude engines are added later, the skill abstraction needs re-expression in those engines (deferred).

## Traceability

- **Roadmap**: `roadmap.md` > Scope > In > "Single bounded `claude ...` invocation per admitted issue"; Boundary Strategy > "Agent-side tool surface (no daemon registration)".
- **Requirements**: none yet — this FR documents the agent-side workflow contract that complements the daemon-side requirements in `roki-mvp` Req 5 / 7 / 9 and the gate-side requirements in `roki-review-gate`. A future spec covering the bundled kiro skill set should backfill explicit requirement IDs and add `implements:` here.
- **Design**:
  - `Engine Adapter` and `Worker invocation loop` sections of `.kiro/specs/roki-mvp/design.md`.
  - `review.md` artifact section of `.kiro/specs/roki-review-gate/design.md`.
- **Related FR**: [07-worker-execution](07-worker-execution.md), [08-pre-implementation-gate](08-pre-implementation-gate.md), [09-pre-pr-gate](09-pre-pr-gate.md), [11-agent-tool-boundary](11-agent-tool-boundary.md), [14-operator-notifications](14-operator-notifications.md).

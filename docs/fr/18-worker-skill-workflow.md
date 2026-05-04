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

> The internal workflow the agent-side kiro skill set runs inside one bounded `claude --print --output-format stream-json` worker invocation: phase plan, the kiro skills it composes, the `review.md` artifact produced before clean exit, and the structured terminal contract the daemon parses back.

## Purpose

[FR 07: Worker Execution](07-worker-execution.md) deliberately keeps the daemon a thin observer that only reacts to subprocess exit, the artifacts left in the session tempdir, and Linear state. The actual code-changing work — implement, review, validate, open PR, fix CI, and finally synthesize a `review.md` artifact — happens **inside one Claude Code session**, driven by the kiro skill set the operator has installed under `~/.claude/skills/kiro-*` and `.claude/skills/kiro-*`. That session is structurally better at iterating with full context than a daemon dispatching N short-lived subprocesses with `format!`-built prompts ever could be.

This FR specifies the **expected shape of that internal workflow**: which kiro skills the worker composes, what budgets they honor, and what it must emit on stdout / leave on disk so the daemon can derive a terminal outcome and the review-gate can evaluate it without looking at agent message contents (per [FR 07: Worker Execution](07-worker-execution.md) §Boundaries).

It is the agent-side counterpart to FR 07 — together they describe one worker invocation — and the inside-the-session counterpart to the daemon-enforced gates in [FR 08: Pre-Implementation Gate](08-pre-implementation-gate.md) and [FR 09: Pre-PR Gate](09-pre-pr-gate.md).

## User-visible Behavior

### Phase plan (single bounded invocation)

The skill set executes the following phases sequentially within one worker subprocess. Phases are skill-internal; the daemon does not observe them granularly. The pre-implementation gate ([FR 08](08-pre-implementation-gate.md)) runs **before** this invocation starts (on the `Judging → Active` transition); the pre-PR gate ([FR 09](09-pre-pr-gate.md)) runs **after** this invocation exits cleanly (on the `Active → Inactive` transition). The worker does not self-check those gates — the daemon side owns the veto.

1. **Context load** — read the ticket description (with `## Acceptance Criteria` per §Acceptance criteria convention), the relevant `.kiro/specs/{feature}/{spec.json,requirements.md,design.md,tasks.md}`, the project's `WORKFLOW.md`, and the relevant `.kiro/steering/` files. Discover validation commands (kiro-impl Step 1 Preflight handles this).
2. **Implement** — invoke `kiro-impl <feature>` ([.claude/skills/kiro-impl](#existing-skill-set)) in autonomous mode (no task numbers): for each pending task, dispatch a fresh implementer subagent (TDD-first), then dispatch `kiro-review` as an independent reviewer subagent. The skill's internal loop handles reviewer rejections by remediation + re-review until APPROVED, marks the task `[x]`, and proceeds. Counters live inside `kiro-impl`.
3. **Feature-level validation** — invoke `kiro-validate-impl <feature>`. Catches cross-task issues that per-task `kiro-review` cannot see (cross-task boundary spillover, integration seams, full-suite regressions). Output: `GO` / `NO_GO`. On `NO_GO` with remediation budget remaining (max 3), feed findings back into another `kiro-impl` pass scoped to the failing tasks; escalate on exhaustion.
4. **Open PR** — `gh pr create` via Bash (no dedicated skill). The PR description embeds a brief change summary plus the validation outcome.
5. **CI fix loop (max 3)** — poll GitHub Actions until checks settle. On red, invoke `kiro-debug` to root-cause + propose a fix, apply it, push. Use `kiro-verify-completion` (claim type `TEST_OR_BUILD`) to gate each push attempt with fresh evidence. Escalate if still red after the budget; the PR remains open per [FR 07: Worker Execution](07-worker-execution.md).
6. **Produce `review.md`** — synthesize the structured `review.md` artifact at the path documented in [ref:artifacts](../reference/artifacts.md), drawing on the verdicts already accumulated this session (per-task `kiro-review` APPROVED set, `kiro-validate-impl` GO, the verify-cmd outcome, and any `kiro-verify-completion` VERIFIED stamps). Each `## Acceptance Criteria` entry from the ticket gets one row with `code_evidence` (file/line) and `test_evidence` (file/line) plus an overall verdict. The pre-PR gate ([FR 09](09-pre-pr-gate.md)) parses this artifact structurally; if it is missing or malformed the gate denies the transition.
7. **Clean exit** — emit a final `ROKI_RESULT:` line on stdout (see §Skill → daemon terminal contract) and exit 0.

A Type B (with-human-planning) ticket inserts one phase before Phase 2: the worker uses the operator-installed Linear MCP (per [FR 11: Agent Tool Boundary](11-agent-tool-boundary.md)) to post questions as ticket comments and poll for replies. When the human approves a plan, write it to the ticket body as a `## kiro Plan` YAML section, then proceed. There is no dedicated `kiro-plan-with-human` skill today — this is normal Claude Code session work using Linear MCP and Bash.

### Existing skill set

The phases above compose existing kiro skills installed at the operator's Claude Code skill directory. The worker uses them directly; no roki-specific subagents are introduced.

| Skill | Used in phase | Purpose | Tool scope (per skill manifest) |
|---|---|---|---|
| `kiro-impl` | Phase 2 | Drives the implementer-then-reviewer loop per task; owns TDD discipline, validation command discovery, and per-task remediation | Read, Write, Edit, MultiEdit, Bash, Glob, Grep, Agent, WebSearch, WebFetch |
| `kiro-review` | Phase 2 (dispatched by `kiro-impl`) | Adversarial per-task review against approved spec + boundary; APPROVED / REJECTED | Read, Bash, Grep, Glob |
| `kiro-validate-impl` | Phase 3 | Cross-task feature-level validation after all tasks complete; GO / NO_GO | Read, Bash, Grep, Glob, Agent |
| `kiro-debug` | Phase 5 | Root-cause-first failure analysis; used on CI red to propose a minimal fix | per skill manifest |
| `kiro-verify-completion` | Phases 3, 5 (and as a fresh-evidence gate elsewhere) | Refuses success claims that lack fresh evidence; VERIFIED / NOT_VERIFIED / MANUAL_VERIFY_REQUIRED | Read, Bash, Grep, Glob |

The authoring-time skills (`kiro-spec-init`, `kiro-spec-requirements`, `kiro-spec-design`, `kiro-spec-tasks`, `kiro-spec-batch`, `kiro-spec-quick`, `kiro-spec-status`, `kiro-validate-design`, `kiro-validate-gap`, `kiro-discovery`, `kiro-steering`, `kiro-steering-custom`) are **operator-side**: they run in the operator's interactive Claude Code session before a ticket is admitted, not inside the worker invocation. The worker reads the artifacts those skills produce (`.kiro/specs/{feature}/*` and `.kiro/steering/*`) but does not invoke them.

### Acceptance criteria convention (EARS)

Every Linear ticket eligible for roki dispatch MUST include an `## Acceptance Criteria` section in its body, written in EARS style — preferably the Ubiquitous (`The X shall Y.`) and Event-driven (`When <trigger>, the X shall Y.`) patterns. Without explicit, written criteria, there is no objective basis for `review.md` to populate per-criterion entries; the setup judge ([FR 05: Setup Judge](05-setup-judge.md)) is expected to refuse such tickets at admission.

### `review.md` artifact contract

Phase 6 writes one Markdown file with YAML front matter at the path documented in [ref:artifacts](../reference/artifacts.md). Front-matter keys (the structurally-parsed surface):

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

### Skill → daemon terminal contract

The daemon does not interpret subprocess output (per [FR 07: Worker Execution](07-worker-execution.md) §Boundaries). To convey a structured outcome without violating that boundary, the worker's final stdout line MUST start with the literal prefix `ROKI_RESULT:` followed by a single-line JSON object:

```json
{
  "outcome": "pr_opened" | "escalated" | "failed",
  "phase":   "context_load" | "plan" | "implement" |
             "validate" | "open_pr" | "ci_fix" |
             "review" | null,
  "pr_url":  "https://github.com/..." | null,
  "review_artifact_path": "<session-tempdir>/review.md" | null,
  "summary": "human-readable single-paragraph summary",
  "reason":  "non-null only when outcome ∈ {escalated, failed}",
  "attempts": { "validate_remediation": 2, "ci_fix": 0 }
}
```

`phase` is the phase the worker was *in* when it terminated. The daemon parses the **last** `ROKI_RESULT:` line; earlier lines are advisory progress markers. This stays compatible with [FR 07: Worker Execution](07-worker-execution.md) §Termination handling — the daemon derives state-machine transitions from subprocess exit and Linear state, treating `ROKI_RESULT` as a notification surface ([FR 14: Operator Notifications](14-operator-notifications.md)) and a `ref:log-events` payload, not as a state-machine input. The pre-PR gate's structural verdict comes from `review.md`, not from `ROKI_RESULT`.

### Cancellation / timeout

If the daemon SIGTERMs the worker (per [FR 07: Worker Execution](07-worker-execution.md) stall handling), the active skill SHOULD treat it as cancellation: stop dispatching new subagents, allow the current subagent to wind down briefly, and exit. The worktree and session tempdir are preserved so a human can inspect intermediate state ([FR 06: Worktree and Session](06-worktree-and-session.md)).

## Capabilities

- **Single-entry orchestration**: one worker invocation drives the whole phase plan; the daemon dispatches once, parses one terminal `ROKI_RESULT:` line, and reads one `review.md`.
- **Bounded loops with skill-owned counters**: `kiro-impl`'s per-task review loop, the validate-remediation budget (3), and the CI fix budget (3) all live in skill / worker state, not in any daemon-side persistent column.
- **Skill-first review.md production**: the gate-relevant artifact is produced inside the worker before clean exit, keeping the boundary "one bounded `claude` invocation per ticket" intact.
- **Composes existing skills, not new ones**: phases map onto `kiro-impl` / `kiro-review` / `kiro-validate-impl` / `kiro-debug` / `kiro-verify-completion`. No roki-specific subagent is introduced.
- **Operator-installed tools, no daemon proxy**: every Bash / `gh` / Linear-MCP call goes through the operator's Claude Code tool surface unchanged ([FR 11: Agent Tool Boundary](11-agent-tool-boundary.md)).

## Boundaries

- **The daemon does not drive these phases** — it observes subprocess lifecycle, the `review.md` artifact, and Linear state only. State-machine transitions come from those observations, not from `ROKI_RESULT.phase` ([FR 07: Worker Execution](07-worker-execution.md)).
- **The daemon does not enforce the phase plan** — a worker that runs a different phase order is the operator's choice. The daemon's only structural expectations are (a) one bounded invocation eventually exits, (b) a `review.md` matching the documented schema is left at the documented path on a clean exit that intends to enter the review gate, and (c) optionally a final `ROKI_RESULT:` line.
- **Loop budgets are advisory** — they describe the bundled flow's intended behavior. An operator-modified flow may set its own budgets; `attempts` in `ROKI_RESULT` reports whatever was used.
- **Skill internals are skill-owned** — the per-task review loop, validation command discovery, and remediation strategies live inside `kiro-impl` / `kiro-validate-impl` / `kiro-review` and are not re-specified here. This FR only specifies how the worker composes them.
- **Authoring-time skills are out of scope** — `kiro-spec-*`, `kiro-validate-design`, `kiro-validate-gap`, `kiro-discovery`, and the steering skills are operator-side. The worker reads their outputs but does not invoke them.
- **No daemon-driven retry of the inner phases** — retries inside the worker are skill / worker responsibility; the daemon-level retry budget ([FR 07: Worker Execution](07-worker-execution.md)) only covers non-clean exits of the whole subprocess. The review-gate's `Deny+RetryWithContext` re-launches the whole worker once with `additional_context`, which is a separate mechanism owned by [FR 09](09-pre-pr-gate.md).
- **Auto-merge is out of scope here** — Phase 4 ends at "PR opened". Whether the daemon then auto-merges on CI green is governed elsewhere (currently out of MVP).
- **Multi-engine portability is out of scope** — the contract assumes one Claude Code session per worker invocation. If non-Claude engines are added later, the kiro skill set abstraction needs re-expression in those engines (deferred).

## Traceability

- **Roadmap**: `roadmap.md` > Scope > In > "Single bounded `claude ...` invocation per admitted issue"; Boundary Strategy > "Agent-side tool surface (no daemon registration)".
- **Requirements**: none yet — this FR documents the agent-side workflow contract that complements the daemon-side requirements in `roki-mvp` Req 5 / 7 / 9 and the gate-side requirements in `roki-review-gate`. A future spec covering the bundled kiro skill set should backfill explicit requirement IDs and add `implements:` here.
- **Design**:
  - `Engine Adapter` and `Worker invocation loop` sections of `.kiro/specs/roki-mvp/design.md`.
  - `review.md` artifact section of `.kiro/specs/roki-review-gate/design.md`.
  - Skill manifests under `.claude/skills/kiro-*/SKILL.md` (kiro-impl, kiro-review, kiro-validate-impl, kiro-debug, kiro-verify-completion).
- **Related FR**: [07-worker-execution](07-worker-execution.md), [08-pre-implementation-gate](08-pre-implementation-gate.md), [09-pre-pr-gate](09-pre-pr-gate.md), [11-agent-tool-boundary](11-agent-tool-boundary.md), [14-operator-notifications](14-operator-notifications.md).

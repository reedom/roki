---
refs:
  id: fr:19-orchestrator-session
  kind: fr
  title: "Orchestrator Session"
  spec: roki-mvp
  related:
    - fr:01-daemon-lifecycle
    - fr:02-configuration
    - fr:04-state-machine-and-recovery
    - fr:07-worker-execution
    - fr:11-agent-tool-boundary
    - fr:12-extension-surface
    - fr:14-operator-notifications
    - fr:18-worker-skill-workflow
---

# FR 19: Orchestrator Session

> The long-lived per-ticket `claude --input-format stream-json --output-format stream-json` "thinking" component, launched after the daemon's mechanical pre-admission-judge passes ([04-state-machine-and-recovery](04-state-machine-and-recovery.md) §Pre-admission judge), with a `mode` flag (`SPEC_DRIVEN` or `NEEDS_CLASSIFY`) rendered into its system prompt. Owns target spec resolution (SPEC_DRIVEN), classify-driven path branching (NEEDS_CLASSIFY), per-phase planning, structural validation of `review.md` and the target spec docs, daemon-directive interpretation, and Linear writes.

## Purpose

The previous architecture launched a single bounded `claude --print` worker per admitted issue plus two auxiliary one-shot subprocesses (a setup judge for admission classification and a linear-updater subagent for daemon-only failure surfacing) and two daemon-side mechanical artifact-validation gates (kiro-spec gate at `Judging → Active`, kiro-review gate at `Active → Inactive`). That model gave the daemon no structured handle for per-phase budgets, no clean way to cap thinking effort, forced two extra prompt templates and lifecycles to surface admission and Linear writes, and split artifact validation across an LLM substantive layer plus a daemon-side mechanical structural layer with its own config namespace, retry counters, and `Inactive.reason` discriminator values.

A first orchestrator-session redesign absorbed all three roles into the orchestrator and introduced a per-issue `materialize_spec` phase that called `kiro-discovery` to produce `requirements.md`. That redesign was wrong on two counts: `kiro-discovery` produces `brief.md`, not `requirements.md`, and forcing every Linear ticket through full per-issue spec materialization is overkill for the bug-fix / config-change / trivial-addition tickets that comprise the bulk of daemon-eligible work.

This FR canonicalizes the corrected design: the daemon does mechanical pre-admission-judging in Rust (assignee match + fixed Linear labels `roki:ready` / `roki:impl`, per [04-state-machine-and-recovery](04-state-machine-and-recovery.md)), sets a per-ticket `mode` flag, and launches the orchestrator with the mode rendered into its system prompt. The orchestrator branches on the mode:

- **SPEC_DRIVEN** (operator declared the project-level `<repo>/.kiro/specs/<target>/` is complete by adding `roki:impl`): the orchestrator's first turn resolves the target spec name from the ticket body, structurally validates the four spec docs, then nominates `implement` (driven by `kiro-impl`) → `review` → `validate` → `open_pr` → optional `ci_fix` → `finalize_review`.
- **NEEDS_CLASSIFY** (operator added only `roki:ready`): the orchestrator's first turn nominates the `classify` phase (driven by `roki-classify`, a daemon-purpose-built classifier derived from `kiro-discovery` Step 1+2 with no dialogue). On Path B (no spec needed) the orchestrator continues to `implement` (driven by a daemon-internal prompt template `prompt_template_implement_direct`, no project-level spec, ticket body's `## Acceptance Criteria` as the sole authoritative source) → the same downstream chain. On Path A / C / D / E the orchestrator writes a Linear comment with the recommended next manual command and label, then stops with `outcome=needs_operator`.

The orchestrator does not edit code (no Edit, no Write) but it can run shell and read files (Bash + Read inside a read-only filesystem sandbox) so it can `stat` / `test -f` / `grep` artifacts and the project-level spec docs itself. The daemon itself never writes Linear directly and never decides whether an artifact passes — both are the orchestrator's responsibility.

Rejected alternatives: putting Linear-label parsing inside the orchestrator (wastes a thinking turn on a mechanical decision the daemon can do for free); making `roki:impl` alone (without `roki:ready`) sufficient for SPEC_DRIVEN admission (operator's two-step "I authorize + I declare spec ready" intent would collapse into a single label slip); driving the SPEC_DRIVEN path through `materialize_spec` + `kiro-spec-quick --auto` (forces full spec materialization even when the operator already has a project-level spec, defeating the operator's explicit `roki:impl` signal); auto-detecting "spec complete enough to implement" via `spec.json` approvals heuristics from inside the daemon (turns the operator's intent into a guess, with no escape hatch when the heuristic disagrees).

## User-visible Behavior

### Lifecycle

- **Launch**: the orchestrator is launched once per ticket on entry to `Pending` (from pre-admission-judge pass per [04-state-machine-and-recovery](04-state-machine-and-recovery.md)). Launch happens inside the issue's session tempdir; tool surface is restricted via `--settings` (see §Tool surface). The `prompt_template_orchestrator` block from `WORKFLOW.md` ([12-extension-surface](12-extension-surface.md)) is rendered as the system prompt with the `mode` flag (`SPEC_DRIVEN` or `NEEDS_CLASSIFY`) substituted in. The mode is immutable for the session lifetime.
- **Steady state**: the daemon writes JSON events to the orchestrator's stdin; the orchestrator returns one strict JSON action object per turn on stdout (after any extended-thinking block). The daemon does not interpret the orchestrator's reasoning text — only the JSON action field.
- **First-turn behavior** depends on mode:
  - `SPEC_DRIVEN`: the orchestrator resolves the target spec name from the ticket body (LLM inference; the ticket body is passed in via the launch envelope), then runs structural checks against `<repo>/.kiro/specs/<target>/{spec.json,requirements.md,design.md,tasks.md}` using Read + Bash. On pass it nominates `implement` (`/kiro-impl <target>`). On fail (target unresolvable, files missing, `approvals.tasks.approved == false`) it writes a Linear comment naming the missing artifact and the recommended `/kiro-spec-*` command, then stops with `outcome=spec_incomplete`.
  - `NEEDS_CLASSIFY`: the orchestrator immediately nominates `classify` (`/roki-classify`). On `classify_complete` it branches per the returned `path` value (see §Event catalog).
- **Graceful termination**: the orchestrator is gracefully terminated when the issue lands in any `Inactive(reason=*)` state and any orchestrator-driven Linear writes for that terminal state have completed. The daemon sends a final `stop`-acknowledgement signal then closes the orchestrator's stdin and waits for clean exit within the configured shutdown window ([01-daemon-lifecycle](01-daemon-lifecycle.md)).
- **Forced termination**: `Cleaning` (entered on tracker-observed terminal Linear state or assignment loss, per [04-state-machine-and-recovery](04-state-machine-and-recovery.md)) may force-terminate the orchestrator regardless of in-flight turns — cleanup of worktree / session tempdir takes priority.
- **Restart non-persistence**: the orchestrator is not persisted across daemon restarts. A fresh orchestrator is launched when the issue re-enters `Pending` via restart-recovery (per `roki-mvp Req 3.14` / `Req 10`). In-flight turns and any orchestrator-internal scratch state are discarded; the next orchestrator starts from the rendered system prompt with the mode recomputed from the current Linear label set.

### Event catalog (daemon → orchestrator stdin)

The daemon translates state-machine and subprocess-lifecycle observations into JSON events on the orchestrator's stdin. Each event is a single JSON object on its own line. The current event catalog:

| event | trigger | Orchestrator action expected |
|---|---|---|
| `phase_complete` | A phase subprocess clean-exited with `subtype: success`. For `classify` the payload includes `result.path ∈ {A, B, C, D, E}` and `result.suggested_command` / `result.suggested_label`. For `finalize_review` the payload includes `review_artifact_path`. For `open_pr` the payload includes `pr_url`. | For `classify`: branch on `path`. Path B → `action=run_phase phase=implement`. Path A / C / D / E → write Linear comment with `suggested_command` + `suggested_label` via Linear MCP, then `action=stop outcome=needs_operator`. For `finalize_review`: read the artifact (see §Artifact validation) and decide. For other phases: return `action=run_phase` (next phase) or `action=stop`. |
| `phase_nonclean` | Phase non-zero exit, stall, or per-phase `--max-turns` exhaustion. | Judgment call: re-run the same phase, fall through to `ci_fix`, or `action=stop`. |
| `daemon_directive` | A daemon-only failure that the orchestrator is alive to surface (stall on a sibling phase, retry exhaustion, filesystem poison, restart-recovery orphan, etc.). | Write the appropriate Linear label + comment via Linear MCP and return `action=linear_update_done`. |
| `tracker_terminal` | Linear state moved to `done` / `canceled` or assignment was lost ([04-state-machine-and-recovery](04-state-machine-and-recovery.md)). | Return `action=stop` with `outcome=cancelled`; cleanup follows. |

The previous `admission_request` event is removed: classification (when needed) is performed inside the `classify` phase subprocess, and SPEC_DRIVEN target resolution happens inside the orchestrator's first deliberation turn against the ticket body and project-level spec docs already on disk.

The daemon does not deliver events to the orchestrator while a phase subprocess is running — it waits for the phase to terminate, observes the exit, then sends the matching `phase_complete` / `phase_nonclean`.

### Response schema (strict JSON)

After any extended-thinking block, the orchestrator emits exactly one JSON object per turn on stdout. The daemon parses the **last** JSON object on the orchestrator's stdout per turn; earlier emissions are advisory progress and do not influence the state machine.

```json
{
  "action": "run_phase" | "linear_update_done" | "stop",
  "phase": "classify" | "implement" | "review" | "validate" | "open_pr" | "ci_fix" | "finalize_review" | null,
  "additional_context": "<bounded string>" | null,
  "outcome": "success" | "failure" | "cancelled" | "needs_operator" | "spec_incomplete" | "needs_split" | "allowlist_rejected" | null,
  "linear_writes": ["label:<name>", "comment_posted:<id>", ...] | null,
  "reason": "<= 200 chars"
}
```

Field semantics:

- `action` is always required; the other fields are populated only when meaningful for the chosen action.
- `phase` is required when `action=run_phase`; the catalog matches the phase set in [18-worker-skill-workflow](18-worker-skill-workflow.md). The set of legal phase values is mode-dependent: `classify` is only legal in NEEDS_CLASSIFY mode (and only on the first turn); the rest are legal in both modes.
- `additional_context` is the verbatim payload forwarded to the next phase subprocess via the engine adapter's `additional_context` channel ([12-extension-surface](12-extension-surface.md)). For SPEC_DRIVEN `implement` it carries the resolved target spec name and the path to the project-level spec dir. For direct-mode `implement` it carries the ticket body's numbered `## Acceptance Criteria` (verbatim) and any prior reviewer findings on retry.
- `outcome` is required when `action=stop`. It drives the issue's terminal `Inactive.reason` selection on the daemon side per the table in [04-state-machine-and-recovery](04-state-machine-and-recovery.md). Pre-phase stops (`spec_incomplete`, `needs_operator`, `needs_split`, `allowlist_rejected`) all require the orchestrator to have written the matching Linear feedback in the same turn — `linear_writes` lists what was written.
- `linear_writes` is required when `action=linear_update_done` and on any `action=stop` whose `outcome` is one of the operator-facing pre-phase stops above; it lists what the orchestrator wrote in this turn so the daemon can log the side effect and detect partial writes.
- `reason` is bounded human-readable rationale for the structured log; it is **not** a state-machine input.

The previous `judge`, `repo`, and `rejected_repos` fields are removed from the schema. Repo identity is resolved inside the orchestrator's first deliberation (SPEC_DRIVEN: from the project-level spec dir's repo; NEEDS_CLASSIFY: from the `classify` phase output) and travels through `additional_context` rather than through a dedicated schema field.

### Tool surface

The orchestrator's tool surface is enforced by the daemon via `--settings` at launch and is independent of phase subprocesses' tool surface ([11-agent-tool-boundary](11-agent-tool-boundary.md)):

- **Linear MCP (write)** — the operator's installed Linear MCP, used by the orchestrator for label + comment writes (pre-phase operator-facing stops, `daemon_directive` surfacing, and `review.md` validation retry-budget exhaustion).
- **Read** (workspace, read-only) — the orchestrator reads ticket-related files inside the worktree and the session tempdir, including the artifacts it validates. In SPEC_DRIVEN mode the Read scope additionally includes `<repo>/.kiro/specs/<target>/{spec.json,requirements.md,design.md,tasks.md}` of the resolved target spec (a project-level path outside the issue's session tempdir).
- **Bash** (read-only filesystem sandbox) — the orchestrator runs read-only shell commands for structural checks: `stat`, `test -f`, `grep -E` for EARS keywords, `jq`-style spot-checks on `spec.json` `approvals.tasks.approved`, and `test -f` for each `code_references` entry's reachability in `review.md`. Bash invocations execute inside the read-only filesystem sandbox so they cannot mutate the worktree, session tempdir, or the project-level spec dir even if the orchestrator or its prompt accidentally tries.
- **No** Edit, **no** Write, **no** Agent dispatch, **no** other MCPs.

The session runs with a read-only filesystem sandbox regardless of operator overrides (per `roki-mvp` Constraints > Permissions). Phase subprocesses inherit the operator's broader tool surface separately; the orchestrator's narrow surface does not constrain them.

A future Rust-native read-only check tool (`roki-check spec <path>`, `roki-check review <path>`) is a deferred MVP-out item: when the orchestrator's Bash usage stabilizes into a fixed pattern, that tool can replace the Bash usage and Bash can be removed from `extension.orchestrator.allowed_tools`. Until then Bash is the simplest path; the read-only filesystem sandbox bounds the blast radius.

### Artifact validation

The orchestrator is the structural witness for two classes of artifact:

- **SPEC_DRIVEN target spec docs** (`<repo>/.kiro/specs/<target>/{spec.json,requirements.md,design.md,tasks.md}`) — checked once on the orchestrator's first turn before any phase is nominated. Checks: target spec name resolvable from ticket body; all four files present; `requirements.md` non-empty with at least one EARS trigger keyword (`WHEN` / `IF` / `WHILE` / `WHERE` / `SHALL`); `tasks.md` contains at least one actionable sub-task line; `spec.json` `approvals.tasks.approved == true`. On pass the orchestrator nominates `implement`. On fail the orchestrator writes a Linear comment naming the missing artifact and the recommended `/kiro-spec-*` command (e.g. `/kiro-spec-tasks <feature>`), then `action=stop outcome=spec_incomplete`. There is no retry budget here — the operator is the only one who can fix a missing or unapproved spec doc.
- **`review.md`** — produced by the `finalize_review` phase ([18-worker-skill-workflow](18-worker-skill-workflow.md)). After the `phase_complete(finalize_review)` event the orchestrator reads the artifact (canonical path in [ref:artifacts](../reference/artifacts.md)) and checks: file presence; schema (overall `status` of `pass | fail`, per-criterion entries indexed by the appropriate criterion ID source — see below — and `code_references` on each `pass` entry); reachability of each `code_references` path via `test -f`. On pass with overall `status=pass`, the orchestrator emits `action=stop` with `outcome=success`. On structural failure (missing artifact, schema invalid, unreachable code reference) or overall `status=fail` with retry budget remaining, the orchestrator re-nominates the `implement` phase with `additional_context` populated from the failing per-criterion entries (criterion id, fail reason, diagnostic text). On retry-budget exhaustion the orchestrator writes the matching Linear label + comment via Linear MCP and emits `action=stop` with `outcome=failure`.

The criterion ID source for `review.md` validation is mode-dependent:

- **SPEC_DRIVEN**: criterion IDs are the numeric requirement IDs in `<repo>/.kiro/specs/<target>/requirements.md`.
- **NEEDS_CLASSIFY (Path B / direct mode)**: criterion IDs are the numbered EARS sentences in the Linear ticket body's `## Acceptance Criteria` section. Per FR 18 §Acceptance criteria convention, every roki-eligible ticket body MUST contain such a section; the daemon's pre-admission-judge does not enforce this (it is a content rule, not a label rule), so a malformed ticket body is caught at the orchestrator's first downstream check rather than at admission.

Substantive judgment of "does the code satisfy the criterion" is owned by the kiro skill set inside the prior phase subprocesses (per-task `kiro-review` plus `kiro-validate-impl` plus `finalize_review` synthesis in SPEC_DRIVEN; the equivalent daemon-internal prompts in direct mode). The orchestrator's role is structural-only: file presence, schema shape, code-reference reachability. The orchestrator does not re-judge whether a criterion is correct.

The retry budget for `review.md` is orchestrator-internal (drawn from `prompt_template_orchestrator`; default suggestion: 3 retries) and is bounded overall by `max_phases`. There is no per-artifact `max_attempts` config knob; operators tune the limit by editing `prompt_template_orchestrator` or `max_phases`.

### Configuration

Configuration lives under the reserved `extension.orchestrator.*` namespace in `WORKFLOW.md` ([12-extension-surface](12-extension-surface.md), per `roki-mvp Req 6.5`). Canonical defaults:

| key | default | meaning |
|---|---|---|
| `model` | `"claude-opus-4-7"` | Claude model identifier for the orchestrator |
| `effort` | `"middle"` | Extended-thinking budget; range `low` / `middle` / `high` |
| `max_phases` | `15` | Total phase subprocesses the orchestrator may nominate before the budget is exhausted (lowered from the prior 20 since `materialize_spec` is removed and `classify` runs at most once per ticket) |
| `allowed_tools` | Linear MCP (write) + `Read` + `Bash` | Allowlist passed via `--settings`. `Bash` is included so the orchestrator can run read-only structural commands (`stat`, `test -f`, `grep -E`, `jq` spot-checks); the read-only filesystem sandbox prevents mutation regardless |

A single named template block — `prompt_template_orchestrator` — drives the system prompt. The mode flag (`SPEC_DRIVEN` or `NEEDS_CLASSIFY`) is substituted into the prompt at render time so the same template adapts to both branches without operators maintaining two templates. The phase-specific daemon-internal templates (`prompt_template_implement_direct`, `prompt_template_validate_direct`, `prompt_template_open_pr`) are listed in [02-configuration §WORKFLOW.md](02-configuration.md) and consumed by their respective phases per [18-worker-skill-workflow](18-worker-skill-workflow.md). Hot-reload of the namespace applies from the next ticket admission; an in-flight orchestrator keeps its rendered prompt for the lifetime of the session.

### Failure modes

When the orchestrator is alive, the orchestrator is responsible for surfacing operator-facing failures to Linear via either the pre-phase `action=stop` path (with `outcome=spec_incomplete | needs_operator | needs_split | allowlist_rejected`) or the `daemon_directive` event path; these replace the previous one-shot linear-updater subprocess. When the orchestrator is dead — process crash, persistent JSON schema drift, or `max_phases` exhausted — the daemon **does not** fall back to a Linear write of its own. Instead it routes the issue to one of three orchestrator-dead `Inactive.reason` values, logs the event structurally, and populates the TUI escalation queue ([14-operator-notifications](14-operator-notifications.md)). Operators notice these three cases via the TUI; there is no Linear-side notification.

| failure | action | Inactive.reason |
|---|---|---|
| Orchestrator crash / SIGSEGV / non-zero exit without a `stop` action | log + escalation queue entry; no Linear write | `orchestrator_crash` |
| Orchestrator schema drift on two consecutive turns (after one daemon-side reprompt) | log + escalation queue entry; no Linear write | `orchestrator_unparseable` |
| `max_phases` exhausted (the orchestrator would nominate another phase but the budget is gone) | log + escalation queue entry; no Linear write | `orchestrator_budget_exhausted` |
| Orchestrator stall (no event emitted in N seconds, configurable) | daemon SIGTERMs the orchestrator → routes through the crash path | `orchestrator_crash` |
| Linear MCP write failure inside an orchestrator turn | the orchestrator retries up to once internally, then surfaces a partial-write entry in `linear_update_done.linear_writes`; daemon does **not** retry on the orchestrator's behalf | n/a (turn continues) |

These three orchestrator-dead reasons sit alongside the operator-facing pre-phase reasons (`spec_incomplete`, `needs_operator`, `needs_split`, `allowlist_rejected`) and the existing post-phase reasons (`awaiting_linear`, `retry_exhausted`, `stall`, `fs_poison`, `orphan`) per the table in [04-state-machine-and-recovery](04-state-machine-and-recovery.md). All non-`awaiting_linear` reasons preserve the worktree / session tempdir until the operator manually closes the Linear ticket, after which `Cleaning` proceeds.

## Capabilities

- **Mechanical pre-admission, LLM-driven branching**: the daemon's pre-admission-judge ([04-state-machine-and-recovery](04-state-machine-and-recovery.md)) decides "is this ticket for roki at all" without any LLM cost; the orchestrator handles every decision after that and is the only "thinking" component the daemon launches per ticket.
- **Mode-tagged orchestrator**: `SPEC_DRIVEN` and `NEEDS_CLASSIFY` modes share one `prompt_template_orchestrator` block with the mode flag substituted in. The orchestrator's first-turn behavior diverges per mode but the action schema is uniform.
- **Strict JSON action contract**: the orchestrator's stdout is a small action enum with a bounded response schema; the daemon parses the last JSON object per turn and ignores reasoning text.
- **Bounded thinking budget**: `extension.orchestrator.{model, effort, max_phases, allowed_tools}` lets the operator cap the orchestrator's thinking effort (`effort`) and total phase nominations (`max_phases`) without per-process `--max-turns`.
- **Read + Bash + Linear-MCP-write only**: enforced by `--settings`; the orchestrator cannot edit code, dispatch agents, or mutate the filesystem (Bash runs inside a read-only sandbox). Code-changing work is the phase subprocess's role.
- **Orchestrator-driven artifact validation**: the orchestrator structurally validates the SPEC_DRIVEN target spec docs (one shot, no retry) and `review.md` after `finalize_review` (with retry-with-context re-nomination of `implement` on failure). No daemon-side gate hook subscribes to state transitions to do this.
- **Single template block + 3 phase-internal templates**: `prompt_template_orchestrator` (orchestrator) + `prompt_template_implement_direct` / `prompt_template_validate_direct` / `prompt_template_open_pr` (per-phase daemon-internal). The prior `prompt_template_judge` and `prompt_template_linear_updater` are removed.
- **Distinct dead-orchestrator failure path**: three orchestrator-dead `Inactive.reason` values plus the TUI escalation queue cover the cases where the orchestrator cannot surface to Linear itself, without forcing the daemon to hold a Linear write path.

## Boundaries

- **The orchestrator does not edit code, invoke `gh`, or push to git** — that is the phase subprocesses' role ([18-worker-skill-workflow](18-worker-skill-workflow.md)). The orchestrator can run shell (Bash) but only read-only inside the sandbox; mutation requires a phase subprocess.
- **The orchestrator does not parse Linear labels** — that is the daemon's pre-admission-judge's job ([04-state-machine-and-recovery](04-state-machine-and-recovery.md)). The orchestrator only sees the resulting `mode` flag.
- **The daemon does not interpret the orchestrator's reasoning text** — only the JSON action field; thinking blocks are ignored.
- **`max_turns` is a per-phase budget, not an orchestrator budget** — the orchestrator is bounded by `max_phases` instead. There is no per-turn budget on the orchestrator.
- **The orchestrator is not persisted across daemon restarts** — restart-recovery starts a fresh orchestrator when the issue re-enters `Pending` (per `roki-mvp Req 3.14` / `Req 10`); the mode is recomputed from the current Linear label set.
- **Phase subprocess catalog is owned by [18-worker-skill-workflow](18-worker-skill-workflow.md)** — this FR enumerates the action enum's `phase` values for completeness but does not restate per-phase contracts.
- **The orchestrator's context compaction across long-running tickets** is out of MVP scope (deferred — Phase 2 feature). For MVP the `max_phases` budget bounds session length and operators retry from scratch when it is hit.
- **Multi-engine portability of the orchestrator role** is out of scope (the orchestrator is written against Claude Code stream-json; cross-engine adapters are deferred).
- **Daemon-side recovery of a partially-completed Linear write after the orchestrator crash** is out of scope. There is no fallback channel; the partial write is surfaced via the TUI escalation queue and the operator reconciles Linear manually.
- **Daemon-driven Linear writes** are not introduced here; the daemon never writes Linear directly. When the orchestrator is dead, the failure surfaces via TUI only.
- **Mode mutation mid-flight** is not supported; relabeling a ticket while its orchestrator is running does not re-route it. The next webhook re-runs pre-admission-judge per [04-state-machine-and-recovery](04-state-machine-and-recovery.md).

## Traceability

- **Roadmap**: `roadmap.md` > Overview ("a long-lived orchestrator session"); Scope > In ("Long-lived **orchestrator session** per admitted issue"); Constraints > Engine ("Orchestrator session (long-lived)"); Boundary Strategy > "Orchestrator-vs-phase boundary".
- **Requirements**: TBD — concrete `req:roki-mvp:*` IDs are added by the requirements rewrite that succeeds the Stage A roadmap rewrite. Until then this FR documents the orchestrator-session contract that complements the daemon-side requirements in `roki-mvp` Req 5 / 7 / 8 / 9 / 13 and replaces the obsolete `extension.linear_updater.*` namespace from prior `Req 2`.
- **Design**: TBD — `.kiro/specs/roki-mvp/design.md` will gain an `Orchestrator Session` section in a later stage; this FR is the placeholder of record until then.
- **Related FR**: [01-daemon-lifecycle](01-daemon-lifecycle.md), [02-configuration](02-configuration.md), [04-state-machine-and-recovery](04-state-machine-and-recovery.md), [07-worker-execution](07-worker-execution.md), [11-agent-tool-boundary](11-agent-tool-boundary.md), [12-extension-surface](12-extension-surface.md), [14-operator-notifications](14-operator-notifications.md), [18-worker-skill-workflow](18-worker-skill-workflow.md).

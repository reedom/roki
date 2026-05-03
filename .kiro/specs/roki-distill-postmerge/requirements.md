---
refs:
  id: requirements:roki-distill-postmerge
  kind: requirements
  title: "roki-distill-postmerge Requirements"
  spec: roki-distill-postmerge
  implements:
    - roadmap
  provides:
    - req:roki-distill-postmerge:1
    - req:roki-distill-postmerge:2
    - req:roki-distill-postmerge:3
    - req:roki-distill-postmerge:4
    - req:roki-distill-postmerge:5
    - req:roki-distill-postmerge:6
    - req:roki-distill-postmerge:7
    - req:roki-distill-postmerge:8
    - req:roki-distill-postmerge:9
    - req:roki-distill-postmerge:10
    - req:roki-distill-postmerge:11
    - req:roki-distill-postmerge:12
    - req:roki-distill-postmerge:13
---

# Requirements Document

## Project Description (Input)
Each Linear ticket leaves behind flow-type artifacts: kiro `design.md` and `tasks.md`, superpowers specs, plan command outputs, scratch notes under `.kiro/specs/<issue>/` and configurable adjacent paths (e.g. `.superpowers/specs/`, `plans/`, `notes/`). Once the PR has merged and Linear is `Done`, these artifacts have served their purpose. Left untouched they accumulate noise; deleted blindly they lose useful long-term context.

roki-distill-postmerge introduces a post-terminal-state phase to roki-mvp's per-issue state machine that runs after the worker observes Linear `Done` (or the equivalent merge signal from `gh pr view --json mergedAt`). The agent enumerates artifacts under configured `distill.paths` from `WORKFLOW.md`, classifies each into one of three dispositions — **delete**, **archive**, **distill** — applies the disposition, and writes a `distill-manifest.json` under `.kiro/specs/<issue>/`. The daemon validates the manifest against a stable, version-tagged schema and enforces path safety before allowing terminal cleanup. The daemon performs no LLM-style judgment of its own; classification is the agent's responsibility.

The phase is a separate state-machine seam: it does not block the PR merge or the Linear `Done` transition itself. It runs after those events. Default behavior is conservative: when the agent is uncertain, the artifact is archived rather than deleted or distilled. Re-running on an already-distilled issue is a no-op.

## Introduction

The roki-distill-postmerge specification defines a post-terminal-state phase added to the roki-mvp per-issue state machine. The phase activates after a worker has observed that its issue has reached Linear `Done` (or a `gh pr view --json mergedAt` merge timestamp confirms the PR has merged). When activated, the daemon dispatches one constrained sweep turn to the agent inside the issue's existing workspace. The agent walks the artifact discovery paths declared in `WORKFLOW.md` under the `distill.*` namespace, classifies each artifact into delete, archive, or distill, executes the disposition, and writes a manifest at `.kiro/specs/<issue>/distill-manifest.json`. The daemon validates that manifest against a stable JSON-Schema and against path-safety invariants reused from roki-mvp's workspace module before performing terminal cleanup of the workspace.

This specification deliberately splits responsibility between the daemon and the agent. The daemon owns the phase orchestration, the manifest schema, the schema validation, the path-safety enforcement, and the gate that blocks terminal cleanup until validation succeeds. The agent owns the artifact enumeration, the classification, the disposition execution (file moves, archive writes, deletions), and the manifest authorship. The daemon never reads artifact contents to judge their value, never invokes an LLM, and never interprets `WORKFLOW.md`'s `distill.routes` rules — those are inputs the agent consumes inside the sweep turn.

## Boundary Context

- **In scope**:
  - A new post-terminal-state phase added to the roki-mvp state machine (between TerminalSuccess and workspace deletion) that runs the distill sweep once per issue per terminal transition.
  - A trigger condition driven by the agent observing Linear `Done` (or detecting merge via `gh pr view --json mergedAt`); the daemon does not poll GitHub for merge state.
  - Configurable artifact discovery via `WORKFLOW.md` keys under `distill.paths` (e.g. `.kiro/specs/<issue>/`, `.superpowers/specs/`, `plans/`, `notes/`) and `distill.routes` (classification rules consumed by the agent).
  - Three dispositions with a defined default-conservative policy: **delete** for ephemeral run-only artifacts, **archive** for verbatim retention under a stable archive path, **distill** for canonical extraction into a stable home; when classification is ambiguous, default to archive.
  - A stable archive path scheme rooted at `.kiro/archive/<issue>/` inside the workspace.
  - A stable, version-tagged manifest schema at `.kiro/specs/<issue>/distill-manifest.json` enumerating all moved, deleted, distilled, and skipped artifacts.
  - Daemon-side validation of the manifest against the schema and against path-safety invariants (writes restricted to the workspace plus configured project archive root) before terminal cleanup.
  - Idempotency: re-running on an already-distilled issue is a no-op driven by the manifest's presence and version field.
  - Subscription to roki-mvp's transition event bus for `TerminalSuccess` events; the phase appears as a vetoable hook between `TerminalSuccess` and workspace deletion (the specific seam name is finalized in design).
  - Inputs the agent may consult during classification: `review.md` (from roki-review-gate) and `requirements.md` (from roki-spec-gate) when present in the workspace.

- **Out of scope**:
  - Cross-issue or project-level distillation passes that consolidate artifacts from many tickets into a project-level EARS document or shared decision log; that lives in a future cross-issue spec.
  - Auto-commit or auto-PR for outputs the agent produces during distill (if distill produces commit-worthy changes, the agent opens that PR in a follow-up turn outside this spec).
  - Real-time distillation during the implementation run; this spec runs only post-merge.
  - Pre-implementation distillation (the EARS-merge case lives in roki-spec-gate, not here).
  - Any LLM-based judgment performed daemon-side; the daemon validates manifest schema and path safety only.
  - Linear or GitHub state writes by the daemon; the existing roki-mvp boundaries hold.
  - Modifications to the merge or `Done` transition itself; the phase runs strictly after those events.

- **Adjacent expectations**:
  - **roki-mvp** publishes the per-issue state machine and the transition event bus that this phase subscribes to. roki-mvp owns the workspace path layout, the path-safety module, and the terminal cleanup step that this phase gates.
  - **roki-review-gate** may produce a `review.md` artifact that this spec's classifier may consult as input. roki-review-gate does not itself trigger the distill phase.
  - **roki-spec-gate** may produce a `requirements.md` artifact under `.kiro/specs/<issue>/` that this spec's classifier may consult. roki-spec-gate does not itself trigger the distill phase.
  - **`WORKFLOW.md`** carries the `distill.paths` and `distill.routes` keys under the extension namespace reserved by roki-mvp; the loader hot-reloads these without daemon restart. The agent reads them at sweep time.
  - The agent is expected to know how to classify artifacts; the agent-side sweep skill (kiro/superpowers integration) is referenced as an open design point and not constrained here at the requirements level.

## Requirements

### Requirement 1: Post-Terminal Phase Activation

**Objective:** As an operator, I want a deterministic post-terminal phase that runs the distill sweep exactly once per issue per terminal transition, so that artifacts are swept after merge without blocking any prior state transition.

#### Acceptance Criteria
1. When an issue's per-`(repo, issue)` state machine transitions into `TerminalSuccess`, the roki daemon shall enqueue a distill sweep turn for that `(repo, issue)` before performing workspace deletion.
2. While the distill sweep is pending or running for an issue, the roki daemon shall hold the workspace deletion step for that `(repo, issue)` and shall not transition the workspace into the deleted state.
3. The roki daemon shall perform no Linear writes, no GitHub writes, and no PR-state mutations as part of activating the distill sweep.
4. If the issue's `TerminalSuccess` transition is reverted to `Active` by a prior subscriber veto or by a tracker event before the sweep is enqueued, the roki daemon shall cancel the pending sweep for that `(repo, issue)`.
5. The roki daemon shall emit a structured log event identifying the `(repo, issue)` key, the trigger source, and the correlation identifier for every distill sweep activation and cancellation.

### Requirement 2: Merge Detection Signal Source

**Objective:** As a system designer, I want merge detection to come from the agent's observation rather than from daemon-side polling of GitHub, so that the daemon stays free of GitHub state coupling and respects the roki-mvp boundary.

#### Acceptance Criteria
1. The roki daemon shall not issue any `gh` CLI command and shall not call any GitHub API to determine merge state for the purpose of triggering the distill sweep.
2. When the agent observes that Linear has transitioned the issue to `Done`, the agent shall be the actor that signals the daemon (via the existing roki-mvp tracker path) that the issue has reached a terminal state.
3. Where the agent uses `gh pr view --json mergedAt` from inside the workspace to corroborate merge state before transitioning Linear, the agent shall do so under its own permission scope and the roki daemon shall not intermediate that call.
4. The roki daemon shall accept the same `TerminalSuccess` transition signal that roki-mvp already publishes and shall add no new daemon-side polling source for merge detection in this spec.

### Requirement 3: Sweep Turn Dispatch

**Objective:** As an operator, I want the distill sweep to run as a single bounded turn against the agent inside the issue's existing workspace, so that the sweep cost is bounded and observable.

#### Acceptance Criteria
1. When the daemon enqueues a distill sweep, the roki daemon shall dispatch one agent turn against the existing per-issue Claude Code session in that issue's workspace.
2. The roki daemon shall enforce a configurable sweep turn budget separate from the implementation-phase turn budget and shall stop sending continuation prompts once that sweep budget is exhausted.
3. While the sweep turn is running, the roki daemon shall apply the same stall-detection window used by roki-mvp's engine policy.
4. If the sweep turn exits non-cleanly or exhausts the sweep turn budget without producing a valid manifest, the roki daemon shall mark the distill phase as failed for that `(repo, issue)`, log the failure, retain the workspace for inspection, and refuse terminal cleanup until an operator intervenes.
5. The roki daemon shall emit one structured log event for sweep turn start and one for sweep turn completion, each carrying the `(repo, issue)` key and the correlation identifier.

### Requirement 4: Artifact Discovery Configuration

**Objective:** As an operator, I want artifact discovery paths and classification rules to be expressible in `WORKFLOW.md`, so that I can adapt distill behavior per repo without code changes.

#### Acceptance Criteria
1. The `WORKFLOW.md` schema shall expose a `distill.paths` key that accepts a list of workspace-relative path patterns under which the agent shall search for artifacts during the sweep.
2. The `WORKFLOW.md` schema shall expose a `distill.routes` key that accepts a list of classification rules associating path patterns or filename patterns with one of `delete`, `archive`, or `distill` dispositions.
3. While `distill.paths` is unset for a repository, the agent shall use a documented default set of paths covering at least `.kiro/specs/<issue>/` for that issue.
4. The `WORKFLOW.md` loader shall accept additions to `distill.paths` and `distill.routes` via hot reload and shall apply the new values to subsequent distill sweeps without daemon restart.
5. If `distill.paths` or `distill.routes` fails schema validation, the WORKFLOW.md loader shall keep the previously valid policy in effect for that repository and shall log the validation failure.

### Requirement 5: Artifact Classification and Three Dispositions

**Objective:** As an operator, I want every discovered artifact to be classified into delete, archive, or distill with a documented default-conservative tie-break, so that no artifact is silently lost and no useful context is silently deleted.

#### Acceptance Criteria
1. When the agent classifies an artifact, the agent shall assign exactly one of the dispositions `delete`, `archive`, or `distill` and shall record the chosen disposition in the manifest entry for that artifact.
2. When `distill.routes` rules in `WORKFLOW.md` produce a definite disposition for an artifact, the agent shall apply that disposition without further judgment.
3. While classification under `distill.routes` is ambiguous or absent, the agent shall fall back to a default-conservative disposition of `archive` and shall record `archive` as the chosen disposition with a reason that names the ambiguity.
4. The agent may consult `review.md` and `requirements.md` (when present in the workspace) as inputs that inform classification decisions; the agent shall not be required to consult them when they are absent.
5. The agent shall never assign a disposition outside the set `{delete, archive, distill}` for any artifact recorded in the manifest.

### Requirement 6: Disposition Execution

**Objective:** As an operator, I want each disposition to perform a precisely defined filesystem effect, so that the manifest accurately reflects the on-disk outcome.

#### Acceptance Criteria
1. When the agent assigns the `delete` disposition to an artifact, the agent shall remove the artifact from its discovered path and shall record the original path and a deletion marker in the manifest entry.
2. When the agent assigns the `archive` disposition to an artifact, the agent shall move the artifact verbatim under `.kiro/archive/<issue>/` preserving its relative path beneath the archive root and shall record the original path and the archive path in the manifest entry.
3. When the agent assigns the `distill` disposition to an artifact, the agent shall extract the canonical content into the disposition's documented stable home (for example a project-level EARS document or a decisions directory configured under `distill.routes`) and shall record both the source path and the distilled output path in the manifest entry.
4. If a disposition execution fails for any artifact, the agent shall record the failure in the manifest with the artifact's original path, the attempted disposition, and a structured failure reason, and shall not silently drop the artifact from the manifest.
5. The agent shall not modify any file outside the workspace root or the configured project archive root when executing dispositions.

### Requirement 7: Manifest Schema and Authorship

**Objective:** As a daemon validator, I want a stable, version-tagged manifest schema that enumerates every discovered artifact and its outcome, so that I can validate the sweep's result without reading artifact contents.

#### Acceptance Criteria
1. The agent shall write the distill manifest to `.kiro/specs/<issue>/distill-manifest.json` exactly once per sweep turn.
2. The manifest shall include a top-level `schema_version` field whose value names a stable, documented version identifier published in `SPEC.md`.
3. The manifest shall include an `entries` array in which each entry names the original artifact path, the chosen disposition, the resulting destination path or deletion marker, the rule source (rule id from `distill.routes` or the literal `default-archive`), and a timestamp.
4. The manifest shall include a `summary` object that totals the count of entries per disposition and that names whether any entries reported execution failures.
5. The manifest shall be written as a complete file in a single write; partial or streaming manifests are not permitted.

### Requirement 8: Daemon-Side Manifest Validation

**Objective:** As an operator, I want the daemon to validate the manifest against its schema and path-safety invariants before terminal cleanup, so that a malformed or unsafe sweep cannot cause data loss or escape the workspace.

#### Acceptance Criteria
1. When the sweep turn completes, the roki daemon shall read `.kiro/specs/<issue>/distill-manifest.json` and shall validate it against the published manifest JSON-Schema for the declared `schema_version`.
2. If the manifest fails schema validation, the roki daemon shall mark the distill phase as failed for that `(repo, issue)`, retain the workspace, log the offending field, and refuse terminal cleanup until an operator intervenes.
3. The roki daemon shall validate every path mentioned in the manifest against the path-safety invariants reused from roki-mvp's workspace module so that no destination path lies outside the workspace root or the configured project archive root.
4. If any manifest path fails path-safety validation, the roki daemon shall mark the distill phase as failed, retain the workspace, log the offending path, and refuse terminal cleanup.
5. The roki daemon shall not interpret the contents of any artifact mentioned in the manifest, shall not invoke an LLM during validation, and shall not classify artifacts itself.

### Requirement 9: Idempotency and Re-Run Safety

**Objective:** As an operator, I want re-running the sweep on an already-distilled issue to be a safe no-op, so that retries, partial recovery, and operator-initiated re-runs do not corrupt prior outputs.

#### Acceptance Criteria
1. When the agent begins a sweep, the agent shall first check for the presence of `.kiro/specs/<issue>/distill-manifest.json` and, if found and matching a recognized `schema_version`, shall treat the sweep as already complete and shall write no new manifest.
2. While the manifest indicates an already-completed sweep, the agent shall not move, delete, distill, or otherwise modify any artifact during the sweep turn.
3. When the daemon reactivates the post-terminal phase for an issue that already has a valid manifest, the roki daemon shall accept the existing manifest as the validation input and shall proceed to terminal cleanup without re-dispatching a sweep turn.
4. If the existing manifest declares an unrecognized `schema_version`, the roki daemon shall mark the distill phase as failed for that `(repo, issue)`, retain the workspace, and log the unrecognized version rather than overwriting the manifest.
5. The agent shall not assume the previous sweep's disposition outcomes are still on disk; the manifest is the source of truth for what happened, and the agent shall not retry executions during a recognized re-run.

### Requirement 10: Stable Archive Path Scheme

**Objective:** As an operator, I want archived artifacts to land under a stable, predictable path scheme inside the workspace, so that future tooling can locate prior archives deterministically.

#### Acceptance Criteria
1. The agent shall use `.kiro/archive/<issue>/` (resolved relative to the workspace root) as the archive root for all `archive`-disposition artifacts within a given sweep.
2. While archiving an artifact whose original path is workspace-relative, the agent shall preserve the artifact's original relative path beneath the archive root so that the archive path mirrors the original path.
3. If an archive destination already exists from a prior run with a recognized manifest, the agent shall treat the destination as authoritative and shall not overwrite it.
4. The agent shall not place archived artifacts outside `.kiro/archive/<issue>/` or any additional archive root explicitly declared in `distill.routes`.
5. The roki daemon shall validate every `archive`-disposition destination path in the manifest against the documented archive path scheme and shall reject manifests whose archive paths violate the scheme.

### Requirement 11: Path Safety and Write Containment

**Objective:** As an operator running the agent under `workspace-write`, I want all distill writes to be contained within the workspace and any explicitly configured project archive roots, so that a misclassification or buggy rule cannot reach beyond declared boundaries.

#### Acceptance Criteria
1. The agent shall restrict every write effect performed during the sweep to paths that lie within the issue's workspace root.
2. Where the operator has configured an additional project archive root in `WORKFLOW.md`'s `distill.routes`, the agent may also write within that configured root, and the roki daemon shall include that root in its path-safety allowlist for the manifest validation.
3. The roki daemon shall reuse the path-safety module published by roki-mvp's workspace component to enforce containment for every manifest path it inspects.
4. If a sweep produces any manifest entry whose paths fail containment, the roki daemon shall refuse terminal cleanup, log the offending paths, and surface the failure as a distill phase failure for that `(repo, issue)`.
5. The agent shall not rely on symlinks, hard links, or filesystem aliases to escape containment; the daemon shall reject manifest paths that resolve outside the allowed roots after canonicalization.

### Requirement 12: Failure Modes and Operator Recovery

**Objective:** As an operator, I want distill phase failures to leave the workspace recoverable and the issue diagnosable, so that I can retry or intervene without losing the artifacts the sweep was meant to handle.

#### Acceptance Criteria
1. When a distill phase fails for any reason (sweep turn failure, manifest schema failure, path-safety failure), the roki daemon shall retain the issue's workspace on disk and shall not delete it.
2. The roki daemon shall record a distill phase failure as a per-`(repo, issue)` state observable through the same structured logs roki-mvp uses for worker failures.
3. While a distill phase is in a failed state for an issue, the roki daemon shall not re-dispatch the sweep automatically and shall wait for operator action before re-attempting.
4. When the operator manually clears the failed manifest (or the operator configures a re-run), the roki daemon shall permit a fresh sweep turn on the next activation while still honoring idempotency for any unchanged completed manifests.
5. The roki daemon shall surface every distill phase failure with sufficient log context (offending path or field, rule source if applicable, schema version) for the operator to diagnose without re-reading artifact contents.

### Requirement 13: Observability of the Distill Phase

**Objective:** As an operator debugging a stuck or unexpected sweep, I want enough structured observability of the distill phase to diagnose issues without bespoke tooling, so that the phase remains operable before any dedicated UI exists.

#### Acceptance Criteria
1. The roki daemon shall emit a structured log event for distill sweep activation, sweep turn start, sweep turn completion, manifest validation start, manifest validation outcome, and terminal cleanup gating decision.
2. Every distill phase log event shall include the `(repo, issue)` key, a correlation identifier shared with the sweep turn, and an event-name field that downstream consumers can filter on.
3. The roki daemon shall redact any configured secrets from distill phase log events using the same redaction layer roki-mvp publishes.
4. The roki daemon shall include in each manifest validation outcome log event the `schema_version` of the manifest, the count of entries per disposition, and any path-safety failure details.
5. The roki daemon shall not log artifact contents, only artifact paths and the manifest's structured fields.

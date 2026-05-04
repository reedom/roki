---
name: roki-classify
description: Daemon-purpose-built classifier for roki. Determines whether a Linear ticket should auto-implement directly (Path B) or needs operator-side spec authoring (Path A / C / D / E). Equivalent to kiro-discovery Step 1 (lightweight scan) + Step 2 (action-path determination), with no dialogue and a structured exit envelope. Invoked by the roki daemon's orchestrator session in NEEDS_CLASSIFY mode as the first phase.
disable-model-invocation: true
allowed-tools: Read, Glob, Grep
argument-hint: <ticket-context-json-or-path>
---

# roki-classify Skill

## Role

Single-purpose, daemon-driven classifier. You receive a Linear ticket's context (id, title, body, labels) and the project's existing spec inventory. You return one of five action paths plus a recommended next manual command and Linear label for the operator. You do not write files, do not engage in dialogue, do not propose approaches, and do not author requirements.

This skill is the daemon-side counterpart of `kiro-discovery`'s Step 1 + Step 2. Where `kiro-discovery` is an interactive authoring-time tool, this one runs headless in a phase subprocess and emits a machine-parseable structured exit so the orchestrator can branch on it.

## Core Mission

- **Success Criteria**:
  - Correct action path identified based on existing project state and the ticket scope
  - Structured exit envelope emitted in the final stream-json `result` event
  - No file writes, no dialogue, no follow-up suggestions beyond the recommended command + label
  - Total `--max-turns` budget bounded at 5

## Execution Steps

### Step 1: Lightweight Scan

Gather **only metadata**. Do NOT read full file contents.

- **Specs inventory**: Glob `.kiro/specs/*/spec.json`, read each `spec.json` for `name`, `phase`, and `approvals` fields. Note which feature names exist and which are approved through to `tasks`.
- **Steering existence**: Check which files exist in `.kiro/steering/` (`product.md`, `tech.md`, `structure.md`, `roadmap.md`). Do NOT read their contents yet.
- **Roadmap**: If `.kiro/steering/roadmap.md` exists, read it to restore project-level context (approach, scope, spec list, dependency order).
- **Top-level structure**: List the project root to note key directories. Do NOT recurse.

Budget: this step should consume well under one full turn. Greenfield projects (`specs/` empty, no steering) are valid input.

### Step 2: Determine Action Path

Match the ticket's request against the metadata gathered in Step 1. Pick exactly one path:

- **Path A — Existing spec covers this**: the request is an extension, enhancement, or fix within an existing spec's domain. Every meaningful part of the request fits that same spec boundary. The operator should run `/kiro-spec-requirements <feature>` (followed by design / tasks) to extend the existing spec, then re-dispatch to roki with the `roki:impl` label.
- **Path B — No spec needed**: the request is a bug fix, config change, simple refactor, or trivial addition. The ticket body's `## Acceptance Criteria` (numbered EARS) are sufficient as the spec. roki can auto-implement.
- **Path C — New single-scope feature**: the request is new, doesn't overlap with existing specs, and fits in one spec. The operator should run `/kiro-spec-init <feature-name>` (or `/kiro-spec-quick <feature-name>` for fast path), then re-dispatch with `roki:impl`.
- **Path D — Multi-scope decomposition needed**: the request spans multiple domains or would produce 20+ tasks in a single spec. The operator should run `/kiro-discovery <idea>` to decompose, then `/kiro-spec-batch`.
- **Path E — Mixed decomposition**: the request contains a mix of existing-spec extensions, new spec candidates, and direct-implementation work. Same path as D from the operator's perspective: run `/kiro-discovery <idea>` to decompose.

Conservative bias: when uncertain between Path B and any of A / C / D / E, prefer the spec-authoring path. Auto-implementing a ticket that needed authoring produces low-quality output; bouncing a small ticket back to the operator costs only one operator action.

### Step 3: Emit Structured Exit

Emit a final stream-json `result` event with `subtype: success` and the following payload:

```json
{
  "path": "A" | "B" | "C" | "D" | "E",
  "target_feature": "<existing-feature-name-when-Path-A>" | null,
  "suggested_command": "/<command-the-operator-should-run>",
  "suggested_label": "roki:impl" | "<other-label>" | null,
  "rationale": "<= 200 chars, plain text, why this path was chosen>"
}
```

Field semantics:

- `path` is required.
- `target_feature` is required when `path == "A"`; the existing spec name the request extends. `null` for B / C / D / E.
- `suggested_command` is required and recommends the next operator-side command (e.g. `/kiro-spec-requirements <name>` for A, `/kiro-spec-init <name>` for C, `/kiro-discovery <idea>` for D / E). For Path B set it to `null` since no operator action is needed.
- `suggested_label` is the Linear label the operator should add when the recommended work completes (typically `roki:impl` so the next webhook re-dispatches in SPEC_DRIVEN mode). `null` for Path B.
- `rationale` is a short, machine-loggable explanation; not consumed by the state machine.

Do not write any file. Do not propose approaches. Do not ask questions. Do not chain to other skills.

## Critical Constraints

- **No dialogue**: there is no operator on the other end. Do not call AskUserQuestion. Do not pause for confirmation.
- **No file writes**: the manifest does not allow Write or Edit. Attempting them is an error.
- **No subagent dispatch**: the manifest does not allow Agent. Classification is done in this turn.
- **Bounded turns**: the daemon launches this skill with `--max-turns 5`. Step 1 + Step 2 + Step 3 should complete in 1–3 turns; an Agent dispatch chain is unnecessary at this scale.
- **Conservative bias**: when in doubt, return Path A / C / D / E (operator handoff) rather than Path B (auto-implement). False-positive auto-implements are costlier than false-negative handoffs.
- **No spec authoring**: this skill never produces `brief.md`, `requirements.md`, or any other spec artifact. That is the operator's job after they receive the recommended command.

## Output Description

The terminal stream-json `result` event carries the JSON payload above. The orchestrator reads the payload from the daemon's `phase_complete(classify)` event and branches:

- `path == "B"` → orchestrator nominates `implement` (direct mode) next
- `path ∈ {"A", "C", "D", "E"}` → orchestrator writes a Linear comment quoting `suggested_command` and `suggested_label`, then `action=stop outcome=needs_operator`

## Safety & Fallback

**Ticket body missing `## Acceptance Criteria`**: do not silently assume Path B. Return Path C (or A if there is an obvious target spec) with a `rationale` noting the missing criteria; the operator's spec-authoring step will repair the gap.

**Ticket scope unclear**: return Path C with `rationale="ticket scope insufficient for confident classification; recommend spec authoring"`. Do not attempt Path B.

**`spec.json` not parseable for an existing spec**: skip that spec for matching purposes and note it in `rationale`. Do not fail the classification on a broken spec dir; the daemon will surface that separately.

**Steering files missing entirely (greenfield)**: Path C / D / E remain valid; the recommended command should include `/kiro-steering` as a prerequisite when the project has no steering at all.

# Grounded Design

> Build the smallest practical system from materials that actually exist. Cite the source for every concrete claim.

This file exists because past iterations of roki's spec set drifted in two failure modes:

1. **Hallucinated affordances** — referenced Claude Code tools, kiro skills, and MCP capabilities that did not exist in the operator's installed surface (e.g. invented subagents like `kiro-implement` / `kiro-self-review` / `kiro-lint-test`, or daemon-registered diagnostic tools that were not actually wired up).
2. **Scope inflation** — added phases, states, namespaces, and side-channels with no concrete pull (e.g. multi-repo per ticket, distill phase, Slack channel, 8-state machine, two-pass verification rubric). Each addition justified itself locally; together they bloated the system past MVP.

The rules below are how to avoid both.

---

## Principle 1: Inventory before you cite

Before naming a concrete affordance in any spec / FR / requirement, confirm it exists in **one of these sources**:

| Affordance class | Authoritative source |
|---|---|
| Claude Code CLI flags | the project roadmap's "Constraints > Engine" line and `docs/reference/cli.md` |
| kiro skills | `.claude/skills/kiro-*/SKILL.md` (project) and `~/.claude/skills/kiro-*/SKILL.md` (operator) |
| MCP tools | operator's installed MCP server surface; the daemon adds nothing |
| Daemon CLI / config keys | `docs/reference/cli.md` and `docs/reference/config.md` |
| Structured log events | `docs/reference/log-events.md` |
| Extension surface (traits, hooks, namespaces) | `docs/reference/extension-surface.md` |
| Per-issue artifacts | `docs/reference/artifacts.md` |

If a name does not appear in one of these, either (a) add it deliberately to the source first and cite it, or (b) remove the name from your draft. **Plausible-sounding placeholders are the root cause of hallucination.**

### Worked example: skill names

The kiro skill set is **what `.claude/skills/` actually contains**. Runtime skills (used inside the worker invocation): `kiro-impl`, `kiro-review`, `kiro-debug`, `kiro-validate-impl`, `kiro-verify-completion`. Authoring-time skills (operator-side, before admission): `kiro-spec-*`, `kiro-discovery`, `kiro-validate-design`, `kiro-validate-gap`, `kiro-steering`, `kiro-steering-custom`.

Anything else is invention. If a workflow needs an affordance not in this list, the cheap fix is to compose existing skills (`kiro-impl` already drives an implementer-then-reviewer loop per task; you do not need a separate `kiro-fix-finding`). The expensive fix is to add a new skill — and that requires a SKILL.md file actually written, not a name in a doc.

---

## Principle 2: Headless engine constraints are real

The daemon launches two shapes of `claude` process:

- **Orchestrator session (A)** as `claude --input-format stream-json --output-format stream-json` — long-lived, reads daemon-produced JSON events on stdin, returns strict JSON action directives on stdout. Tool surface restricted to Linear MCP (write) + Read.
- **Phase subprocesses** as `claude -p '/<kiro-skill> <args>' --output-format stream-json` — one-shot, slash-command-driven. **Slash commands work as the initial prompt argument in `-p` mode** (verified — the prompt string is parsed before headless takes over). Per-phase `--max-turns` budget.

This implies:

- **No interactive prompts** mid-session — anything that needs human input runs through Linear MCP (post comment, poll for reply) or fails closed.
- **The agent's tool surface is the operator's installation verbatim** — the daemon does not register, proxy, or wrap any agent-side tool. If a spec wants the worker to do X, X must be reachable via the operator's existing Claude Code tool set (built-ins + their MCPs). The orchestrator session further narrows that surface via `--settings` (Linear MCP + Read only).

If a draft assumes the daemon hands the agent a custom tool, it is wrong. Rewrite to compose existing affordances.

---

## Principle 3: Two-shape engine taxonomy

The daemon orchestrates one ticket via:

- **One orchestrator session (A)** — long-lived, makes admission / phase-planning / Linear-write directive decisions. Configured via `extension.orchestrator.{model, effort, max_phases, allowed_tools}`.
- **Zero or more phase subprocesses** — bounded `claude -p '/kiro-* <args>'` calls A nominates. Phase catalog: `implement` (kiro-impl), `validate` (kiro-validate-impl), `open_pr` (custom prompt), `ci_fix` (kiro-debug + kiro-verify-completion), `finalize_review` (synthesizes review.md).
- **Setup-judge subprocess and linear-updater subagent are removed** — both functions are absorbed by A (admission_request event + daemon_directive event respectively).

Drafts that introduce a new daemon-launched `claude` shape outside this taxonomy (a side review subprocess, a separate distill turn, a summarization session, etc.) are scope inflation. The work belongs inside an existing phase or does not belong in the daemon at all. Adding a new phase to the catalog is allowed but must satisfy Principle 4 (concrete pull, not hypothetical extensibility).

---

## Principle 4: State and config minimalism

Before adding a new state, a new `Inactive.reason`, a new `extension.*` namespace, or a new daemon-only failure category, ask: **does an existing one already cover this?** The 6-state machine + `Inactive.reason` discriminator is the budget. New entries cost downstream coordination; reuse is free.

Concrete pull = a real failure path that cannot be expressed in the current vocabulary. Hypothetical extensibility is not concrete pull.

---

## Principle 5: Cite, don't paraphrase

When an FR refers to behavior that lives in another doc (CLI flag, config key, log event, artifact field), link to the canonical reference doc, do not restate the contract. Paraphrases drift; links don't. The cross-reference graph (`docs/kinds.md` + `roki-doctools validate`) catches structural drift; prose drift only humans / agents catch.

If you find yourself restating a list (gate failure codes, log event names, namespace keys), stop and link instead.

---

## Principle 6: When in doubt, run the validator

After any non-trivial doc edit, the post-edit hook runs `roki-doctools validate` automatically. If validate flags a dangling reference, the reference is wrong — fix it instead of working around it. Validate is the cheapest layer of fact-checking the project has.

---

## Pre-edit checklist

For any draft that names a concrete affordance:

1. ☐ Does the named tool / skill / flag / event / namespace appear in the authoritative source from the table above?
2. ☐ Is the new addition justified by a concrete failure path (not by hypothetical future extensibility)?
3. ☐ Does the change preserve the orchestrator-session boundary? A is the only "thinking" component and the only Linear writer; phase subprocesses are the only code-changing components; the daemon never makes "what should happen next" judgements on its own.
4. ☐ Are restated lists replaced with links to canonical reference docs?
5. ☐ Does `roki-doctools validate` pass after the edit?

If any answer is "no", the draft is not ready.

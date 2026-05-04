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

The worker runs as `claude --print --output-format stream-json`. This implies:

- **Slash commands do not work** in headless `-p` mode. Skills must auto-invoke by description.
- **No interactive prompts** — anything that needs human input runs through Linear MCP (post comment, poll for reply) or fails closed.
- **The agent's tool surface is the operator's installation verbatim** — the daemon does not register, proxy, or wrap any agent-side tool. If a spec wants the worker to do X, X must be reachable via the operator's existing Claude Code tool set (built-ins + their MCPs).

If a draft assumes the daemon hands the agent a custom tool, it is wrong. Rewrite to compose existing affordances.

---

## Principle 3: One bounded invocation per ticket

Per the roadmap: a single bounded `claude` invocation drives the implementation; the daemon does not relaunch the worker on its own initiative. The two exceptions are explicit and named:

- **Setup judge** before worker launch (one-shot bounded `claude`).
- **linear-updater** on daemon-only failures (one-shot bounded `claude`).
- **Review-gate `Deny+RetryWithContext`** re-launches the same worker once with `additional_context` populated.

Drafts that introduce new daemon-launched `claude` subprocesses for review, distill, summarization, etc., are scope inflation. The work belongs inside the existing worker invocation (skill-first), or it does not belong in the daemon at all.

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
3. ☐ Does the change keep the "single bounded `claude` invocation per ticket" boundary intact?
4. ☐ Are restated lists replaced with links to canonical reference docs?
5. ☐ Does `roki-doctools validate` pass after the edit?

If any answer is "no", the draft is not ready.

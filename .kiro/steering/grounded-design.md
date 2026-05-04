# Grounded Design

> Build the smallest practical system from materials that actually exist. Cite the source for every concrete claim.

Past iterations drifted in two failure modes:

1. **Hallucinated affordances** — referenced Claude Code tools, kiro skills, and MCP capabilities not in the operator's installed surface.
2. **Scope inflation** — added phases, states, namespaces, side-channels with no concrete pull.

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

The kiro skill set is **what `.claude/skills/` actually contains**. Runtime skills (worker invocation): `kiro-impl`, `kiro-review`, `kiro-debug`, `kiro-validate-impl`, `kiro-verify-completion`. Authoring-time skills (operator-side, pre-admission): `kiro-spec-*`, `kiro-discovery`, `kiro-validate-design`, `kiro-validate-gap`, `kiro-steering`, `kiro-steering-custom`.

Anything else is invention. Cheap fix: compose existing skills (`kiro-impl` already drives implementer-then-reviewer per task). Expensive fix: add a new skill with an actual SKILL.md file.

---

## Principle 2: Headless engine constraints are real

The daemon launches two shapes of `claude` process:

- **Orchestrator session (A)** as `claude --input-format stream-json --output-format stream-json` — long-lived, reads daemon-produced JSON events on stdin, returns strict JSON action directives on stdout. Tool surface restricted to Linear MCP (write) + Read.
- **Phase subprocesses** as `claude -p '/<kiro-skill> <args>' --output-format stream-json` — one-shot, slash-command-driven. **Slash commands work as the initial prompt argument in `-p` mode** (verified). Per-phase `--max-turns` budget.

Implications:

- **No interactive prompts** mid-session — human input goes through Linear MCP (post comment, poll for reply) or fails closed.
- **Tool surface is operator's installation verbatim** — the daemon never registers, proxies, or wraps agent-side tools. The orchestrator session further narrows via `--settings` (Linear MCP + Read only).

Drafts assuming the daemon hands the agent a custom tool are wrong. Compose existing affordances.

---

## Principle 3: Two-shape engine taxonomy

The daemon orchestrates one ticket via:

- **One orchestrator session (A)** — long-lived, makes admission / phase-planning / Linear-write directive decisions. Configured via `extension.orchestrator.{model, effort, max_phases, allowed_tools}`.
- **Zero or more phase subprocesses** — bounded `claude -p '/kiro-* <args>'` calls A nominates. Phase catalog: `implement` (kiro-impl), `validate` (kiro-validate-impl), `open_pr` (custom prompt), `ci_fix` (kiro-debug + kiro-verify-completion), `finalize_review` (synthesizes review.md).
- **Setup-judge subprocess and linear-updater subagent are removed** — absorbed by A (admission_request + daemon_directive events).

New daemon-launched `claude` shapes outside this taxonomy are scope inflation. Adding a new phase to the catalog must satisfy Principle 4 (concrete pull).

---

## Principle 4: State and config minimalism

Before adding a new state, `Inactive.reason`, `extension.*` namespace, or daemon-only failure category, ask: **does an existing one already cover this?** The 6-state machine + `Inactive.reason` discriminator is the budget.

Concrete pull = real failure path not expressible in current vocabulary. Hypothetical extensibility is not concrete pull.

---

## Principle 5: Cite, don't paraphrase

When an FR refers to behavior in another doc (CLI flag, config key, log event, artifact field), link to the canonical reference doc — do not restate the contract. Paraphrases drift; links don't.

If you find yourself restating a list (gate failure codes, log event names, namespace keys), stop and link instead.

---

## Principle 6: When in doubt, run the validator

The post-edit hook runs `roki-doctools validate` automatically. If validate flags a dangling reference, the reference is wrong — fix it, don't work around it.

---

## Pre-edit checklist

For any draft that names a concrete affordance:

1. ☐ Does the named tool / skill / flag / event / namespace appear in an authoritative source from the table above?
2. ☐ Is the addition justified by a concrete failure path (not hypothetical extensibility)?
3. ☐ Does the change preserve the orchestrator-session boundary? A is the only "thinking" component and only Linear writer; phase subprocesses are the only code-changing components; the daemon never decides "what should happen next."
4. ☐ Are restated lists replaced with links to canonical reference docs?
5. ☐ Does `roki-doctools validate` pass?

If any answer is "no", the draft is not ready.

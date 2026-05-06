---
paths: .kiro/specs/**/*.md
---

# No fake decisions

## Rule

If a question's answer is forced by a constraint already stated upstream — brief out-of-scope, requirement, prior decision, or canonical reference — **state the answer**. Do not frame it as an open option, a "research item", or a "two viable positions" pair.

A constraint forces an answer when, after reading the constraint, every alternative is either contradicted, redundant, or out of scope.

## Forms to avoid

- "Two viable positions" / "Three options" / "A vs B" framings whose losing branches violate something already written.
- "Recommendation: …" lines that restate a requirement or a prior forced deferral.
- "Research items" lists that contain forced conclusions instead of genuine external lookups or genuine open trade-offs.
- Trailing "Updated recommendations for design phase" sections that re-emit upstream requirements as if they were new decisions.

## Forms that are fine

- **Forced deferrals** stated as facts, with the upstream constraint cited (`fr:NN §...`, `Req N.M`, `brief out-of-scope: <spec-name>`). Useful for traceability so downstream specs know what to pick up.
- **Genuine open items** with non-trivial trade-offs that survive after upstream constraints are applied.
- **External-API doc lookups** (canonical schema not in this repo, third-party API shape) — name the lookup, do not invent options.

## Check before flagging anything as open

For each "open" item, ask in order:

1. Is one branch contradicted by the brief / requirements / a cited FR / canonical reference? → not open. State the answer; cite the constraint.
2. Are the branches behaviorally equivalent? → not open. Pick one; move on.
3. Does the answer just require reading an external doc? → it is a lookup, not a decision.
4. Only if 1–3 fail: it is a genuine open item.

## Test

For every option list, recommendation, and "research item": delete the framing and write only the conclusion plus the constraint citation. If the result still answers the original question, the framing was fake — keep the deletion.

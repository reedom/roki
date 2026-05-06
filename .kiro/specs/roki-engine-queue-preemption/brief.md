---
refs:
  id: brief:roki-engine-queue-preemption
  kind: brief
  title: "roki-engine-queue-preemption Brief"
  spec: roki-engine-queue-preemption
---

# Brief: roki-engine-queue-preemption

## Problem

Webhooks arriving while a ticket has an in-flight cycle either drop or interrupt the cycle.

## Desired Outcome

A mid-cycle webhook updates the in-memory diff cache to the new state but defers rule re-evaluation until the cycle ends. After the cycle, the daemon re-evaluates against the latest cached state — only the final state matters; retained webhooks are not replayed individually. Admission-filter loss mid-cycle (assignee revoked, repo allowlist drop) does not preempt; the cycle runs to natural end and the daemon then evicts and orphan-cleans.

## Scope

- **In**: deferred rule-eval queue; cache update on every webhook; post-cycle re-eval; non-preemption on admission loss + post-cycle orphan delete.
- **Out**: full Linear admission gate (`roki-linear-admission-repos`); diff cache itself (`roki-linear-diff-cache`).

## Dependencies

- roki-engine-iteration-loop

## Critical FR references

- fr:01-engine-model
- fr:03-linear-admission

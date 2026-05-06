---
refs:
  id: brief:roki-linear-diff-cache
  kind: brief
  title: "roki-linear-diff-cache Brief"
  spec: roki-linear-diff-cache
---

# Brief: roki-linear-diff-cache

## Problem

Skeleton's ticket cache is single-shot; full rule dispatch needs a per-ticket diff.

## Desired Outcome

In-memory cache `(ticket_id) → {status, labels, assignee, repo, workflow_path}`. Rule eval fires only when status / labels / assignee changed since the previous webhook. Eviction on assignee loss; eviction triggers post-cycle worktree + tempdir orphan delete.

## Scope

- **In**: cache CRUD; diff compute; eviction trigger; mid-cycle update path (queue-preemption-friendly).
- **Out**: persistent storage (deliberately none); cross-process sharing.

## Dependencies

- roki-skeleton

## Critical FR references

- fr:03-linear-admission
- fr:07-recovery

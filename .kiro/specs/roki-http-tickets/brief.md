---
refs:
  id: brief:roki-http-tickets
  kind: brief
  title: "roki-http-tickets Brief"
  spec: roki-http-tickets
---

# Brief: roki-http-tickets

## Problem

Observers need a snapshot of active tickets and their cycle state.

## Desired Outcome

`GET /api/v1/tickets` returns active tickets with cached fields (`status`, `labels`, `assignee`, `repo`) plus their current cycle state (kind, iter, in-flight phase, last directive).

## Scope

- **In**: cache snapshot serialization; cycle-state projection.
- **Out**: write / mutation; per-ticket detail endpoints (separate or follow-up).

## Dependencies

- roki-http-server

## Critical FR references

- fr:10-http-api

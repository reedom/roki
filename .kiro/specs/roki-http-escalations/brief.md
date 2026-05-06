---
refs:
  id: brief:roki-http-escalations
  kind: brief
  title: "roki-http-escalations Brief"
  spec: roki-http-escalations
---

# Brief: roki-http-escalations

## Problem

Observers cannot see no-handler failures except by reading the log.

## Desired Outcome

`GET /api/v1/escalations` returns the in-memory escalation queue: `{cycle_id, ticket_id, kind, phase, timestamp, error_text}` per design §6.4.

## Scope

- **In**: queue snapshot; serializer.
- **Out**: dismiss endpoint (operators dismiss by closing the corresponding Linear ticket); persistence.

## Dependencies

- roki-http-server
- roki-engine-failure-cycle

## Critical FR references

- fr:10-http-api
- fr:06-failure-handling

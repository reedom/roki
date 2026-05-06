---
refs:
  id: brief:roki-http-events
  kind: brief
  title: "roki-http-events Brief"
  spec: roki-http-events
---

# Brief: roki-http-events

## Problem

Observers need a live event tail over HTTP.

## Desired Outcome

`GET /api/v1/events?since=<cursor>` returns the slice of the ring buffer after the cursor as JSON. SSE / long-poll variant for follow mode.

## Scope

- **In**: ring-buffer cursor handler; SSE / chunked-transfer follow; serializer.
- **Out**: filter DSL.

## Dependencies

- roki-http-server
- roki-obs-ring-buffer

## Critical FR references

- fr:10-http-api

---
refs:
  id: brief:roki-tui-detail-view
  kind: brief
  title: "roki-tui-detail-view Brief"
  spec: roki-tui-detail-view
---

# Brief: roki-tui-detail-view

## Problem

Operators need per-ticket history (recent phase output, last directive, current cycle).

## Desired Outcome

Ticket detail panel rendering recent phase output for the focused ticket, fetched on focus from the events endpoint filtered by ticket id.

## Scope

- **In**: detail widget; HTTP fetch on focus; scroll; refresh.
- **Out**: live tail (handled by events view).

## Dependencies

- roki-tui-foundation
- roki-http-events

## Critical FR references

- fr:11-roki-tui

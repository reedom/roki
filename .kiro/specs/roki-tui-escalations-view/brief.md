---
refs:
  id: brief:roki-tui-escalations-view
  kind: brief
  title: "roki-tui-escalations-view Brief"
  spec: roki-tui-escalations-view
---

# Brief: roki-tui-escalations-view

## Problem

Escalations are not surfaced inside the TUI; operators must read the log.

## Desired Outcome

Escalation queue view rendering entries from `GET /api/v1/escalations`. Operator dismiss = closing the corresponding Linear ticket; the TUI itself does not write.

## Scope

- **In**: queue widget; correlation to ticket id; refresh.
- **Out**: in-app dismiss / mutation.

## Dependencies

- roki-tui-foundation
- roki-http-escalations

## Critical FR references

- fr:11-roki-tui

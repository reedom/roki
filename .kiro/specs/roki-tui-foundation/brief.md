---
refs:
  id: brief:roki-tui-foundation
  kind: brief
  title: "roki-tui-foundation Brief"
  spec: roki-tui-foundation
---

# Brief: roki-tui-foundation

## Problem

The TUI does not exist; later view specs need a shell to plug into.

## Desired Outcome

ratatui app shell with view router, key map, and an HTTP API client targeting `/api/v1/*`. Provides the trait surface that ticket / detail / events / escalations views compose against.

## Scope

- **In**: app loop; key bindings; view registry; HTTP client (reqwest or hyper); error / disconnect handling.
- **Out**: per-view rendering (separate Wave 8 specs).

## Dependencies

- roki-http-tickets

## Critical FR references

- fr:11-roki-tui

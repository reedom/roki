---
refs:
  id: brief:roki-http-server
  kind: brief
  title: "roki-http-server Brief"
  spec: roki-http-server
---

# Brief: roki-http-server

## Problem

TUI and external observers need a stable read surface.

## Desired Outcome

axum HTTP server bound to `[network].bind` / `port`; `/api/v1/*` versioned namespace; loopback-only by default. Read-only endpoints; no write paths.

## Scope

- **In**: server boot; route registry; version path prefix; loopback-only default.
- **Out**: write endpoints; auth tokens (deferred).

## Dependencies

- roki-skeleton

## Critical FR references

- fr:10-http-api

---
refs:
  id: brief:roki-tui-events-view
  kind: brief
  title: "roki-tui-events-view Brief"
  spec: roki-tui-events-view
---

# Brief: roki-tui-events-view

## Problem

A live event stream is not visible without `roki events --follow`.

## Desired Outcome

Live event tail subscribed to `/api/v1/events` follow mode; scroll buffer; pause / resume.

## Scope

- **In**: stream client; bounded scroll buffer; pause / resume key bindings.
- **Out**: filter DSL (post-MVP).

## Dependencies

- roki-tui-foundation
- roki-http-events

## Critical FR references

- fr:11-roki-tui

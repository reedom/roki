---
refs:
  id: brief:roki-tui-tickets-view
  kind: brief
  title: "roki-tui-tickets-view Brief"
  spec: roki-tui-tickets-view
---

# Brief: roki-tui-tickets-view

## Problem

Active tickets are unobservable from a terminal.

## Desired Outcome

Ticket list view rendering active tickets and their cycle state from `GET /api/v1/tickets`. Sortable; basic filter (status / label).

## Scope

- **In**: list widget; sort / filter; selection → focus.
- **Out**: detail view (`roki-tui-detail-view`).

## Dependencies

- roki-tui-foundation

## Critical FR references

- fr:11-roki-tui

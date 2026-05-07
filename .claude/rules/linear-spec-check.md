---
paths:
  - crates/**/linear/**
  - docs/fr/**/*.md
---

# Verify Linear API claims against the official spec

## Rule

Before writing or editing code, FR docs, requirements, design, or tasks that touch the Linear API surface, fetch the current Linear spec via the **context7 MCP** and verify every concrete claim against it.

Concrete claims include:

- HTTP method, endpoint, header name, signature algorithm.
- Webhook payload field paths and types (e.g. `data.assignee.id`, `data.state.name`, `data.labels[].name`).
- GraphQL operation names, input shapes, return shapes.
- Rate limits, retry schedules, replay-protection windows.
- Authentication method (OAuth2 vs personal API key) and required headers.

## How to apply

1. Resolve the library id once per session: `mcp__context7__resolve-library-id` with `libraryName: "Linear API"` (canonical id: `/websites/linear_app_developers`; SDK: `/linear/linear`).
2. Query the relevant area via `mcp__context7__query-docs` before committing the wording or the code path.
3. Cite the verified shape in the doc / commit / PR description so reviewers can re-verify cheaply.
4. If context7 does not return a confirmed number or shape, **drop the specific claim** rather than inventing one. State the contract abstractly and reference Linear's published docs instead.

## Why

Linear's API evolves. Numeric rate limits, header names, and webhook field paths drift between Linear's docs and stale memory. Hard-coded numbers in FR docs ("5,000 req/hr") and webhook field paths in code (`data.id` vs `id`) are exactly the kind of claim that silently rots and causes 401 / 4xx incidents in production. A single context7 lookup at authoring time prevents that.

## Test

For every Linear-related sentence or code path: ask "where does this number / field path / header name come from?" If the answer is "memory" or "another file in this repo that I did not verify against Linear's spec", run a context7 query before merging.

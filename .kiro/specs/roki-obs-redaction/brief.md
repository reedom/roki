---
refs:
  id: brief:roki-obs-redaction
  kind: brief
  title: "roki-obs-redaction Brief"
  spec: roki-obs-redaction
---

# Brief: roki-obs-redaction

## Problem

Secrets (Linear API token, MCP-installed credentials) leak via tracing emissions.

## Desired Outcome

Redaction layer in the tracing pipeline: known secret field names (`[linear].token`, configured deny-list) are replaced with `***` before file / stdout / ring-buffer emission.

## Scope

- **In**: redaction layer position in tracing pipeline; deny-list config; substring scrub for inline values.
- **Out**: log encryption; PII-class detection beyond explicit field names.

## Dependencies

- roki-obs-tracing-pipeline

## Critical FR references

- fr:08-observability-logs

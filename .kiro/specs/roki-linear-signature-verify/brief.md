---
refs:
  id: brief:roki-linear-signature-verify
  kind: brief
  title: "roki-linear-signature-verify Brief"
  spec: roki-linear-signature-verify
---

# Brief: roki-linear-signature-verify

## Problem

Skeleton accepts unsigned webhooks; production requires HMAC verification.

## Desired Outcome

HMAC verification against `[linear].webhook_secret` on every inbound webhook. Reject missing or invalid signature with a structured log event and 401 response.

## Scope

- **In**: header read; HMAC SHA256 compare (constant-time); reject path; structured log on reject.
- **Out**: replay protection / nonce cache (deferred).

## Dependencies

- roki-skeleton

## Critical FR references

- fr:03-linear-admission

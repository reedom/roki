#!/usr/bin/env bash
# PostToolUse hook for the cross-reference graph.
#
# Claude Code sends the hook payload as JSON over stdin. We extract
# `tool_input.file_path`, dispatch to roki-doctools, and surface the
# result back to Claude via `hookSpecificOutput.additionalContext`
# (which is rendered next to the tool result).
#
# Dispatch:
#   *.md edits                                  → roki-doctools validate
#   source / spec / steering / kinds.md edits   → roki-doctools touched <file>
#
# A clean validate (`OK (N docs)`) produces no additionalContext — the hook
# stays out of the way when nothing is wrong. Validate failures and `touched`
# output are always injected so the agent reads them next turn.
#
# The hook never exits non-zero — it's purely informational.

set -u

cd "${CLAUDE_PROJECT_DIR:-$(pwd)}"

payload="$(cat)"
[[ -z "$payload" ]] && exit 0

# --- extract file_path ---
abs_path=""
if command -v jq >/dev/null 2>&1; then
  abs_path="$(printf '%s' "$payload" | jq -r '.tool_input.file_path // empty')"
elif command -v python3 >/dev/null 2>&1; then
  abs_path="$(printf '%s' "$payload" | python3 -c '
import json, sys
try:
    d = json.load(sys.stdin)
    print((d.get("tool_input") or {}).get("file_path", "") or "")
except Exception:
    pass')"
else
  exit 0
fi
[[ -z "$abs_path" ]] && exit 0

file="${abs_path#"$PWD/"}"
[[ -z "$file" ]] && exit 0

# --- pick binary ---
bin=""
if [[ -x "target/release/roki-doctools" ]]; then
  bin="./target/release/roki-doctools"
elif command -v roki-doctools >/dev/null 2>&1; then
  bin="$(command -v roki-doctools)"
else
  bin="cargo run --release -q -p roki-doctools --"
fi

# --- run the check ---
out=""
case "$file" in
  *.md)
    out="$($bin validate 2>&1 || true)"
    # Suppress the silent-success line (`OK (N docs)`) so we don't add noise.
    if [[ "$out" =~ ^OK ]]; then
      out=""
    fi
    ;;
  crates/*|.kiro/specs/*|.kiro/steering/*|docs/kinds.md)
    out="$($bin touched "$file" 2>&1 || true)"
    ;;
esac

[[ -z "$out" ]] && exit 0

# --- emit additionalContext to Claude ---
if command -v jq >/dev/null 2>&1; then
  printf '%s' "$out" | jq -Rs '{
    hookSpecificOutput: {
      hookEventName: "PostToolUse",
      additionalContext: .
    }
  }'
elif command -v python3 >/dev/null 2>&1; then
  printf '%s' "$out" | python3 -c '
import json, sys
ctx = sys.stdin.read()
print(json.dumps({
    "hookSpecificOutput": {
        "hookEventName": "PostToolUse",
        "additionalContext": ctx,
    }
}))'
else
  # No JSON encoder available: fall back to bare stderr.
  printf '%s\n' "$out" >&2
fi

exit 0

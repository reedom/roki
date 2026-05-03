#!/usr/bin/env bash
# PostToolUse hook for the cross-reference graph.
#
# Claude Code sends the hook payload as JSON over stdin. We extract
# `tool_input.file_path` (and `tool_name`), dispatch to roki-doctools,
# detect "judgment-needed" triggers from .claude/rules/refs.md, and
# surface the result back to Claude via
# `hookSpecificOutput.additionalContext`.
#
# Mechanical checks:
#   *.md edits                                  -> roki-doctools validate
#   source / spec / steering / kinds.md edits   -> roki-doctools touched <file>
#
# Judgment excerpts (appended when the corresponding trigger matches):
#   1. New .md under managed paths
#   2. Edit to .kiro/specs/*/requirements.md
#   3. Edit to crates/* source
#   4. Edit to docs/kinds.md
#   5. Existing graph-linked .md edited -> surface immediate links via `show`,
#      and (when the file is a requirements.md) per-`provides:` consumers via
#      `impact --depth 1`. Reminds the operator to keep linked-doc bodies in sync.
#
# A clean validate (`OK (N docs)`) on its own produces no additionalContext;
# the hook only emits when there's a mechanical hit OR a judgment trigger.
#
# The hook never exits non-zero -- it's purely informational.

set -u

cd "${CLAUDE_PROJECT_DIR:-$(pwd)}"

payload="$(cat)"
[[ -z "$payload" ]] && exit 0

# --- extract file_path and tool_name ---
abs_path=""
tool_name=""
if command -v jq >/dev/null 2>&1; then
  abs_path="$(printf '%s' "$payload" | jq -r '.tool_input.file_path // empty')"
  tool_name="$(printf '%s' "$payload" | jq -r '.tool_name // empty')"
elif command -v python3 >/dev/null 2>&1; then
  abs_path="$(printf '%s' "$payload" | python3 -c '
import json, sys
try:
    d = json.load(sys.stdin)
    print((d.get("tool_input") or {}).get("file_path", "") or "")
except Exception:
    pass')"
  tool_name="$(printf '%s' "$payload" | python3 -c '
import json, sys
try:
    d = json.load(sys.stdin)
    print(d.get("tool_name", "") or "")
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

# --- mechanical check ---
out=""
case "$file" in
  *.md)
    out="$($bin validate 2>&1 || true)"
    # Suppress the silent-success line so a clean validate adds no noise.
    if [[ "$out" =~ ^OK ]]; then
      out=""
    fi
    ;;
  crates/*|.kiro/specs/*|.kiro/steering/*|docs/kinds.md)
    out="$($bin touched "$file" 2>&1 || true)"
    ;;
esac

# --- judgment triggers (excerpts of .claude/rules/refs.md) ---
judgment=""

is_new_md=false
if [[ "$tool_name" == "Write" && "$file" == *.md ]]; then
  if ! git ls-files --error-unmatch -- "$file" >/dev/null 2>&1; then
    is_new_md=true
  fi
fi

# Trigger 1: new .md file.
if $is_new_md; then
    judgment+='
## Judgment: new `.md` file

Pick kind by matching the file path against `docs/kinds.md` `path_globs`:
- Path matches an existing kind glob -> add `refs:` of that kind, mirroring the closest sibling.
- New instance, new location -> extend the `path_globs` for that kind, then add `refs:`.
- New category -> add a new kind entry to `docs/kinds.md` (name + `path_globs` + `id_pattern`; `index.output` only if a generated index is wanted), then add `refs:`.
- Outside graph (README/template/scratch/generated) -> no `refs:`; tighten the glob if validate complains.

Field reference: `docs/reference/frontmatter.md`. Do not guess fields.
'
fi

# Trigger 2: any edit/write to .kiro/specs/<spec>/requirements.md.
case "$file" in
  .kiro/specs/*/requirements.md)
    judgment+='
## Judgment: requirements.md edit

If a numbered requirement was added (or sub-id like `req:<spec>:N.M`) and any other doc may cite it now or later, add the new ID(s) to the `provides:` list in this file. The validator only catches dangling references at the citation site, not missing entries on the producer side.
'
    ;;
esac

# Trigger 3: source change under crates/*. The hook already injects `touched`
# output above; this section adds the per-change-type decision table.
case "$file" in
  crates/*)
    judgment+='
## Judgment: source change

Per the `touched` output above, decide which doc (if any) to update:

| Change | Doc update? |
|---|---|
| Internal (refactor / bugfix / perf / dep bump) | none |
| New / changed CLI flag | `ref:cli` |
| New / changed config key | `ref:config` |
| New / removed structured log event | `ref:log-events` |
| New extension point (trait / hook / schema slot) | `ref:extension-surface` + relevant `fr:` |
| New / removed end-to-end behavior | relevant `fr:NN-*` (and crate README when the public surface changed) |

If unsure whether a change is "public surface", err on updating the doc.
'
    ;;
esac

# Trigger 4: docs/kinds.md edit.
if [[ "$file" == "docs/kinds.md" ]]; then
  judgment+='
## Judgment: kinds.md edit

| Edit type | Risk |
|---|---|
| Tightening a `path_globs` | safe |
| Loosening / adding new globs | every newly-matched file must have `refs:` (validator enforces) |
| Renaming a kind | invalidates every `kind: <old-name>` in existing front matter; audit + rewrite |
| Adding `index.output` | run `roki-doctools index` once to materialize the file |
'
fi

# Trigger 5: existing graph-linked .md edit -> surface links for body sync.
# Skipped on new files (trigger 1 already covers authoring).
if ! $is_new_md && [[ "$file" == *.md && -f "$file" ]]; then
  doc_id="$(awk '
    /^---$/{f++; if(f>=2)exit; next}
    f==1 && /^[[:space:]]+id:[[:space:]]+/{
      sub(/^[[:space:]]+id:[[:space:]]+/, "")
      gsub(/^[[:space:]]+|[[:space:]]+$/, "")
      gsub(/^"|"$/, "")
      print
      exit
    }
  ' "$file")"

  if [[ -n "$doc_id" ]]; then
    show_out="$($bin show "$doc_id" 2>&1 || true)"
    if [[ -n "$show_out" ]]; then
      judgment+='
## Judgment: linked-doc content drift

If this edit changed observable behavior (not a typo or prose-only tweak), the linked docs below may need matching content updates so their text does not drift from this file. Re-read each before deciding to update.

```
'"$show_out"'
```
'
    fi

    # Trigger 5b: requirements.md -> per-provides-id consumers.
    if [[ "$file" == .kiro/specs/*/requirements.md ]]; then
      prov_ids="$(awk '
        /^---$/{f++; if(f>=2)exit; next}
        f==1 && /^[[:space:]]+provides:[[:space:]]*$/{p=1; next}
        f==1 && p && /^[[:space:]]+-[[:space:]]+/{
          sub(/^[[:space:]]+-[[:space:]]+/, "")
          gsub(/^[[:space:]]+|[[:space:]]+$/, "")
          gsub(/^"|"$/, "")
          print
          next
        }
        f==1 && p && !/^[[:space:]]+-[[:space:]]+/ && !/^[[:space:]]*$/{p=0}
      ' "$file")"

      if [[ -n "$prov_ids" ]]; then
        per_req=""
        while IFS= read -r req_id; do
          [[ -z "$req_id" ]] && continue
          consumers="$($bin impact "$req_id" --depth 1 2>&1 \
            | awk '/^depth 1:/{p=1; next} p && /^  /{
                sub(/^  +/, "")
                sub(/[[:space:]]+.*/, "")
                printf "%s%s", sep, $0
                sep=", "
              } END{ if(sep) print "" }')"
          if [[ -n "$consumers" ]]; then
            per_req+="  ${req_id} -> ${consumers}"$'\n'
          else
            per_req+="  ${req_id} -> (no direct consumers)"$'\n'
          fi
        done <<< "$prov_ids"
        if [[ -n "$per_req" ]]; then
          judgment+='
For each requirement ID this file `provides:`, the docs that directly cite it (depth 1):

```
'"$per_req"'```
'
        fi
      fi
    fi
  fi
fi

# --- combine and emit ---
final=""
[[ -n "$out" ]] && final="$out"
if [[ -n "$judgment" ]]; then
  if [[ -n "$final" ]]; then
    final="${final}
${judgment}"
  else
    final="$judgment"
  fi
fi

[[ -z "$final" ]] && exit 0

# Footer pointing at the authoritative rule + skill.
final="${final}
---
Authoritative rule: \`.claude/rules/refs.md\`. Tool reference: invoke the \`roki-doctools\` skill (\`.claude/skills/roki-doctools/SKILL.md\`)."

if command -v jq >/dev/null 2>&1; then
  printf '%s' "$final" | jq -Rs '{
    hookSpecificOutput: {
      hookEventName: "PostToolUse",
      additionalContext: .
    }
  }'
elif command -v python3 >/dev/null 2>&1; then
  printf '%s' "$final" | python3 -c '
import json, sys
ctx = sys.stdin.read()
print(json.dumps({
    "hookSpecificOutput": {
        "hookEventName": "PostToolUse",
        "additionalContext": ctx,
    }
}))'
else
  printf '%s\n' "$final" >&2
fi

exit 0

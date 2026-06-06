#!/usr/bin/env bash
# PostToolUse hook (Bash): when a python/bash/perl command references .tengo
# files, remind Claude to run the formatter — script-driven edits bypass the
# Write/Edit auto-format hook.
#
# Detection is a heuristic over the command STRING (the script is not executed):
#   - false negative: `python convert.py` that writes .tengo internally is missed
#   - false positive: `echo foo.tengo >> log` matches
# This is inherent to string inspection; the reminder is advisory, never blocking.
set -euo pipefail

input=$(cat)
command -v jq >/dev/null 2>&1 || exit 0

cmd=$(printf '%s' "$input" | jq -r '.tool_input.command // empty')
[ -n "$cmd" ] || exit 0

# Must reference a .tengo path...
printf '%s' "$cmd" | grep -qE '\.tengo([^A-Za-z0-9_]|$)' || exit 0

# ...driven by a script interpreter (word-bounded so `shasum` won't match `sh`)...
printf '%s' "$cmd" | grep -qE '(^|[^A-Za-z0-9_/.-])(python3?|perl|bash|sh|zsh)([^A-Za-z0-9_]|$)' || exit 0

# ...and not already a formatter invocation.
if printf '%s' "$cmd" | grep -qE 'tengo-lsp[[:space:]]+fmt'; then
  exit 0
fi

jq -n '{
  hookSpecificOutput: {
    hookEventName: "PostToolUse",
    additionalContext: "A script (python/perl/bash) just touched one or more .tengo files. Script-driven edits bypass the auto-formatter that runs on Write/Edit. Format the affected files now:\n\n    tengo-lsp fmt --write <file.tengo> [more.tengo ...]\n\n(omit --write to preview on stdout). Files with syntax errors are left untouched."
  }
}'
exit 0

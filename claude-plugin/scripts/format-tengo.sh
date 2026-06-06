#!/usr/bin/env bash
# PostToolUse hook (Write|Edit|MultiEdit): reformat the just-written file with
# `tengo-lsp fmt --write` when it is a Tengo file. If the formatter changed the
# file, tell Claude (via additionalContext) so it re-reads before further edits.
#
# Output contract (PostToolUse, exit 0): emit JSON on stdout to inject context;
# emit nothing for a clean no-op.
set -euo pipefail

input=$(cat)

# Helper: emit a systemMessage (shown to the user) and exit without acting.
warn() {
  jq -n --arg m "tengo-fmt: $1" \
    '{hookSpecificOutput: {hookEventName: "PostToolUse", additionalContext: $m}, systemMessage: $m}'
  exit 0
}

# jq is required: tool_input.content is arbitrary text, so the JSON cannot be
# parsed safely by grep/regex.
command -v jq >/dev/null 2>&1 || { echo '{"systemMessage":"tengo-fmt: jq not found; install jq to enable auto-formatting"}'; exit 0; }

file=$(printf '%s' "$input" | jq -r '.tool_input.file_path // empty')
[ -n "$file" ] || exit 0

# *.tengo covers .tengo, .lib.tengo, .tpl.tengo.
case "$file" in
  *.tengo) ;;
  *) exit 0 ;;
esac

[ -f "$file" ] || exit 0

command -v tengo-lsp >/dev/null 2>&1 || warn "tengo-lsp not on PATH; skipped formatting $file"

before=$(shasum "$file" 2>/dev/null | awk '{print $1}') || exit 0
tengo-lsp fmt --write "$file" >/dev/null 2>&1 || exit 0
after=$(shasum "$file" 2>/dev/null | awk '{print $1}') || exit 0

if [ "$before" != "$after" ]; then
  jq -n --arg f "$file" '{
    hookSpecificOutput: {
      hookEventName: "PostToolUse",
      additionalContext: ("tengo-lsp reformatted " + $f + " (indentation normalized to tabs); on-disk content now differs from what you wrote. Re-read it before editing it again.")
    }
  }'
fi
exit 0

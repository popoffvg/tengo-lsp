# tengo-fmt — Claude Code plugin

Keeps Tengo files formatted while Claude Code works on them.

Two `PostToolUse` hooks:

- **Write / Edit / MultiEdit on a `*.tengo` file** → runs `tengo-lsp fmt --write`
  on it. If the formatter changed the file, Claude is told (via
  `additionalContext`) to re-read it before editing again.
- **Bash command that looks like a `python`/`perl`/`bash` script touching a
  `.tengo` file** → injects a reminder to run `tengo-lsp fmt --write`, because
  script-driven edits bypass the Write/Edit hook above.

## Requirements

- [`tengo-lsp`](https://github.com/popoffvg/tengo-lsp) on `PATH` (provides the
  `fmt` subcommand). The `fmt` subcommand was added after v0.1.1 — use a current
  build.
- `jq` on `PATH` (JSON parsing). Missing `jq`/`tengo-lsp` degrades to a no-op
  with a `systemMessage`, never a hard failure.

## Install

Via the marketplace shipped in this repo:

```
/plugin marketplace add popoffvg/tengo-lsp
/plugin install tengo-fmt@tengo-lsp
```

Local dev (load the directory directly):

```bash
claude --plugin-dir ./claude-plugin
```

## The script-touch reminder is a heuristic

Detection inspects the Bash **command string** (the script is not executed):

- **false negative** — `python convert.py` that writes `.tengo` files internally
  is missed (no `.tengo` in the command line).
- **false positive** — `echo foo.tengo >> log` matches the pattern.

This is inherent to string inspection. The reminder is advisory and never
blocks the tool.

## Files

- `.claude-plugin/plugin.json` — manifest
- `hooks/hooks.json` — `PostToolUse` matchers
- `scripts/format-tengo.sh` — Write/Edit auto-format
- `scripts/remind-script-touch.sh` — Bash script-touch reminder

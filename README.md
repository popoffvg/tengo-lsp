# tengo-lsp

A [Language Server Protocol](https://microsoft.github.io/language-server-protocol/)
server for the [Tengo](https://github.com/d5/tengo) scripting language, with
first-class support for [Platforma](https://platforma.bio) Tengo packages.

Written in Rust on top of [`tower-lsp`](https://crates.io/crates/tower-lsp) and
the [`tree-sitter-tengo`](https://github.com/popoffvg/tree-sitter-tengo)
grammar. It speaks LSP over stdio, so it works with any compatible editor —
see the [Zed](https://github.com/popoffvg/zed-tengo) and
[VS Code](https://github.com/popoffvg/vscode-tengo) extensions.

## Features

| Capability | Details |
| --- | --- |
| **Go to definition** | Jump from an `import("...")` string or an import alias to the imported file; from `alias.member` to the matching key in the target file's `export { ... }` block; and from SDK artifact calls to the artifact file. |
| **Find references** | Workspace-wide for exported module members: every `alias.member` usage across importing files plus references inside the defining module. Local symbols resolve within their scope in the current file. |
| **Rename** | Renames a symbol and all its references using the same scope/cross-file analysis as find-references: local symbols within their scope, exported members across the whole workspace (including the `export` key). Shadow-aware (won't touch a parameter that shadows the name) and reads unsaved editor buffers. Divergent export keys (`ext: local`, literals, inline funcs) are refused via `prepareRename` to avoid half-applied edits. |
| **Formatting** | Reindents a document with tabs, one level per enclosing `{}`/`[]`/`()` block, and strips trailing whitespace — matching the reference go-mode rules. Indentation only: code is never reflowed and comments are preserved. Also available as a CLI: `tengo-lsp fmt [--write] [files...]` (stdin→stdout with no files). Leaves files with syntax errors untouched. |
| **Completion** | After `.` on an imported alias, lists the members exported by that module. |
| **Hover** | Signature and doc comment of imported members — both `//` line docs and `/** */` JSDoc blocks, resolved through wrapped `export` maps. |
| **Linting** | Publishes diagnostics as you type, and runs as a CLI: `tengo-lsp lint [--rules] [paths...]`. See [Linting](#linting). |

### SDK artifact calls

Go-to-definition treats the string argument of these methods like an import id,
regardless of the receiver alias:

```
getTemplateId  getSoftwareInfo  importTemplate  importSoftware  importAsset
```

## Import resolution

Imports are resolved the way the Platforma package builder resolves them:

- **Local artifacts** — `:util`, `:pframes.pcolumn` — resolve against the
  current package's `src/`. Dotted ids map to nested paths
  (`pframes.pcolumn` → `src/pframes/pcolumn.lib.tengo`), with `index.*`
  fallbacks for directory imports. Recognized extensions:
  `.lib.tengo`, `.tpl.tengo`, `.tengo`, `.sw.json`, `.as.json`.
- **Package artifacts** — `@scope/pkg:index`, `pkg:util` — resolve via the
  nearest `node_modules`, probing both the source layout and the published
  `dist/tengo/{lib,software,asset}/<id>.<ext>` layout. (Compiled `.plj.gz`
  templates are binary, so only text artifacts are navigable.)
- **Stdlib modules** — `fmt`, `json`, `text`, `os`, `math`, `times`, … — are
  recognized and intentionally do not navigate to a file.

## Linting

Diagnostics are published on open and on every change, using the parse tree the
server already holds — so linting costs no extra parse. The same rules run from
the command line:

```bash
tengo-lsp lint src/            # walks directories, skips node_modules/ and dist/
tengo-lsp lint a.tengo b.tengo
tengo-lsp lint --rules         # list rules and their default severities
```

Exit status is 1 only when a finding is an `error`; warnings report without
failing a build.

### Rules

| Rule | Default | What it reports |
| --- | --- | --- |
| `no-underscore-export-key` | `warn` | A key in the `export` map starting with `_`. The key is the name importers see, so the prefix claims the member is private while publishing it. Sees through `export ll.toStrict({ ... })`. |
| `unused-import` | `warn` | An `import(...)` alias never mentioned again in the file. |
| `unused-function` | `warn` | A top-level function that is neither called nor exported. Adding a function to the export map counts as use. A function that only calls itself is still reported — recursion is not a use. Functions named `Test*` inside `*.test.tengo` are entry points for the test runner and are never reported. |
| `missing-type-guard` | **`off`** | A parameter of an *exported* function that no type guard ever inspects. See [Type guards](#type-guards). |

`missing-type-guard` ships **off**. Enable it per package:

```tengo
export { rules: { "missing-type-guard": "warn" } }
```

### Type guards

Exported functions are the boundary where callers are outside the author's
control, so this rule asks whether each parameter is checked before use. A
parameter counts as guarded when it is passed to any of:

- **a name starting with `is_` or `is` + capital** — the Tengo builtins
  (`is_undefined`, `is_map`, `is_callable`), the SDK's camelCase predicates
  (`ll.isMap`, `validation.isValid`), and project-defined ones (`isAssetInfo`,
  `isExpression`). A prefix rule rather than a fixed list, so new predicates
  count without editing the linter.
- **`assertType`, `assert`, `assertJsonSchema`, `checkJson`, `type_name`** —
  matched on the callee name only, so any import alias works:
  `validation.assertType`, `val.assertType` and a bare `assertType` all count.

Deliberately generous — the rule is advisory, so counting a loose check as a
guard beats reporting code that does validate.

Not reported:

- **Variadic parameters.** `...rest` is an array by construction, so a type
  guard on it is vacuous; the value worth checking is `rest[0]`.
- **`_`**, the conventional throwaway parameter.
- **Private functions and zero-parameter functions.**
- Guards inside a nested closure still count — the value is inspected either way.

The rule reports per parameter, naming only the unchecked ones. For
`walk(expression, visitFn)` where `expression` is guarded by `isExpression` but
`visitFn` is called unchecked, only `visitFn` is named.

Measured on the Platforma monorepo, which is why the default is `off`: of
exported functions taking at least one parameter, **1.1%** call
`validation.assertType` — 15.6% inside `workflow-tengo`, 0% in the blocks. It
is an SDK convention, not a monorepo-wide one.

A file the grammar cannot parse is skipped entirely: in a truncated tree
"unused" and "not exported" both read as true for symbols that are neither, so
guessing would be worse than staying quiet.

### Configuration

Optional. Drop a `lint.tengo` at the workspace root; it is read as Tengo
**data** — parsed with the same grammar as any other source file, never
executed. Every field is optional, and anything unrecognized is ignored rather
than fatal.

```tengo
export {
	rules: {
		"unused-import":            "error",
		"no-underscore-export-key": "warn",
		"unused-function":          "off"
	},
	exclude: ["**/dist/**", "**/vendor/**"]
}
```

Severities are `off`, `hint`, `info`, `warn` and `error`. Rules absent from the
map keep the default from the table above. In `exclude`, `*` stays inside one
path segment and `**` spans any number.

### Custom rules

A `custom` array declares new rules in the config itself — a
[tree-sitter query](https://tree-sitter.github.io/tree-sitter/using-parsers/queries/1-syntax.html)
plus the message to report where it matches. No rebuild, no plugin:

```tengo
export {
	custom: [
		{
			name:     "no-debug-print",
			severity: "error",
			message:  "leftover ll.print({{arg}}) — remove before committing",
			query: `((call_expression
			           function: (selector_expression
			             object: (identifier) @o
			             field: (identifier) @m)
			           (argument_list . (_) @arg)) @node
			         (#eq? @o "ll") (#eq? @m "print"))`
		}
	]
}
```

| Field | Required | Meaning |
| --- | --- | --- |
| `name` | yes | Rule id, used as the diagnostic code and in the `rules` map |
| `query` | yes | Tree-sitter query with at least one capture |
| `message` | no | Text to report; defaults to `name` |
| `severity` | no | Defaults to `warn`; overridable via `rules` like any built-in |

- **Write queries as backtick raw strings.** Queries contain double quotes, and
  a backtick string passes regexes through to `#match?` untouched.
- **The reported range** is the capture named `@node`, or the match's first
  capture when there is none.
- **`{{capture}}` in `message`** expands to the text that capture matched. An
  unknown name is left as written so the typo is visible.
- **Predicates** `#eq?`, `#not-eq?`, `#any-of?` and `#match?` are evaluated.
- Because matching is on the syntax tree, a commented-out call is not a match —
  on the Platforma blocks the rule above reports 7 real `ll.print` calls where
  `grep` reports 15, the other 8 being commented out.
- A custom rule that fails to load (bad query, missing field, duplicate name,
  or a name that shadows a built-in) is reported and skipped; the other rules
  still run. The CLI prints these to stderr; the server logs them.

Queries are matched against the tree-sitter grammar, so node names come from
[`tree-sitter-tengo`](https://github.com/popoffvg/tree-sitter-tengo) —
`call_expression`, `selector_expression`, `map_entry`, `func_literal`,
`export_statement`, and so on.

`tengo-lsp lint --rules` lists built-ins and custom rules together with their
effective severities, which is the quickest way to check a config was read.

## Installation

### Homebrew (macOS / Linux)

```bash
brew install popoffvg/tengo-lsp/tengo-lsp
```

This taps [`popoffvg/homebrew-tengo-lsp`](https://github.com/popoffvg/homebrew-tengo-lsp)
and installs the prebuilt binary. Upgrade with `brew upgrade tengo-lsp`.

### Prebuilt binaries

Download the archive for your platform from the
[Releases page](https://github.com/popoffvg/tengo-lsp/releases) and put
`tengo-lsp` on your `PATH`:

| Platform | Asset |
| --- | --- |
| macOS arm64 | `tengo-lsp-darwin-aarch64.tar.gz` |
| macOS x86_64 | `tengo-lsp-darwin-x86_64.tar.gz` |
| Linux arm64 | `tengo-lsp-linux-aarch64.tar.gz` |
| Linux x86_64 | `tengo-lsp-linux-x86_64.tar.gz` |
| Windows x86_64 | `tengo-lsp-windows-x86_64.zip` |

```bash
tar -xzf tengo-lsp-darwin-aarch64.tar.gz
install -m755 tengo-lsp ~/.local/bin/tengo-lsp
```

> **macOS (Apple Silicon):** if you copy the binary into place, re-sign it with
> `codesign --force --sign - ~/.local/bin/tengo-lsp`. Copying invalidates the
> ad-hoc signature and the OS will `SIGKILL` the unsigned binary on launch.

### From source

```bash
cargo build --release
# binary at target/release/tengo-lsp
```

### Editors

The [Zed](https://github.com/popoffvg/zed-tengo) and
[VS Code](https://github.com/popoffvg/vscode-tengo) extensions locate
`tengo-lsp` on your `PATH` and otherwise download the matching release binary
automatically — no manual setup needed.

## Claude Code plugin

This repo also ships a [Claude Code](https://code.claude.com) plugin,
**`tengo-fmt`** (in [`claude-plugin/`](claude-plugin/)), that keeps Tengo files
formatted while Claude Code edits them. Two `PostToolUse` hooks:

- After Claude writes/edits a `*.tengo` file → runs `tengo-lsp fmt --write` on it.
- When a `python`/`perl`/`bash` command touches a `.tengo` file → reminds Claude
  to format (script edits bypass the write hook).

Requires `tengo-lsp` (current build, for the `fmt` subcommand) and `jq` on `PATH`.

### Install

The repo doubles as a plugin marketplace (`.claude-plugin/marketplace.json`).
In a Claude Code session:

```
/plugin marketplace add popoffvg/tengo-lsp
/plugin install tengo-fmt@tengo-lsp
```

Equivalent CLI:

```bash
claude plugin marketplace add popoffvg/tengo-lsp
claude plugin install tengo-fmt@tengo-lsp
```

Local development — load the plugin directory directly without the marketplace:

```bash
claude --plugin-dir ./claude-plugin
# validate the manifests:
claude plugin validate .
```

See [`claude-plugin/README.md`](claude-plugin/README.md) for the hook details
and the limits of the script-touch heuristic.

## How it works

The server parses each open document with Tree-sitter and builds a per-file
model — scopes, definitions, references, and imports
(`src/scope.rs`, `src/symbols.rs`, `src/document.rs`). Requests are served from
that model:

- `src/definition.rs` — go to definition
- `src/references.rs` — find references (single-file and workspace-wide)
- `src/rename.rs` — rename symbol / prepare-rename, reusing the reference search
- `src/format.rs` — formatter (LSP `textDocument/formatting` + `fmt` CLI)
- `src/completion.rs` — member completion
- `src/hover.rs` — hover docs
- `src/exports.rs` — parse `export { ... }` maps, member ranges, signatures, docs
- `src/resolver.rs` — map import ids to files on disk
- `src/lint.rs` — rule registry and rules (LSP diagnostics + `lint` CLI)
- `src/lint_config.rs` — read `lint.tengo` as data, resolve severities and excludes

Workspace-wide reference search scans the workspace folders reported at
`initialize` (falling back to the file's package root), skipping
`node_modules`, `dist`, `target`, and `.git`.

## Releasing

Pushing a `v*` tag triggers
[`.github/workflows/release.yml`](.github/workflows/release.yml), which builds
the cross-platform binaries and attaches them to a GitHub release.

```bash
# bump version in Cargo.toml first, then:
git tag v0.1.1
git push origin v0.1.1
```

## License

MIT

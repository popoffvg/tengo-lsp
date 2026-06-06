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

## Installation

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

# tengo-lsp

A Tengo language server (Rust, tower-lsp + tree-sitter).

## Commits & changelog (git-cliff)

This repo uses [Conventional Commits](https://www.conventionalcommits.org) and
[git-cliff](https://github.com/orhun/git-cliff) to generate the changelog.

- **Commit format is mandatory:** `<type>(<scope>)?: <description>`
  Types: `feat fix docs style refactor perf test build ci chore revert`.
  Example: `feat(rename): add textDocument/rename support`.
- A **`commit-msg` hook** (`.githooks/commit-msg`) enforces this. After cloning,
  enable it once: `git config core.hooksPath .githooks`.
- **Changelog config** lives in `cliff.toml`. Regenerate `CHANGELOG.md` with:
  `git-cliff --output CHANGELOG.md`. Preview the unreleased section with
  `git-cliff --unreleased`.
- **Releases** are tag-driven: pushing a `v*` tag runs `.github/workflows/release.yml`,
  whose `changelog` job runs git-cliff (`--latest`) to build the GitHub release
  notes; the build matrix then attaches the per-platform binaries.

When writing commits here, always use a conventional type — non-conventional
commits are silently dropped from the changelog (and rejected by the hook).

use std::sync::OnceLock;

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Range};
use tree_sitter::{Node, Query, QueryCursor, StreamingIterator};

use crate::document::FileState;
use crate::lint_config::{CustomRule, LintConfig, Severity};
use crate::symbols::{node_to_lsp_range, SymbolKind};

/// One lint rule: its name (also the diagnostic code), the severity it reports
/// at unless `lint.tengo` says otherwise, and its implementation.
pub struct Rule {
    pub name: &'static str,
    pub default: Severity,
    pub description: &'static str,
    run: fn(&FileState, &mut Sink),
}

/// The rule registry — the single source of truth shared by the engine, the
/// `--rules` listing and the config validator.
pub const RULES: &[Rule] = &[
    Rule {
        name: "no-underscore-export-key",
        default: Severity::Warn,
        description: "an exported key is public, so it must not be prefixed with `_`",
        run: no_underscore_export_key,
    },
    Rule {
        name: "unused-import",
        default: Severity::Warn,
        description: "an imported alias is never referenced",
        run: unused_import,
    },
    Rule {
        name: "unused-function",
        default: Severity::Warn,
        description: "a top-level function is neither exported nor called",
        run: unused_function,
    },
    Rule {
        name: "missing-type-guard",
        // Off by default: across the Platforma monorepo only 1.1% of exported
        // functions call `validation.assertType`, so enabling this everywhere
        // would bury real findings. Turn it on per package.
        default: Severity::Off,
        description: "a parameter of an exported function is never type-checked",
        run: missing_type_guard,
    },
];

/// Whether a callee name counts as inspecting the value it is handed. Matched
/// on the callee name alone, so the import alias is irrelevant —
/// `validation.assertType`, `val.assertType` and a bare `assertType` all count.
///
/// Any `is_*` or `is[A-Z]*` name qualifies. That covers the Tengo builtins
/// (`is_undefined`, `is_map`), the SDK's camelCase predicates (`ll.isMap`,
/// `validation.isValid`) and project-defined ones (`isAssetInfo`,
/// `isExpression`) without a list that goes stale as conventions grow.
/// Deliberately generous: this rule is advisory, so counting a loose check as
/// a guard is better than reporting code that does validate.
fn is_type_guard(name: &str) -> bool {
    if let Some(rest) = name.strip_prefix("is") {
        if rest.starts_with('_') || rest.starts_with(|c: char| c.is_ascii_uppercase()) {
            return true;
        }
    }
    matches!(
        name,
        "assertType" | "assert" | "assertJsonSchema" | "checkJson" | "type_name"
    )
}

/// Collects diagnostics for one rule, stamping each with that rule's name and
/// resolved severity so the rules themselves never repeat it.
struct Sink<'a> {
    rule: &'static str,
    severity: DiagnosticSeverity,
    out: &'a mut Vec<Diagnostic>,
}

impl Sink<'_> {
    fn report(&mut self, range: Range, message: String) {
        self.out.push(Diagnostic {
            range,
            severity: Some(self.severity),
            code: Some(NumberOrString::String(self.rule.to_string())),
            source: Some("tengo-lint".to_string()),
            message,
            ..Default::default()
        });
    }
}

/// Run every enabled rule over an already-parsed document.
pub fn check(state: &FileState, config: &LintConfig) -> Vec<Diagnostic> {
    // A file the grammar cannot parse yields a truncated tree, where "unused"
    // and "not exported" both read as true for symbols that are neither. Skip
    // it rather than emit confident nonsense.
    if state.tree.root_node().has_error() {
        return Vec::new();
    }

    let mut out = Vec::new();
    for rule in RULES {
        let Some(severity) = config.severity_for(rule.name, rule.default).to_lsp() else {
            continue;
        };
        let mut sink = Sink {
            rule: rule.name,
            severity,
            out: &mut out,
        };
        (rule.run)(state, &mut sink);
    }

    // Rules declared in `lint.tengo`. Their configured severity is itself
    // overridable, so `rules: { "my-rule": "off" }` turns one off.
    for custom in &config.custom {
        let Some(severity) = config.severity_for(&custom.name, custom.severity).to_lsp() else {
            continue;
        };
        run_custom(state, custom, severity, &mut out);
    }
    out
}

/// Run one config-declared rule. The reported node is the capture named
/// `@node` when present, else the match's first capture.
fn run_custom(
    state: &FileState,
    rule: &CustomRule,
    severity: DiagnosticSeverity,
    out: &mut Vec<Diagnostic>,
) {
    let names = rule.query.capture_names();
    let anchor = rule.query.capture_index_for_name("node");
    let source = state.source.as_bytes();

    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&rule.query, state.tree.root_node(), source);
    while let Some(m) = matches.next() {
        let target = anchor
            .and_then(|i| m.captures.iter().find(|c| c.index == i))
            .or_else(|| m.captures.first());
        let Some(target) = target else {
            continue;
        };
        out.push(Diagnostic {
            range: node_to_lsp_range(target.node),
            severity: Some(severity),
            code: Some(NumberOrString::String(rule.name.clone())),
            source: Some("tengo-lint".to_string()),
            message: render(&rule.message, m.captures, names, source),
            ..Default::default()
        });
    }
}

/// Expand `{{capture}}` placeholders to the text each capture matched. An
/// unknown placeholder is left as written so the mistake is visible.
fn render(
    template: &str,
    captures: &[tree_sitter::QueryCapture],
    names: &[&str],
    source: &[u8],
) -> String {
    if !template.contains("{{") {
        return template.to_string();
    }
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            break;
        };
        let key = after[..end].trim();
        let text = captures
            .iter()
            .find(|c| names.get(c.index as usize) == Some(&key))
            .and_then(|c| c.node.utf8_text(source).ok());
        match text {
            Some(t) => out.push_str(t),
            None => {
                out.push_str("{{");
                out.push_str(&after[..end]);
                out.push_str("}}");
            }
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

/// Load the config for `dir` and describe anything wrong with it: structural
/// problems found while parsing, plus severities set for names no rule
/// answers to. One entry point so the CLI and the server report identically.
pub fn load_config(dir: &std::path::Path) -> (LintConfig, Vec<String>) {
    let config = LintConfig::discover(dir);
    let mut problems = config.errors.clone();
    for name in config.override_names() {
        let known =
            RULES.iter().any(|r| r.name == name) || config.custom.iter().any(|c| c.name == name);
        if !known {
            problems.push(format!("severity set for unknown rule `{name}`"));
        }
    }
    for custom in &config.custom {
        if RULES.iter().any(|r| r.name == custom.name) {
            problems.push(format!(
                "custom rule `{}` shadows a built-in rule; pick another name",
                custom.name
            ));
        }
    }
    (config, problems)
}

// ── Rules ───────────────────────────────────────────────────────────────────

/// An `export` map key is the name importers see. A `_` prefix there claims the
/// member is private while publishing it, so the prefix and the export
/// disagree. Reuses `exports::find_export_map`, which already knows both the
/// bare `export { ... }` and wrapped `export ll.toStrict({ ... })` shapes.
fn no_underscore_export_key(state: &FileState, sink: &mut Sink) {
    let Some(map) = crate::exports::find_export_map(state.tree.root_node()) else {
        return;
    };
    let mut cursor = map.walk();
    for entry in map.children(&mut cursor) {
        if entry.kind() != "map_entry" {
            continue;
        }
        let Some(key) = entry.child_by_field_name("key") else {
            continue;
        };
        let name = crate::lint_config::literal_text(key, &state.source);
        if !name.starts_with('_') {
            continue;
        }
        sink.report(
            node_to_lsp_range(key),
            format!(
                "exported key `{name}` starts with `_` but is public; \
                 drop the prefix or remove it from the export map"
            ),
        );
    }
}

/// An import whose alias is never mentioned again. `state.refs` holds every
/// non-defining identifier in the file, so a name absent from it is unused —
/// including aliases only ever meant for `alias.member` access.
fn unused_import(state: &FileState, sink: &mut Sink) {
    for def in &state.defs {
        if def.kind != SymbolKind::Import || is_referenced(state, &def.name, None) {
            continue;
        }
        let message = match state.imports.get(&def.name) {
            Some(info) => format!("unused import `{}` of \"{}\"", def.name, info.module_path),
            None => format!("unused import `{}`", def.name),
        };
        sink.report(def.range, message);
    }
}

/// A top-level function that nothing reaches. Exporting counts as use, because
/// the export map mentions the name and that mention lands in `state.refs`.
/// The function's own body is excluded from the scan, so a recursive call does
/// not make a dead function look alive.
fn unused_function(state: &FileState, sink: &mut Sink) {
    for (name_node, body) in top_level_funcs(state) {
        let name = name_node
            .utf8_text(state.source.as_bytes())
            .unwrap_or("")
            .to_string();
        if name.is_empty()
            || is_test_entry_point(&state.uri, &name)
            || is_referenced(state, &name, Some(body.byte_range()))
        {
            continue;
        }
        sink.report(
            node_to_lsp_range(name_node),
            format!(
                "function `{name}` is never used; \
                 call it, add it to the export map, or delete it"
            ),
        );
    }
}

/// A parameter of an exported function that no type guard ever inspects.
/// Scoped to exported functions because that is the boundary where callers are
/// out of the author's control; a private helper is checked by its callers.
fn missing_type_guard(state: &FileState, sink: &mut Sink) {
    let exported = exported_names(state);
    if exported.is_empty() {
        return;
    }
    let source = state.source.as_bytes();

    for (name_node, func) in top_level_funcs(state) {
        let name = name_node.utf8_text(source).unwrap_or("");
        if name.is_empty() || !exported.contains(name) {
            continue;
        }
        let params = func_params(func, source);
        if params.is_empty() {
            continue;
        }
        let guarded = guarded_idents(func, source);
        // Variadics are skipped: `...args` is an array by construction, so a
        // type guard on it would be vacuous — the value worth checking is
        // `args[0]`. `_` is the conventional throwaway.
        let missing: Vec<&str> = params
            .iter()
            .filter(|(name, variadic)| !variadic && *name != "_" && !guarded.contains(*name))
            .map(|(name, _)| *name)
            .collect();
        if missing.is_empty() {
            continue;
        }
        let list = missing
            .iter()
            .map(|p| format!("`{p}`"))
            .collect::<Vec<_>>()
            .join(", ");
        let noun = if missing.len() == 1 {
            "parameter"
        } else {
            "parameters"
        };
        sink.report(
            node_to_lsp_range(name_node),
            format!(
                "exported `{name}` never type-checks {noun} {list}; \
                 guard with validation.assertType or an is_* check"
            ),
        );
    }
}

// ── Shared helpers ──────────────────────────────────────────────────────────

/// Every identifier mentioned inside a top-level `export` statement. Used to
/// tell a module's public surface from its internals.
fn exported_names(state: &FileState) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    let root = state.tree.root_node();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "export_statement" {
            collect_idents(child, state.source.as_bytes(), &mut out);
        }
    }
    out
}

fn collect_idents(node: Node, source: &[u8], out: &mut std::collections::HashSet<String>) {
    if node.kind() == "identifier" {
        out.insert(node.utf8_text(source).unwrap_or("").to_string());
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_idents(child, source, out);
    }
}

/// Parameter names of a `func_literal` as `(name, is_variadic)`.
/// `parameter_list` is an optional unnamed child, so a no-argument function
/// yields an empty list.
fn func_params<'a>(func: Node<'a>, source: &'a [u8]) -> Vec<(&'a str, bool)> {
    let mut out = Vec::new();
    let mut cursor = func.walk();
    for child in func.children(&mut cursor) {
        if child.kind() != "parameter_list" {
            continue;
        }
        let mut inner = child.walk();
        for param in child.children(&mut inner) {
            match param.kind() {
                "identifier" => out.push((param.utf8_text(source).unwrap_or(""), false)),
                "variadic_parameter" => {
                    let mut v = param.walk();
                    for g in param.children(&mut v) {
                        if g.kind() == "identifier" {
                            out.push((g.utf8_text(source).unwrap_or(""), true));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// Identifiers handed to a recognized type guard anywhere inside `node`.
/// Nested closures count: a guard applied deeper still inspects the value.
fn guarded_idents(node: Node, source: &[u8]) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    collect_guarded(node, source, &mut out);
    out
}

fn collect_guarded(node: Node, source: &[u8], out: &mut std::collections::HashSet<String>) {
    if node.kind() == "call_expression" {
        if let Some(callee) = node.child_by_field_name("function") {
            let name = match callee.kind() {
                "identifier" => callee.utf8_text(source).ok(),
                "selector_expression" => callee
                    .child_by_field_name("field")
                    .and_then(|f| f.utf8_text(source).ok()),
                _ => None,
            };
            if name.is_some_and(is_type_guard) {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "argument_list" {
                        collect_idents(child, source, out);
                    }
                }
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_guarded(child, source, out);
    }
}

/// A test case is reached by the runner, not by a call. `test.lib.tengo`
/// documents the contract: "Regular tengo file with tests contains bunch of
/// fuctions starting with 'Test' prefix". Only `.test.tengo` files get the
/// exemption, so a leftover `TestFoo` in a library is still reported.
fn is_test_entry_point(uri: &str, name: &str) -> bool {
    uri.ends_with(".test.tengo") && name.starts_with("Test")
}

/// Whether `name` is referenced anywhere, optionally ignoring references that
/// fall inside `skip` (used to discount a function's own body).
fn is_referenced(state: &FileState, name: &str, skip: Option<std::ops::Range<usize>>) -> bool {
    state.refs.iter().any(|r| {
        r.name == name
            && match &skip {
                Some(body) => r.byte_range.start < body.start || r.byte_range.end > body.end,
                None => true,
            }
    })
}

/// Top-level `name := func(...)` and `var name = func(...)`, as
/// `(name node, func_literal node)`. Both declaration shapes are one query so
/// the two spellings cannot drift apart.
fn top_level_funcs(state: &FileState) -> Vec<(Node<'_>, Node<'_>)> {
    const SRC: &str = r#"
(source_file
  (assignment_statement
    left: (identifier) @name
    operator: ":="
    right: (func_literal) @body))
(source_file
  (var_declaration
    name: (identifier) @name
    value: (func_literal) @body))
"#;
    static QUERY: OnceLock<Option<Query>> = OnceLock::new();
    let Some(query) = QUERY
        .get_or_init(|| Query::new(&tree_sitter_tengo::LANGUAGE.into(), SRC).ok())
        .as_ref()
    else {
        return Vec::new();
    };
    // Both captures exist in every pattern, so these indices always resolve.
    let (Some(i_name), Some(i_body)) = (
        query.capture_index_for_name("name"),
        query.capture_index_for_name("body"),
    ) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, state.tree.root_node(), state.source.as_bytes());
    while let Some(m) = matches.next() {
        let name = m.captures.iter().find(|c| c.index == i_name);
        let body = m.captures.iter().find(|c| c.index == i_body);
        if let (Some(name), Some(body)) = (name, body) {
            out.push((name.node, body.node));
        }
    }
    out
}

// ── CLI ─────────────────────────────────────────────────────────────────────

/// `tengo-lsp lint [--rules] [paths...]`. Directories are walked for `.tengo`
/// files. Exits 1 when any diagnostic is an error, mirroring the convention
/// that warnings report but do not fail a build.
pub fn run_cli(args: &[String]) -> i32 {
    let mut paths: Vec<&String> = Vec::new();
    let mut list_rules = false;
    for arg in args {
        match arg.as_str() {
            "--rules" => list_rules = true,
            "--help" | "-h" => {
                eprintln!("usage: tengo-lsp lint [--rules] [paths...]");
                return 0;
            }
            _ => paths.push(arg),
        }
    }

    if paths.is_empty() && !list_rules {
        eprintln!("usage: tengo-lsp lint [--rules] [paths...]");
        return 2;
    }

    // The config supplies custom rules and severities, so it is read before
    // anything is listed or linted. With no paths, `--rules` reads it from cwd.
    let config_dir = match paths.first() {
        None => std::path::PathBuf::from("."),
        Some(first) => {
            let start = std::path::Path::new(first.as_str());
            if start.is_dir() {
                start.to_path_buf()
            } else {
                start
                    .parent()
                    .unwrap_or(std::path::Path::new("."))
                    .to_path_buf()
            }
        }
    };
    let (config, problems) = load_config(&config_dir);
    for problem in &problems {
        eprintln!("tengo-lsp lint: config: {problem}");
    }

    if list_rules {
        for rule in RULES {
            println!(
                "{:<26} {:<7} {}",
                rule.name,
                config.severity_for(rule.name, rule.default).label(),
                rule.description
            );
        }
        for rule in &config.custom {
            println!(
                "{:<26} {:<7} (custom) {}",
                rule.name,
                config.severity_for(&rule.name, rule.severity).label(),
                rule.message
            );
        }
        return 0;
    }

    let mut files = Vec::new();
    for p in &paths {
        collect_tengo_files(std::path::Path::new(p.as_str()), &mut files);
    }

    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter_tengo::LANGUAGE.into())
        .is_err()
    {
        eprintln!("tengo-lsp lint: failed to load the Tengo grammar");
        return 2;
    }

    let mut had_error = false;
    let mut findings = 0usize;
    for file in &files {
        let display = file.to_string_lossy().to_string();
        if config.is_excluded(&display) {
            continue;
        }
        let Ok(src) = std::fs::read_to_string(file) else {
            eprintln!("tengo-lsp lint: {display}: cannot read");
            continue;
        };
        let Some(state) = FileState::parse(display.clone(), src, &mut parser) else {
            continue;
        };
        for d in check(&state, &config) {
            let sev = match d.severity {
                Some(DiagnosticSeverity::ERROR) => {
                    had_error = true;
                    "error"
                }
                Some(DiagnosticSeverity::WARNING) => "warning",
                Some(DiagnosticSeverity::INFORMATION) => "info",
                _ => "hint",
            };
            let code = match &d.code {
                Some(NumberOrString::String(s)) => s.clone(),
                _ => String::new(),
            };
            println!(
                "{}:{}:{}: {}: {} [{}]",
                display,
                d.range.start.line + 1,
                d.range.start.character + 1,
                sev,
                d.message,
                code
            );
            findings += 1;
        }
    }

    eprintln!(
        "tengo-lsp lint: {} file(s), {} finding(s)",
        files.len(),
        findings
    );
    i32::from(had_error)
}

/// Recursively gather `.tengo` files, skipping directories that only ever hold
/// generated or vendored copies.
fn collect_tengo_files(path: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    if path.is_file() {
        out.push(path.to_path_buf());
        return;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if p.is_dir() {
            if matches!(name.as_ref(), "node_modules" | "dist" | ".git" | "target") {
                continue;
            }
            collect_tengo_files(&p, out);
        } else if name.ends_with(".tengo") {
            out.push(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lint(source: &str) -> Vec<Diagnostic> {
        lint_as("file:///t.tengo", source)
    }

    fn lint_as(uri: &str, source: &str) -> Vec<Diagnostic> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_tengo::LANGUAGE.into())
            .unwrap();
        let state = FileState::parse(uri.to_string(), source.to_string(), &mut parser)
            .expect("fixture must parse");
        check(&state, &LintConfig::default())
    }

    fn codes(source: &str) -> Vec<String> {
        lint(source)
            .into_iter()
            .filter_map(|d| match d.code {
                Some(NumberOrString::String(s)) => Some(s),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn flags_underscore_export_key() {
        let src = r#"helper := func() { return 1 }

export {
	_helper: helper
}
"#;
        assert_eq!(codes(src), vec!["no-underscore-export-key"]);
    }

    #[test]
    fn flags_underscore_key_through_wrapped_export() {
        // `export ll.toStrict({...})` must be seen through, or the rule misses
        // nearly every SDK file.
        let src = r#"ll := import(":ll")
helper := func() { return 1 }

export ll.toStrict({
	_helper: helper
})
"#;
        assert!(codes(src).contains(&"no-underscore-export-key".to_string()));
    }

    #[test]
    fn accepts_private_helper_reexported_under_public_key() {
        // The `feats.lib.tengo` idiom: private name, public key. Not a finding.
        let src = r#"_getTypes := func(q) { return q }

export {
	getResourceTypes: _getTypes
}
"#;
        assert_eq!(codes(src), Vec::<String>::new());
    }

    #[test]
    fn flags_unused_import_only() {
        let src = r#"used := import(":used")
unused := import(":unused")

export {
	go: func() { return used.thing() }
}
"#;
        let diags = lint(src);
        assert_eq!(diags.len(), 1, "{diags:#?}");
        assert!(diags[0].message.contains("unused"));
        assert!(diags[0].message.contains(":unused"));
    }

    #[test]
    fn flags_unused_function() {
        let src = r#"dead := func() { return 1 }
alive := func() { return 2 }

export {
	alive: alive
}
"#;
        let diags = lint(src);
        assert_eq!(diags.len(), 1, "{diags:#?}");
        assert!(diags[0].message.contains("`dead`"));
    }

    #[test]
    fn locally_called_function_is_used() {
        let src = r#"helper := func() { return 1 }
main := func() { return helper() }

export {
	main: main
}
"#;
        assert_eq!(codes(src), Vec::<String>::new());
    }

    #[test]
    fn recursion_alone_does_not_count_as_use() {
        // `countdown` only ever calls itself, so it is still dead code.
        let src = r#"countdown := func(n) {
	if n <= 0 { return 0 }
	return countdown(n - 1)
}

export {
	other: func() { return 1 }
}
"#;
        let diags = lint(src);
        assert_eq!(diags.len(), 1, "{diags:#?}");
        assert!(diags[0].message.contains("`countdown`"));
    }

    #[test]
    fn test_cases_are_entry_points_not_dead_code() {
        // The runner finds these by name, so neither is a finding. `helper` is
        // called by one of them, and would be reported if the scan were wrong.
        let src = r#"TestThing := func() { return helper() }
TestOther := func() { return 2 }
helper := func() { return 1 }
"#;
        assert_eq!(
            lint_as("file:///x.test.tengo", src)
                .into_iter()
                .map(|d| d.message)
                .collect::<Vec<_>>(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn test_prefix_is_not_excused_outside_test_files() {
        // Same code in a library file: `TestThing` is genuinely unreachable.
        let src = "TestThing := func() { return 1 }\n";
        let diags = lint_as("file:///util.lib.tengo", src);
        assert_eq!(diags.len(), 1, "{diags:#?}");
        assert!(diags[0].message.contains("`TestThing`"));
    }

    #[test]
    fn nested_function_is_not_top_level() {
        // Inner closures are out of scope for the rule; only top level counts.
        let src = r#"main := func() {
	inner := func() { return 1 }
	return inner()
}

export {
	main: main
}
"#;
        assert_eq!(codes(src), Vec::<String>::new());
    }

    #[test]
    fn unparseable_file_reports_nothing() {
        // Backtick map keys are currently a grammar gap; such a file must yield
        // no diagnostics at all rather than a pile of false "unused" claims.
        let src = "helper := func() { return 1 }\nx := { `a,b`: 1 }\n";
        assert_eq!(codes(src), Vec::<String>::new());
    }

    #[test]
    fn severity_off_disables_a_rule() {
        let src = r#"dead := func() { return 1 }
export { other: 1 }
"#;
        let cfg = LintConfig::parse("export { rules: { \"unused-function\": \"off\" } }");
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_tengo::LANGUAGE.into())
            .unwrap();
        let state =
            FileState::parse("file:///t.tengo".to_string(), src.to_string(), &mut parser).unwrap();
        assert!(check(&state, &cfg).is_empty());
    }

    /// Lint with a config, bypassing the built-ins so custom behaviour is
    /// isolated from the three default rules.
    fn lint_with(config_src: &str, source: &str) -> Vec<Diagnostic> {
        // Turn every built-in off so only the custom rules can report.
        let offs: Vec<String> = RULES
            .iter()
            .map(|r| format!("\"{}\": \"off\"", r.name))
            .collect();
        let with_offs = config_src.replacen(
            "export {",
            &format!("export {{\n\trules: {{ {} }},", offs.join(", ")),
            1,
        );
        let cfg = LintConfig::parse(&with_offs);
        assert!(cfg.errors.is_empty(), "config errors: {:?}", cfg.errors);

        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_tengo::LANGUAGE.into())
            .unwrap();
        let state = FileState::parse(
            "file:///t.tengo".to_string(),
            source.to_string(),
            &mut parser,
        )
        .expect("fixture must parse");
        check(&state, &cfg)
    }

    const DEBUG_PRINT_RULE: &str = r#"export {
	custom: [
		{
			name: "no-debug-print",
			severity: "error",
			message: "leftover ll.print({{arg}}) — drop it before committing",
			query: `((call_expression
			           function: (selector_expression
			             object: (identifier) @o
			             field: (identifier) @m)
			           (argument_list (_) @arg)) @node
			         (#eq? @o "ll") (#eq? @m "print"))`
		}
	]
}
"#;

    #[test]
    fn custom_rule_matches_and_renders_captures() {
        let src = r#"ll := import(":ll")
run := func() {
	ll.print("hello")
	return ll.other("kept")
}
export { run: run }
"#;
        let diags = lint_with(DEBUG_PRINT_RULE, src);
        assert_eq!(diags.len(), 1, "{diags:#?}");
        let d = &diags[0];
        assert_eq!(d.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(
            d.code,
            Some(NumberOrString::String("no-debug-print".to_string()))
        );
        // `{{arg}}` expanded to the matched argument text.
        assert_eq!(
            d.message,
            "leftover ll.print(\"hello\") — drop it before committing"
        );
        // Anchored on @node (the whole call), not on an inner capture.
        assert_eq!(d.range.start.line, 2);
        assert_eq!(d.range.start.character, 1);
    }

    #[test]
    fn custom_rule_severity_is_overridable() {
        let cfg = LintConfig::parse(&DEBUG_PRINT_RULE.replacen(
            "export {",
            "export {\n\trules: { \"no-debug-print\": \"off\" },",
            1,
        ));
        assert!(cfg.errors.is_empty(), "{:?}", cfg.errors);
        let src = "ll := import(\":ll\")\nrun := func() { ll.print(\"x\") }\nexport { run: run }\n";
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_tengo::LANGUAGE.into())
            .unwrap();
        let state =
            FileState::parse("file:///t.tengo".to_string(), src.to_string(), &mut parser).unwrap();
        let codes: Vec<String> = check(&state, &cfg)
            .into_iter()
            .filter_map(|d| match d.code {
                Some(NumberOrString::String(s)) => Some(s),
                _ => None,
            })
            .collect();
        assert!(!codes.contains(&"no-debug-print".to_string()), "{codes:?}");
    }

    #[test]
    fn unknown_placeholder_is_left_visible() {
        let cfg = r#"export {
	custom: [
		{ name: "todo", message: "TODO here: {{nope}}", query: `((comment) @node (#match? @node "TODO"))` }
	]
}
"#;
        let diags = lint_with(cfg, "// TODO fix\nx := 1\n");
        assert_eq!(diags.len(), 1, "{diags:#?}");
        assert_eq!(diags[0].message, "TODO here: {{nope}}");
    }

    #[test]
    fn custom_rule_without_node_capture_anchors_on_first() {
        let cfg = r#"export {
	custom: [
		{ name: "no-ternary-bool", message: "simplify `{{t}}`",
		  query: `(ternary_expression consequence: (boolean) alternative: (boolean)) @t` }
	]
}
"#;
        let diags = lint_with(cfg, "x := true\ny := x ? false : true\nexport { y: y }\n");
        assert_eq!(diags.len(), 1, "{diags:#?}");
        assert_eq!(diags[0].message, "simplify `x ? false : true`");
    }

    #[test]
    fn load_config_flags_shadowing_and_unknown_names() {
        let dir = std::env::temp_dir().join("tengo_lint_cfg_test");
        let _ = std::fs::create_dir_all(&dir);
        let cfg = r#"export {
	rules: { "typo-rule": "warn" },
	custom: [
		{ name: "unused-import", query: `((comment) @node)` }
	]
}
"#;
        std::fs::write(dir.join("lint.tengo"), cfg).unwrap();
        let (_, problems) = load_config(&dir);
        assert!(
            problems.iter().any(|p| p.contains("typo-rule")),
            "{problems:?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("shadows a built-in rule")),
            "{problems:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Lint with exactly one rule enabled, whatever its default severity.
    fn lint_only(rule: &str, source: &str) -> Vec<Diagnostic> {
        let settings: Vec<String> = RULES
            .iter()
            .map(|r| {
                let sev = if r.name == rule { "warn" } else { "off" };
                format!("\"{}\": \"{sev}\"", r.name)
            })
            .collect();
        let cfg = LintConfig::parse(&format!(
            "export {{ rules: {{ {} }} }}",
            settings.join(", ")
        ));
        assert!(cfg.errors.is_empty(), "{:?}", cfg.errors);
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_tengo::LANGUAGE.into())
            .unwrap();
        let state = FileState::parse(
            "file:///t.tengo".to_string(),
            source.to_string(),
            &mut parser,
        )
        .expect("fixture must parse");
        check(&state, &cfg)
    }

    fn guard_messages(source: &str) -> Vec<String> {
        lint_only("missing-type-guard", source)
            .into_iter()
            .map(|d| d.message)
            .collect()
    }

    #[test]
    fn type_guard_rule_is_off_by_default() {
        // Adherence is 1.1% across the monorepo, so it must not fire unasked.
        let src = "f := func(a) { return a }\nexport { f: f }\n";
        assert_eq!(codes(src), Vec::<String>::new());
    }

    #[test]
    fn flags_unguarded_exported_parameter() {
        let src = "f := func(cfg) { return cfg.name }\nexport { f: f }\n";
        let msgs = guard_messages(src);
        assert_eq!(msgs.len(), 1, "{msgs:#?}");
        assert!(msgs[0].contains("`cfg`"), "{}", msgs[0]);
        assert!(msgs[0].contains("parameter "), "{}", msgs[0]);
    }

    #[test]
    fn builtin_and_sdk_guards_both_count() {
        // `is_undefined` (Tengo builtin), `ll.isMap` (SDK camelCase) and
        // `validation.assertType` must all satisfy the rule.
        let src = r#"ll := import(":ll")
validation := import(":validation")

a := func(x) {
	if is_undefined(x) { return 0 }
	return x
}
b := func(y) {
	if !ll.isMap(y) { return 0 }
	return y
}
c := func(z) {
	validation.assertType(z, "string")
	return z
}
export { a: a, b: b, c: c }
"#;
        assert_eq!(guard_messages(src), Vec::<String>::new());
    }

    #[test]
    fn reports_only_the_unguarded_parameter() {
        // The `expressionTraverse` shape: first arg guarded, callback is not.
        let src = r#"walk := func(expression, visitFn) {
	if !isExpression(expression) { return }
	visitFn(expression)
}
export { walk: walk }
"#;
        let msgs = guard_messages(src);
        assert_eq!(msgs.len(), 1, "{msgs:#?}");
        assert!(msgs[0].contains("`visitFn`"), "{}", msgs[0]);
        assert!(!msgs[0].contains("expression"), "{}", msgs[0]);
    }

    #[test]
    fn variadic_and_underscore_params_are_exempt() {
        // `...rest` is an array by construction; `_` is a throwaway.
        let src = r#"f := func(...rest) { return len(rest) }
g := func(_) { return 1 }
export { f: f, g: g }
"#;
        assert_eq!(guard_messages(src), Vec::<String>::new());
    }

    #[test]
    fn unexported_and_zero_arg_functions_are_exempt() {
        let src = r#"helper := func(raw) { return raw.x }
noArgs := func() { return 1 }
export { noArgs: noArgs }
"#;
        // `helper` is private; `noArgs` takes nothing. Neither is reported.
        assert_eq!(guard_messages(src), Vec::<String>::new());
    }

    #[test]
    fn guard_inside_a_nested_closure_still_counts() {
        let src = r#"f := func(cfg) {
	return func() {
		if is_undefined(cfg) { return 0 }
		return cfg.v
	}
}
export { f: f }
"#;
        assert_eq!(guard_messages(src), Vec::<String>::new());
    }

    #[test]
    fn type_guard_names_are_recognised() {
        for name in [
            "is_undefined",
            "is_map",
            "isMap",
            "isAssetInfo",
            "assertType",
            "assert",
            "checkJson",
            "type_name",
        ] {
            assert!(is_type_guard(name), "{name} should count as a guard");
        }
        for name in ["island", "issue", "isolate", "print", "len", "append"] {
            assert!(!is_type_guard(name), "{name} must not count as a guard");
        }
    }

    #[test]
    fn rule_names_are_unique() {
        let mut names: Vec<&str> = RULES.iter().map(|r| r.name).collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count);
    }
}

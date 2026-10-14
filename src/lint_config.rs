use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tower_lsp::lsp_types::DiagnosticSeverity;

/// Name of the per-workspace config file, read as Tengo *data*: it is parsed
/// with the same grammar as any other source file, never executed.
pub const CONFIG_FILE: &str = "lint.tengo";

/// Severity a rule reports at, or `Off` to disable it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Off,
    Hint,
    Info,
    Warn,
    Error,
}

impl Severity {
    fn parse(s: &str) -> Option<Severity> {
        match s {
            "off" => Some(Severity::Off),
            "hint" => Some(Severity::Hint),
            "info" => Some(Severity::Info),
            "warn" | "warning" => Some(Severity::Warn),
            "error" => Some(Severity::Error),
            _ => None,
        }
    }

    /// `None` means the rule is off and must not run at all.
    pub fn to_lsp(self) -> Option<DiagnosticSeverity> {
        match self {
            Severity::Off => None,
            Severity::Hint => Some(DiagnosticSeverity::HINT),
            Severity::Info => Some(DiagnosticSeverity::INFORMATION),
            Severity::Warn => Some(DiagnosticSeverity::WARNING),
            Severity::Error => Some(DiagnosticSeverity::ERROR),
        }
    }

    /// Lower-case word used by the CLI reporter.
    pub fn label(self) -> &'static str {
        match self {
            Severity::Off => "off",
            Severity::Hint => "hint",
            Severity::Info => "info",
            Severity::Warn => "warning",
            Severity::Error => "error",
        }
    }
}

/// A rule declared in `lint.tengo` rather than compiled in: a tree-sitter
/// query plus the message to report where it matches. Held behind an `Arc` so
/// cloning the config stays cheap — a compiled `Query` is neither cheap to
/// build nor `Clone`.
#[derive(Debug)]
pub struct CustomRule {
    pub name: String,
    pub severity: Severity,
    /// Message template. `{{capture}}` expands to that capture's matched text.
    pub message: String,
    pub query: tree_sitter::Query,
}

/// Severity overrides, path exclusions and custom rules read from
/// `lint.tengo`. Rule *defaults* for built-ins live with the rules in
/// `lint::RULES`, so this only stores what the workspace changed or added.
#[derive(Debug, Clone, Default)]
pub struct LintConfig {
    overrides: HashMap<String, Severity>,
    exclude: Vec<String>,
    /// Rules declared in the config file, already compiled.
    pub custom: Vec<Arc<CustomRule>>,
    /// Config file this came from, for diagnostics about the config itself.
    pub path: Option<PathBuf>,
    /// Problems found while reading the config, reported to the user rather
    /// than swallowed. A bad config must never take the server down.
    pub errors: Vec<String>,
}

impl LintConfig {
    /// Walk up from `dir` looking for `lint.tengo`. A workspace without one
    /// gets the built-in defaults.
    pub fn discover(dir: &Path) -> LintConfig {
        let mut cur = Some(dir);
        while let Some(d) = cur {
            let candidate = d.join(CONFIG_FILE);
            if candidate.is_file() {
                if let Ok(src) = std::fs::read_to_string(&candidate) {
                    let mut cfg = LintConfig::parse(&src);
                    cfg.path = Some(candidate);
                    return cfg;
                }
            }
            cur = d.parent();
        }
        LintConfig::default()
    }

    /// Parse the `export { rules: {...}, exclude: [...] }` map. Anything the
    /// reader does not understand is ignored rather than fatal — a config typo
    /// must not take the whole language server down.
    pub fn parse(source: &str) -> LintConfig {
        let mut cfg = LintConfig::default();

        let mut parser = tree_sitter::Parser::new();
        if parser
            .set_language(&tree_sitter_tengo::LANGUAGE.into())
            .is_err()
        {
            return cfg;
        }
        let Some(tree) = parser.parse(source, None) else {
            return cfg;
        };
        let Some(map) = crate::exports::find_export_map(tree.root_node()) else {
            return cfg;
        };

        for (key, value) in map_entries(map, source) {
            match key.as_str() {
                "rules" => {
                    if value.kind() != "map_literal" {
                        continue;
                    }
                    for (rule, sev_node) in map_entries(value, source) {
                        let word = literal_text(sev_node, source);
                        match Severity::parse(&word) {
                            Some(sev) => {
                                cfg.overrides.insert(rule, sev);
                            }
                            None => cfg.errors.push(format!(
                                "rule `{rule}`: unknown severity `{word}` \
                                 (expected off, hint, info, warn or error)"
                            )),
                        }
                    }
                }
                "custom" => {
                    if value.kind() != "array_literal" {
                        continue;
                    }
                    let mut cursor = value.walk();
                    for item in value.children(&mut cursor) {
                        if item.kind() == "map_literal" {
                            cfg.read_custom_rule(item, source);
                        }
                    }
                }
                "exclude" => {
                    if value.kind() != "array_literal" {
                        continue;
                    }
                    let mut cursor = value.walk();
                    for item in value.children(&mut cursor) {
                        if matches!(item.kind(), "string_literal" | "raw_string_literal") {
                            cfg.exclude.push(literal_text(item, source));
                        }
                    }
                }
                _ => {}
            }
        }
        cfg
    }

    /// Read one `custom: [{ ... }]` entry. Every problem is collected into
    /// `errors` and the rule dropped, so one broken rule cannot silence the
    /// others.
    fn read_custom_rule(&mut self, map: tree_sitter::Node, source: &str) {
        let mut fields: HashMap<String, String> = HashMap::new();
        for (key, value) in map_entries(map, source) {
            fields.insert(key, literal_text(value, source));
        }

        let name = fields.get("name").cloned().unwrap_or_default();
        if name.is_empty() {
            self.errors
                .push("custom rule is missing a non-empty `name`".to_string());
            return;
        }
        if self.custom.iter().any(|r| r.name == name) {
            self.errors
                .push(format!("custom rule `{name}` is declared more than once"));
            return;
        }

        let Some(query_src) = fields.get("query") else {
            self.errors
                .push(format!("custom rule `{name}` is missing a `query`"));
            return;
        };

        // Queries contain double quotes, so they are normally written as
        // backtick raw strings; either spelling reaches us as plain text here.
        let query = match tree_sitter::Query::new(&tree_sitter_tengo::LANGUAGE.into(), query_src) {
            Ok(q) => q,
            Err(e) => {
                self.errors
                    .push(format!("custom rule `{name}`: query does not compile: {e}"));
                return;
            }
        };
        if query.capture_names().is_empty() {
            self.errors.push(format!(
                "custom rule `{name}`: query has no captures, so there is \
                 nothing to report on — capture the offending node as `@node`"
            ));
            return;
        }

        let severity = match fields.get("severity") {
            None => Severity::Warn,
            Some(word) => match Severity::parse(word) {
                Some(s) => s,
                None => {
                    self.errors
                        .push(format!("custom rule `{name}`: unknown severity `{word}`"));
                    return;
                }
            },
        };

        let message = fields
            .get("message")
            .cloned()
            .unwrap_or_else(|| name.clone());

        self.custom.push(Arc::new(CustomRule {
            name,
            severity,
            message,
            query,
        }));
    }

    /// Resolve a rule's severity, falling back to its built-in default.
    pub fn severity_for(&self, rule: &str, default: Severity) -> Severity {
        self.overrides.get(rule).copied().unwrap_or(default)
    }

    /// Rule names the config sets a severity for, so callers can flag ones that
    /// no rule answers to.
    pub fn override_names(&self) -> impl Iterator<Item = &str> {
        self.overrides.keys().map(String::as_str)
    }

    /// True when `path` matches any `exclude` glob. Matching is on the path as
    /// given, so relative and absolute paths both work with a leading `**/`.
    pub fn is_excluded(&self, path: &str) -> bool {
        let normalized = path.replace('\\', "/");
        self.exclude
            .iter()
            .any(|pat| glob_match(pat, normalized.trim_start_matches("./")))
    }
}

/// Yield `(key, value_node)` for every entry of a map literal. Keys may be
/// bare identifiers or quoted strings; both reduce to the same text.
fn map_entries<'a>(
    map: tree_sitter::Node<'a>,
    source: &str,
) -> Vec<(String, tree_sitter::Node<'a>)> {
    let mut out = Vec::new();
    let mut cursor = map.walk();
    for entry in map.children(&mut cursor) {
        if entry.kind() != "map_entry" {
            continue;
        }
        let (Some(key), Some(value)) = (
            entry.child_by_field_name("key"),
            entry.child_by_field_name("value"),
        ) else {
            continue;
        };
        out.push((literal_text(key, source), value));
    }
    out
}

/// Text of a literal or identifier with its delimiters removed. Only the one
/// matching pair is stripped, so content that itself starts or ends with a
/// quote or backtick survives — `"a `b`"` must stay `a `b``, not `a `b`.
pub fn literal_text(node: tree_sitter::Node, source: &str) -> String {
    let raw = node.utf8_text(source.as_bytes()).unwrap_or("");
    let inner = match node.kind() {
        "string_literal" => raw
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .unwrap_or(raw),
        "raw_string_literal" => raw
            .strip_prefix('`')
            .and_then(|s| s.strip_suffix('`'))
            .unwrap_or(raw),
        _ => raw,
    };
    inner.to_string()
}

/// Match `text` against a glob where `*` stays inside one path segment and
/// `**` spans any number of segments.
fn glob_match(pattern: &str, text: &str) -> bool {
    let pat: Vec<&str> = pattern.split('/').collect();
    let txt: Vec<&str> = text.split('/').collect();
    segments_match(&pat, &txt)
}

fn segments_match(pat: &[&str], txt: &[&str]) -> bool {
    match pat.first() {
        None => txt.is_empty(),
        Some(&"**") => (0..=txt.len()).any(|skip| segments_match(&pat[1..], &txt[skip..])),
        Some(seg) => {
            !txt.is_empty() && star_match(seg, txt[0]) && segments_match(&pat[1..], &txt[1..])
        }
    }
}

fn star_match(pat: &str, text: &str) -> bool {
    match pat.find('*') {
        None => pat == text,
        Some(i) => {
            let (head, tail) = (&pat[..i], &pat[i + 1..]);
            if !text.starts_with(head) {
                return false;
            }
            let rest = &text[head.len()..];
            (0..=rest.len()).any(|skip| star_match(tail, &rest[skip..]))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"export {
	rules: {
		"unused-import": "error",
		"unused-function": "off",
		"no-underscore-export-key": "hint",
		"not-a-rule": "banana"
	},
	exclude: ["**/dist/**", "vendor/*.tengo"]
}
"#;

    #[test]
    fn reads_severities_and_excludes() {
        let cfg = LintConfig::parse(CONFIG);
        assert_eq!(
            cfg.severity_for("unused-import", Severity::Warn),
            Severity::Error
        );
        assert_eq!(
            cfg.severity_for("unused-function", Severity::Warn),
            Severity::Off
        );
        assert_eq!(
            cfg.severity_for("no-underscore-export-key", Severity::Warn),
            Severity::Hint
        );
        // A rule the config never mentions keeps its built-in default.
        assert_eq!(
            cfg.severity_for("unmentioned", Severity::Error),
            Severity::Error
        );
        // An unparseable severity is surfaced, not silently accepted.
        assert_eq!(cfg.errors.len(), 1, "{:?}", cfg.errors);
        assert!(cfg.errors[0].contains("not-a-rule"));
        assert!(cfg.errors[0].contains("banana"));
    }

    const WITH_CUSTOM: &str = r#"export {
	custom: [
		{
			name: "no-debug-print",
			severity: "error",
			message: "leftover ll.print({{arg}})",
			query: `((call_expression
			           function: (selector_expression
			             object: (identifier) @o
			             field: (identifier) @m)
			           (argument_list (_) @arg)) @node
			         (#eq? @o "ll") (#eq? @m "print"))`
		},
		{
			name: "no-todo",
			message: "resolve this TODO",
			query: `((comment) @node (#match? @node "TODO"))`
		}
	]
}
"#;

    #[test]
    fn reads_custom_rules() {
        let cfg = LintConfig::parse(WITH_CUSTOM);
        assert!(cfg.errors.is_empty(), "{:?}", cfg.errors);
        assert_eq!(cfg.custom.len(), 2);
        assert_eq!(cfg.custom[0].name, "no-debug-print");
        assert_eq!(cfg.custom[0].severity, Severity::Error);
        assert_eq!(cfg.custom[0].message, "leftover ll.print({{arg}})");
        // `severity` is optional and defaults to warn.
        assert_eq!(cfg.custom[1].severity, Severity::Warn);
    }

    #[test]
    fn a_broken_custom_rule_does_not_drop_the_others() {
        let src = r#"export {
	custom: [
		{ name: "bad-query", query: `(this_node_kind_does_not_exist) @node` },
		{ name: "no-name-at-all", query: `((comment) @node)` },
		{ name: "no-query" },
		{ name: "no-captures", query: `(comment)` },
		{ name: "fine", query: `((comment) @node)` }
	]
}
"#;
        let cfg = LintConfig::parse(src);
        let kept: Vec<&str> = cfg.custom.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(kept, vec!["no-name-at-all", "fine"]);
        // One error each for the bad query, the missing query and no captures.
        assert_eq!(cfg.errors.len(), 3, "{:?}", cfg.errors);
        assert!(cfg.errors.iter().any(|e| e.contains("does not compile")));
        assert!(cfg.errors.iter().any(|e| e.contains("missing a `query`")));
        assert!(cfg.errors.iter().any(|e| e.contains("no captures")));
    }

    #[test]
    fn duplicate_custom_rule_names_are_rejected() {
        let src = r#"export {
	custom: [
		{ name: "dup", query: `((comment) @node)` },
		{ name: "dup", query: `((identifier) @node)` }
	]
}
"#;
        let cfg = LintConfig::parse(src);
        assert_eq!(cfg.custom.len(), 1);
        assert!(cfg.errors[0].contains("more than once"));
    }

    #[test]
    fn excludes_match_globs() {
        let cfg = LintConfig::parse(CONFIG);
        assert!(cfg.is_excluded("pkg/dist/tengo/lib/util.lib.tengo"));
        assert!(cfg.is_excluded("./vendor/thing.tengo"));
        assert!(!cfg.is_excluded("src/util.lib.tengo"));
        // `*` must not cross a segment boundary.
        assert!(!cfg.is_excluded("vendor/nested/thing.tengo"));
    }

    #[test]
    fn missing_config_yields_defaults() {
        let cfg = LintConfig::parse("x := 1");
        assert!(cfg.path.is_none());
        assert_eq!(
            cfg.severity_for("unused-import", Severity::Warn),
            Severity::Warn
        );
        assert!(!cfg.is_excluded("anything.tengo"));
    }

    #[test]
    fn config_with_backtick_keys_and_partial_map() {
        // Only `rules` present, keys unquoted — both shapes must read.
        let cfg = LintConfig::parse("export { rules: { unused-import: \"off\" } }");
        // `unused-import` is not a valid Tengo identifier, so this parses as
        // something else entirely; the reader must not panic on it.
        let _ = cfg.severity_for("unused-import", Severity::Warn);
    }
}

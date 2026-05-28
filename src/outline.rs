use tower_lsp::lsp_types::{DocumentSymbol, DocumentSymbolResponse, Range, SymbolKind as LspKind};

use crate::document::FileState;
use crate::exports::{parse_exports, ItemKind};
use crate::symbols::node_to_lsp_range;

/// Handle textDocument/documentSymbol.
///
/// Walks the parse tree directly so that each top-level definition's range
/// covers the entire statement (the LSP convention) and so that *builder
/// methods* — `name: func(...) { ... }` entries inside a map literal — can be
/// nested under the top-level def that contains them.
///
/// Emits, in file order:
/// - top-level functions / variables / imports;
/// - an `export` group whose children are the keys of the `export { ... }` map.
pub fn document_symbols(state: &FileState) -> Option<DocumentSymbolResponse> {
    let source = state.source.as_bytes();
    let root = state.tree.root_node();
    let mut out: Vec<DocumentSymbol> = Vec::new();

    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        match child.kind() {
            "var_declaration" => {
                if let Some(sym) = build_decl_symbol(child, source) {
                    out.push(sym);
                }
            }
            "assignment_statement" => {
                if is_short_var_decl(child) {
                    if let Some(sym) = build_decl_symbol(child, source) {
                        out.push(sym);
                    }
                }
            }
            "export_statement" => {
                if let Some(sym) = build_export_group(state) {
                    out.push(sym);
                }
            }
            _ => {}
        }
    }

    if out.is_empty() {
        None
    } else {
        Some(DocumentSymbolResponse::Nested(out))
    }
}

fn is_short_var_decl(node: tree_sitter::Node) -> bool {
    node.child_by_field_name("operator")
        .map_or(false, |op| op.kind() == ":=")
}

/// Build a DocumentSymbol for `var_declaration` or `assignment_statement` whose
/// left-hand side is an identifier. Collects builder-method children from the
/// value subtree.
fn build_decl_symbol(stmt: tree_sitter::Node, source: &[u8]) -> Option<DocumentSymbol> {
    let (name_node, value_node) = match stmt.kind() {
        "var_declaration" => (
            stmt.child_by_field_name("name")?,
            stmt.child_by_field_name("value"),
        ),
        "assignment_statement" => {
            let left = stmt.child_by_field_name("left")?;
            if left.kind() != "identifier" {
                return None;
            }
            (left, stmt.child_by_field_name("right"))
        }
        _ => return None,
    };

    let name = name_node.utf8_text(source).ok()?.to_string();
    let (kind, detail) = classify_value(value_node, source, &name);
    let children = value_node
        .map(|v| collect_method_entries(v, source))
        .unwrap_or_default();

    #[allow(deprecated)]
    Some(DocumentSymbol {
        name,
        detail,
        kind,
        tags: None,
        deprecated: None,
        range: node_to_lsp_range(stmt),
        selection_range: node_to_lsp_range(name_node),
        children: if children.is_empty() { None } else { Some(children) },
    })
}

fn classify_value(
    value: Option<tree_sitter::Node>,
    source: &[u8],
    _name: &str,
) -> (LspKind, Option<String>) {
    match value.map(|v| (v.kind(), v)) {
        Some(("func_literal", v)) => (LspKind::FUNCTION, render_param_list(v, source)),
        Some(("import_expression", v)) => (
            LspKind::MODULE,
            extract_import_path(v, source).map(|p| format!("import \"{p}\"")),
        ),
        _ => (LspKind::VARIABLE, None),
    }
}

/// Render `(a, b, ...rest)` for a `func_literal` node.
fn render_param_list(func: tree_sitter::Node, source: &[u8]) -> Option<String> {
    let mut params: Vec<String> = Vec::new();
    let mut cursor = func.walk();
    for child in func.children(&mut cursor) {
        if child.kind() == "parameter_list" {
            let mut pc = child.walk();
            for p in child.named_children(&mut pc) {
                match p.kind() {
                    "identifier" => {
                        params.push(p.utf8_text(source).unwrap_or("").to_string());
                    }
                    "variadic_parameter" => {
                        let ident = p
                            .named_child(0)
                            .and_then(|i| i.utf8_text(source).ok())
                            .unwrap_or("");
                        params.push(format!("...{ident}"));
                    }
                    _ => {}
                }
            }
        }
    }
    Some(format!("({})", params.join(", ")))
}

fn extract_import_path(node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "string_literal" {
            let raw = child.utf8_text(source).ok()?;
            return Some(raw.trim_matches('"').to_string());
        }
    }
    None
}

/// Walk `node` and collect every `key: func(...)` map entry as a Function
/// DocumentSymbol. Stops descending into a map_entry once emitted — nested
/// methods are gathered recursively from inside the func_literal's body so each
/// becomes a child of the enclosing method.
fn collect_method_entries(node: tree_sitter::Node, source: &[u8]) -> Vec<DocumentSymbol> {
    let mut out = Vec::new();
    walk_for_methods(node, source, &mut out);
    out
}

fn walk_for_methods(node: tree_sitter::Node, source: &[u8], out: &mut Vec<DocumentSymbol>) {
    if node.kind() == "map_entry" {
        if let (Some(key), Some(value)) = (
            node.child_by_field_name("key"),
            node.child_by_field_name("value"),
        ) {
            if key.kind() == "identifier" && value.kind() == "func_literal" {
                if let Ok(name) = key.utf8_text(source) {
                    let sub = collect_method_entries(value, source);
                    #[allow(deprecated)]
                    out.push(DocumentSymbol {
                        name: name.to_string(),
                        detail: render_param_list(value, source),
                        kind: LspKind::METHOD,
                        tags: None,
                        deprecated: None,
                        range: node_to_lsp_range(node),
                        selection_range: node_to_lsp_range(key),
                        children: if sub.is_empty() { None } else { Some(sub) },
                    });
                    return; // children already harvested from `value`
                }
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_for_methods(child, source, out);
    }
}

fn build_export_group(state: &FileState) -> Option<DocumentSymbol> {
    let exports = parse_exports(&state.source);
    if exports.is_empty() {
        return None;
    }
    let children: Vec<DocumentSymbol> = exports
        .iter()
        .map(|m| {
            let kind = match m.kind {
                ItemKind::Function => LspKind::FUNCTION,
                ItemKind::Variable => LspKind::FIELD,
            };
            #[allow(deprecated)]
            DocumentSymbol {
                name: m.name.clone(),
                detail: m.signature.clone(),
                kind,
                tags: None,
                deprecated: None,
                range: m.key_range,
                selection_range: m.key_range,
                children: None,
            }
        })
        .collect();

    let group_range = Range {
        start: exports.first().unwrap().key_range.start,
        end: exports.last().unwrap().key_range.end,
    };
    let n = exports.len();
    #[allow(deprecated)]
    Some(DocumentSymbol {
        name: "export".to_string(),
        detail: Some(format!("{} member{}", n, if n == 1 { "" } else { "s" })),
        kind: LspKind::NAMESPACE,
        tags: None,
        deprecated: None,
        range: group_range,
        selection_range: group_range,
        children: Some(children),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn parse(src: &str) -> FileState {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_tengo::LANGUAGE.into())
            .unwrap();
        FileState::parse("file:///t.tengo".into(), src.into(), &mut parser).unwrap()
    }

    fn nested(resp: DocumentSymbolResponse) -> Vec<DocumentSymbol> {
        match resp {
            DocumentSymbolResponse::Nested(v) => v,
            DocumentSymbolResponse::Flat(_) => panic!("expected nested response"),
        }
    }

    #[test]
    fn lists_top_level_imports_funcs_vars() {
        let src = "fmt := import(\"fmt\")\nu := import(\":util\")\n\nname := \"x\"\n\nadd := func(a, b) { return a + b }\n";
        let syms = nested(document_symbols(&parse(src)).expect("symbols"));
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["fmt", "u", "name", "add"]);

        let kinds: Vec<LspKind> = syms.iter().map(|s| s.kind).collect();
        assert_eq!(kinds, vec![LspKind::MODULE, LspKind::MODULE, LspKind::VARIABLE, LspKind::FUNCTION]);

        // Import detail surfaces the module path.
        assert_eq!(syms[1].detail.as_deref(), Some("import \":util\""));
        // Function detail surfaces its parameter list.
        assert_eq!(syms[3].detail.as_deref(), Some("(a, b)"));
    }

    #[test]
    fn exports_appear_as_nested_group() {
        let src = "doThing := func(x) { return x }\nname := \"x\"\n\nexport {\n    doThing: doThing,\n    name: name\n}\n";
        let syms = nested(document_symbols(&parse(src)).expect("symbols"));
        let group = syms.iter().find(|s| s.name == "export").expect("export group");
        assert_eq!(group.kind, LspKind::NAMESPACE);
        let children = group.children.as_ref().expect("children");
        let kid_names: Vec<&str> = children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(kid_names, vec!["doThing", "name"]);
        assert_eq!(children[0].kind, LspKind::FUNCTION);
        assert_eq!(children[1].kind, LspKind::FIELD);
    }

    #[test]
    fn empty_file_returns_none() {
        assert!(document_symbols(&parse("")).is_none());
    }

    #[test]
    fn function_locals_do_not_appear() {
        let src = "f := func() {\n    local := 1\n    return local\n}\n";
        let syms = nested(document_symbols(&parse(src)).expect("symbols"));
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["f"], "only top-level `f` should appear");
    }

    #[test]
    fn builder_methods_become_children() {
        // Mirrors workflow-tengo/src/exec/index.lib.tengo: a top-level def that
        // returns a map literal of methods.
        let src = "make := func() {\n    return {\n        env: func(name, value) {\n            return self\n        },\n        argExpr: func(arg) {\n            return self\n        }\n    }\n}\n";
        let syms = nested(document_symbols(&parse(src)).expect("symbols"));
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "make");
        let kids = syms[0].children.as_ref().expect("children");
        let kid_names: Vec<&str> = kids.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(kid_names, vec!["env", "argExpr"]);
        assert_eq!(kids[0].kind, LspKind::METHOD);
        assert_eq!(kids[0].detail.as_deref(), Some("(name, value)"));
    }
}

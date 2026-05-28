use tower_lsp::lsp_types::{DocumentSymbol, DocumentSymbolResponse, Range, SymbolKind as LspKind};

use crate::document::FileState;
use crate::exports::{parse_exports, ItemKind};
use crate::symbols::SymbolKind;

/// Handle textDocument/documentSymbol.
///
/// Emits top-level imports, functions and variables, followed by an `export`
/// group whose children are the keys of the file's `export { ... }` map.
pub fn document_symbols(state: &FileState) -> Option<DocumentSymbolResponse> {
    let mut symbols: Vec<DocumentSymbol> = Vec::new();

    for def in &state.defs {
        // Top-level only — defs in nested scopes are local to their function.
        let is_top = state
            .scopes
            .get(def.scope_id)
            .map_or(false, |s| s.parent.is_none());
        if !is_top {
            continue;
        }

        let (kind, detail) = match def.kind {
            SymbolKind::Import => (
                LspKind::MODULE,
                state
                    .imports
                    .get(&def.name)
                    .map(|i| format!("import \"{}\"", i.module_path)),
            ),
            SymbolKind::Func => (LspKind::FUNCTION, None),
            SymbolKind::Var => (LspKind::VARIABLE, None),
            // Param / ForVar can't appear at root scope.
            SymbolKind::Param | SymbolKind::ForVar => continue,
        };

        #[allow(deprecated)]
        symbols.push(DocumentSymbol {
            name: def.name.clone(),
            detail,
            kind,
            tags: None,
            deprecated: None,
            range: def.range,
            selection_range: def.range,
            children: None,
        });
    }

    // Exports group with its members as children.
    let exports = parse_exports(&state.source);
    if !exports.is_empty() {
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
        symbols.push(DocumentSymbol {
            name: "export".to_string(),
            detail: Some(format!("{} member{}", n, if n == 1 { "" } else { "s" })),
            kind: LspKind::NAMESPACE,
            tags: None,
            deprecated: None,
            range: group_range,
            selection_range: group_range,
            children: Some(children),
        });
    }

    if symbols.is_empty() {
        None
    } else {
        Some(DocumentSymbolResponse::Nested(symbols))
    }
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
}

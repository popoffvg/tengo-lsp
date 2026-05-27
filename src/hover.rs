use tower_lsp::lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position};

use crate::document::FileState;
use crate::exports::{self, ExportMember, ItemKind};
use crate::resolver;

/// Handle textDocument/hover. When the cursor is on the member of
/// `<alias>.<member>` and `<alias>` is an imported module, show the member's
/// signature and doc comment.
pub fn hover(state: &FileState, position: Position) -> Option<Hover> {
    let (alias, member) = alias_member_at(state, position)?;
    let import_info = state.imports.get(&alias)?;

    let current_file = resolver::uri_to_path(&state.uri)?;
    let resolved = resolver::resolve_import(&import_info.module_path, &current_file)?;
    let source = std::fs::read_to_string(&resolved).ok()?;

    let members = exports::parse_exports(&source);
    let found = members.into_iter().find(|m| m.name == member)?;

    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: render_hover(&found),
        }),
        range: None,
    })
}

fn render_hover(m: &ExportMember) -> String {
    let head = match m.kind {
        ItemKind::Function => match &m.signature {
            Some(sig) => format!("{}{}", m.name, sig),
            None => m.name.clone(),
        },
        ItemKind::Variable => m.name.clone(),
    };
    let mut out = format!("```tengo\n{head}\n```");
    if let Some(doc) = &m.doc {
        out.push_str("\n\n");
        out.push_str(doc);
    }
    out
}

/// Resolve `<alias>.<member>` at `position`, where the cursor is on `member`.
fn alias_member_at(state: &FileState, position: Position) -> Option<(String, String)> {
    let point = tree_sitter::Point {
        row: position.line as usize,
        column: position.character as usize,
    };
    let root = state.tree.root_node();
    let node = root.descendant_for_point_range(point, point)?;
    if node.kind() != "identifier" {
        return None;
    }
    let parent = node.parent()?;
    if parent.kind() != "selector_expression" {
        return None;
    }
    let field = parent.child_by_field_name("field")?;
    if field.id() != node.id() {
        return None;
    }
    let object = parent.child_by_field_name("object")?;
    if object.kind() != "identifier" {
        return None;
    }
    let alias = object.utf8_text(state.source.as_bytes()).ok()?.to_string();
    let member = field.utf8_text(state.source.as_bytes()).ok()?.to_string();
    Some((alias, member))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exports::{ExportMember, ItemKind};
    use tower_lsp::lsp_types::{Position, Range};

    fn rng() -> Range {
        Range::new(Position::new(0, 0), Position::new(0, 0))
    }

    #[test]
    fn renders_function_with_doc() {
        let m = ExportMember {
            name: "createFileDataset".into(),
            key_range: rng(),
            kind: ItemKind::Function,
            signature: Some("(a, b)".into()),
            doc: Some("Builds a dataset.".into()),
        };
        let s = render_hover(&m);
        assert!(s.contains("```tengo\ncreateFileDataset(a, b)\n```"));
        assert!(s.ends_with("Builds a dataset."));
    }

    #[test]
    fn renders_variable_without_doc() {
        let m = ExportMember {
            name: "isGrouped".into(),
            key_range: rng(),
            kind: ItemKind::Variable,
            signature: None,
            doc: None,
        };
        let s = render_hover(&m);
        assert_eq!(s, "```tengo\nisGrouped\n```");
    }

    #[test]
    fn end_to_end_hover_resolves_member() {
        let base = std::env::temp_dir().join(format!("tengo-lsp-hover-{}", std::process::id()));
        let src_dir = base.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(base.join("package.json"), "{}").unwrap();
        let lib = "// Adds two numbers.\nadd := func(a, b) {\n    return a + b\n}\n\nexport {\n    add: add\n}\n";
        std::fs::write(src_dir.join("util.lib.tengo"), lib).unwrap();

        let main_src = "util := import(\":util\")\nx := util.add\n";
        std::fs::write(src_dir.join("main.lib.tengo"), main_src).unwrap();

        let uri = format!("file://{}", src_dir.join("main.lib.tengo").display());
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_tengo::LANGUAGE.into())
            .unwrap();
        let state = FileState::parse(uri, main_src.into(), &mut parser).unwrap();

        let col = main_src.lines().nth(1).unwrap().find("add").unwrap() as u32;
        let h = hover(&state, Position::new(1, col));

        std::fs::remove_dir_all(&base).ok();

        let h = h.expect("hover should resolve add");
        match h.contents {
            HoverContents::Markup(mc) => {
                assert!(mc.value.contains("add(a, b)"));
                assert!(mc.value.contains("Adds two numbers."));
            }
            other => panic!("expected markup, got {other:?}"),
        }
    }

    #[test]
    fn unresolvable_module_no_panic() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_tengo::LANGUAGE.into())
            .unwrap();
        let src = "util := import(\":nonexistent\")\nx := util.add\n";
        let state =
            FileState::parse("file:///nowhere/main.lib.tengo".into(), src.into(), &mut parser)
                .unwrap();
        let col = src.lines().nth(1).unwrap().find("add").unwrap() as u32;
        assert!(hover(&state, Position::new(1, col)).is_none());
    }
}

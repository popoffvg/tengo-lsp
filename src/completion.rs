use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, CompletionResponse, Documentation, Position,
};

use crate::document::FileState;
use crate::exports::{self, ExportMember, ItemKind};
use crate::resolver;

/// Handle textDocument/completion. Offers exported members of an imported
/// module when the cursor follows `<alias>.`.
pub fn completion(state: &FileState, position: Position) -> Option<CompletionResponse> {
    let alias = alias_at_completion(state, position)?;
    let import_info = state.imports.get(&alias)?;

    let current_file = resolver::uri_to_path(&state.uri)?;
    let resolved = resolver::resolve_import(&import_info.module_path, &current_file)?;
    let source = std::fs::read_to_string(&resolved).ok()?;

    let members = exports::parse_exports(&source);
    if members.is_empty() {
        return None;
    }

    let items: Vec<CompletionItem> = members.into_iter().map(to_completion_item).collect();
    Some(CompletionResponse::Array(items))
}

fn to_completion_item(m: ExportMember) -> CompletionItem {
    let kind = match m.kind {
        ItemKind::Function => CompletionItemKind::FUNCTION,
        ItemKind::Variable => CompletionItemKind::VARIABLE,
    };
    let detail = m
        .signature
        .as_ref()
        .map(|s| format!("{}{}", m.name, s));
    CompletionItem {
        label: m.name,
        kind: Some(kind),
        detail,
        documentation: m.doc.map(Documentation::String),
        ..Default::default()
    }
}

/// If the cursor is in a `<alias>.` member-access position, return the alias.
/// Tries the AST (selector_expression field) first, then a byte-scan fallback
/// for the common just-typed-a-dot case where the parse has no field yet.
fn alias_at_completion(state: &FileState, position: Position) -> Option<String> {
    let point = tree_sitter::Point {
        row: position.line as usize,
        column: position.character as usize,
    };
    let root = state.tree.root_node();

    // AST path: cursor on the `field` identifier of a selector_expression.
    if let Some(node) = root.descendant_for_point_range(point, point) {
        if node.kind() == "identifier" {
            if let Some(parent) = node.parent() {
                if parent.kind() == "selector_expression" {
                    if let Some(field) = parent.child_by_field_name("field") {
                        if field.id() == node.id() {
                            if let Some(obj) = parent.child_by_field_name("object") {
                                if obj.kind() == "identifier" {
                                    if let Ok(name) = obj.utf8_text(state.source.as_bytes()) {
                                        return Some(name.to_string());
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Byte-scan fallback: the char immediately before the cursor is `.`,
    // preceded by an identifier.
    alias_before_dot(&state.source, position)
}

/// Find an identifier immediately followed by `.` ending at `position`.
fn alias_before_dot(source: &str, position: Position) -> Option<String> {
    let offset = position_to_offset(source, position)?;
    let bytes = source.as_bytes();
    if offset == 0 {
        return None;
    }
    // Skip back over an already-typed partial member identifier.
    let mut i = offset;
    while i > 0 && is_ident_byte(bytes[i - 1]) {
        i -= 1;
    }
    // Now expect a `.`.
    if i == 0 || bytes[i - 1] != b'.' {
        return None;
    }
    let dot = i - 1;
    // Collect the identifier preceding the dot.
    let mut start = dot;
    while start > 0 && is_ident_byte(bytes[start - 1]) {
        start -= 1;
    }
    if start == dot {
        return None;
    }
    Some(source[start..dot].to_string())
}

fn is_ident_byte(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric()
}

fn position_to_offset(source: &str, position: Position) -> Option<usize> {
    let mut line = 0u32;
    let mut col = 0u32;
    for (idx, ch) in source.char_indices() {
        if line == position.line && col == position.character {
            return Some(idx);
        }
        if ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += ch.len_utf16() as u32;
        }
    }
    if line == position.line && col == position.character {
        return Some(source.len());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_alias_before_dot() {
        let src = "util := import(\":util\")\nx := util.\n";
        // line 1, after `util.` -> char 10
        let pos = Position::new(1, 10);
        assert_eq!(alias_before_dot(src, pos).as_deref(), Some("util"));
    }

    #[test]
    fn detects_alias_with_partial_member() {
        let src = "util := import(\":util\")\nx := util.create\n";
        // cursor at end of `create` -> char 16
        let pos = Position::new(1, 16);
        assert_eq!(alias_before_dot(src, pos).as_deref(), Some("util"));
    }

    #[test]
    fn no_dot_is_none() {
        let src = "util := import(\":util\")\nx := util\n";
        assert_eq!(alias_before_dot(src, Position::new(1, 9)), None);
    }

    #[test]
    fn missing_module_returns_none_no_panic() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_tengo::LANGUAGE.into())
            .unwrap();
        let src = "util := import(\":nonexistent\")\nx := util.\n";
        let state =
            FileState::parse("file:///nowhere/main.lib.tengo".into(), src.into(), &mut parser)
                .unwrap();
        assert!(completion(&state, Position::new(1, 10)).is_none());
    }
}

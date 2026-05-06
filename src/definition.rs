use tower_lsp::lsp_types::{GotoDefinitionResponse, Location, Position, Range, Url};

use crate::document::FileState;
use crate::resolver;

/// Handle textDocument/definition.
pub fn goto_definition(
    state: &FileState,
    position: Position,
) -> Option<GotoDefinitionResponse> {
    let point = tree_sitter::Point {
        row: position.line as usize,
        column: position.character as usize,
    };

    let root = state.tree.root_node();
    let node = root.descendant_for_point_range(point, point)?;

    if node.kind() != "identifier" {
        // Maybe cursor is on a string inside import("...")
        if node.kind() == "string_literal" {
            return handle_import_string(node, state);
        }
        return None;
    }

    let name = node.utf8_text(state.source.as_bytes()).ok()?;
    let parent = node.parent()?;

    // Case 1: selector_expression field — pkg.Something
    if parent.kind() == "selector_expression" {
        let field_node = parent.child_by_field_name("field")?;
        if field_node.id() == node.id() {
            // This is the .field part — check if object is an import
            let object_node = parent.child_by_field_name("object")?;
            if object_node.kind() == "identifier" {
                let obj_name = object_node.utf8_text(state.source.as_bytes()).ok()?;
                if let Some(import_info) = state.imports.get(obj_name) {
                    return jump_to_import_file(import_info, state);
                }
            }
            return None;
        }
    }

    // Case 2: any identifier (including import vars) — find its definition in scope chain.
    // For `pkg` in `pkg.field`, this jumps to the `pkg := import(...)` statement.
    let def = state.resolve_def(name, node.start_byte())?;
    let uri = Url::parse(&state.uri).ok()?;
    Some(GotoDefinitionResponse::Scalar(Location {
        uri,
        range: def.range,
    }))
}

fn handle_import_string(
    node: tree_sitter::Node,
    state: &FileState,
) -> Option<GotoDefinitionResponse> {
    let parent = node.parent()?;
    if parent.kind() != "import_expression" {
        return None;
    }
    let text = node.utf8_text(state.source.as_bytes()).ok()?;
    let module_path = text.trim_matches('"');
    let current_file = resolver::uri_to_path(&state.uri)?;
    let resolved = resolver::resolve_import(module_path, &current_file)?;
    let uri = Url::from_file_path(&resolved).ok()?;
    Some(GotoDefinitionResponse::Scalar(Location {
        uri,
        range: Range::new(Position::new(0, 0), Position::new(0, 0)),
    }))
}

fn jump_to_import_file(
    import_info: &crate::symbols::ImportInfo,
    state: &FileState,
) -> Option<GotoDefinitionResponse> {
    let current_file = resolver::uri_to_path(&state.uri)?;
    let resolved = resolver::resolve_import(&import_info.module_path, &current_file)?;
    let uri = Url::from_file_path(&resolved).ok()?;
    Some(GotoDefinitionResponse::Scalar(Location {
        uri,
        range: Range::new(Position::new(0, 0), Position::new(0, 0)),
    }))
}


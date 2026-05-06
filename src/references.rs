use tower_lsp::lsp_types::{Location, Position, Url};

use crate::document::FileState;

/// Handle textDocument/references.
pub fn find_references(
    state: &FileState,
    position: Position,
    include_declaration: bool,
) -> Option<Vec<Location>> {
    let point = tree_sitter::Point {
        row: position.line as usize,
        column: position.character as usize,
    };

    let root = state.tree.root_node();
    let node = root.descendant_for_point_range(point, point)?;

    if node.kind() != "identifier" {
        return None;
    }

    let name = node.utf8_text(state.source.as_bytes()).ok()?;
    let uri = Url::parse(&state.uri).ok()?;

    // Find the definition to determine the scope
    let def = state.resolve_def(name, node.start_byte())?;
    let def_scope = &state.scopes[def.scope_id];

    let mut locations = Vec::new();

    // Optionally include the declaration itself
    if include_declaration {
        locations.push(Location {
            uri: uri.clone(),
            range: def.range,
        });
    }

    // Collect all references with matching name that are within the definition's scope
    for r in &state.refs {
        if r.name == name
            && r.byte_range.start >= def_scope.start_byte
            && r.byte_range.start < def_scope.end_byte
            && r.byte_range.start >= def.byte_range.start
        {
            locations.push(Location {
                uri: uri.clone(),
                range: r.range,
            });
        }
    }

    if locations.is_empty() {
        None
    } else {
        Some(locations)
    }
}

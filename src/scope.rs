use tree_sitter::Tree;

#[derive(Debug, Clone)]
pub struct Scope {
    pub id: usize,
    pub parent: Option<usize>,
    pub start_byte: usize,
    pub end_byte: usize,
}

/// Build a flat list of scopes from the tree-sitter parse tree.
/// Scope-creating nodes: source_file, block, func_literal.
pub fn build_scopes(tree: &Tree, scopes: &mut Vec<Scope>) {
    scopes.clear();
    let root = tree.root_node();
    walk_scopes(root, None, scopes);
}

fn walk_scopes(node: tree_sitter::Node, parent: Option<usize>, scopes: &mut Vec<Scope>) {
    let creates_scope = matches!(
        node.kind(),
        "source_file" | "block" | "func_literal"
    );

    let current_scope = if creates_scope {
        let id = scopes.len();
        scopes.push(Scope {
            id,
            parent,
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
        });
        Some(id)
    } else {
        parent
    };

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_scopes(child, current_scope, scopes);
    }
}

use std::collections::HashMap;
use std::ops::Range;

use tree_sitter::Tree;

use crate::scope::Scope;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Var,
    Func,
    Import,
    Param,
    ForVar,
}

#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub range: tower_lsp::lsp_types::Range,
    pub byte_range: Range<usize>,
    pub scope_id: usize,
}

#[derive(Debug, Clone)]
pub struct SymbolRef {
    pub name: String,
    pub range: tower_lsp::lsp_types::Range,
    pub byte_range: Range<usize>,
}

#[derive(Debug, Clone)]
pub struct ImportInfo {
    pub module_path: String,
    pub var_name: String,
}

/// Extract all definitions and references from the parse tree.
pub fn extract_symbols(
    tree: &Tree,
    source: &[u8],
    scopes: &[Scope],
    defs: &mut Vec<Symbol>,
    refs: &mut Vec<SymbolRef>,
    imports: &mut HashMap<String, ImportInfo>,
) {
    let root = tree.root_node();
    walk_node(root, source, scopes, defs, refs, imports);
}

fn walk_node(
    node: tree_sitter::Node,
    source: &[u8],
    scopes: &[Scope],
    defs: &mut Vec<Symbol>,
    refs: &mut Vec<SymbolRef>,
    imports: &mut HashMap<String, ImportInfo>,
) {
    match node.kind() {
        "var_declaration" => {
            handle_var_declaration(node, source, scopes, defs, imports);
        }
        "assignment_statement" => {
            handle_assignment(node, source, scopes, defs, imports);
        }
        "parameter_list" => {
            handle_parameters(node, source, scopes, defs);
        }
        "variadic_parameter" => {
            handle_variadic_param(node, source, scopes, defs);
        }
        "for_in_clause" => {
            handle_for_in(node, source, scopes, defs);
        }
        "identifier" => {
            // Only record as reference if not already captured as a definition.
            // We check by seeing if the parent already handled this node.
            if !is_definition_position(node) {
                let name = node_text(node, source);
                refs.push(SymbolRef {
                    name,
                    range: node_to_lsp_range(node),
                    byte_range: node.start_byte()..node.end_byte(),
                });
            }
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_node(child, source, scopes, defs, refs, imports);
    }
}

/// Check if this identifier is in a position already handled as a definition.
fn is_definition_position(node: tree_sitter::Node) -> bool {
    if let Some(parent) = node.parent() {
        match parent.kind() {
            "var_declaration" => {
                // name field
                parent
                    .child_by_field_name("name")
                    .map_or(false, |n| n.id() == node.id())
            }
            "assignment_statement" => {
                // left field with := operator
                if let Some(op) = parent.child_by_field_name("operator") {
                    let op_text = op.kind();
                    if op_text == ":=" {
                        return parent
                            .child_by_field_name("left")
                            .map_or(false, |n| n.id() == node.id());
                    }
                }
                false
            }
            "parameter_list" => true,
            "variadic_parameter" => true,
            "for_in_clause" => {
                let is_key = parent
                    .child_by_field_name("key")
                    .map_or(false, |n| n.id() == node.id());
                let is_val = parent
                    .child_by_field_name("value")
                    .map_or(false, |n| n.id() == node.id());
                is_key || is_val
            }
            _ => false,
        }
    } else {
        false
    }
}

fn handle_var_declaration(
    node: tree_sitter::Node,
    source: &[u8],
    scopes: &[Scope],
    defs: &mut Vec<Symbol>,
    imports: &mut HashMap<String, ImportInfo>,
) {
    let name_node = match node.child_by_field_name("name") {
        Some(n) => n,
        None => return,
    };
    let value_node = node.child_by_field_name("value");
    let name = node_text(name_node, source);

    let (kind, import_info) = classify_value(value_node, source, &name);
    let scope_id = find_scope(scopes, name_node.start_byte());

    if let Some(info) = &import_info {
        imports.insert(name.clone(), info.clone());
    }

    defs.push(Symbol {
        name,
        kind,
        range: node_to_lsp_range(name_node),
        byte_range: name_node.start_byte()..name_node.end_byte(),
        scope_id,
    });
}

fn handle_assignment(
    node: tree_sitter::Node,
    source: &[u8],
    scopes: &[Scope],
    defs: &mut Vec<Symbol>,
    imports: &mut HashMap<String, ImportInfo>,
) {
    let op_node = match node.child_by_field_name("operator") {
        Some(n) => n,
        None => return,
    };
    // Only := creates a new definition
    if op_node.kind() != ":=" {
        return;
    }
    let left_node = match node.child_by_field_name("left") {
        Some(n) if n.kind() == "identifier" => n,
        _ => return,
    };
    let right_node = node.child_by_field_name("right");
    let name = node_text(left_node, source);

    let (kind, import_info) = classify_value(right_node, source, &name);
    let scope_id = find_scope(scopes, left_node.start_byte());

    if let Some(info) = &import_info {
        imports.insert(name.clone(), info.clone());
    }

    defs.push(Symbol {
        name,
        kind,
        range: node_to_lsp_range(left_node),
        byte_range: left_node.start_byte()..left_node.end_byte(),
        scope_id,
    });
}

fn handle_parameters(
    node: tree_sitter::Node,
    source: &[u8],
    scopes: &[Scope],
    defs: &mut Vec<Symbol>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "identifier" {
            let name = node_text(child, source);
            let scope_id = find_scope(scopes, child.start_byte());
            defs.push(Symbol {
                name,
                kind: SymbolKind::Param,
                range: node_to_lsp_range(child),
                byte_range: child.start_byte()..child.end_byte(),
                scope_id,
            });
        }
    }
}

fn handle_variadic_param(
    node: tree_sitter::Node,
    source: &[u8],
    scopes: &[Scope],
    defs: &mut Vec<Symbol>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "identifier" {
            let name = node_text(child, source);
            let scope_id = find_scope(scopes, child.start_byte());
            defs.push(Symbol {
                name,
                kind: SymbolKind::Param,
                range: node_to_lsp_range(child),
                byte_range: child.start_byte()..child.end_byte(),
                scope_id,
            });
        }
    }
}

fn handle_for_in(
    node: tree_sitter::Node,
    source: &[u8],
    scopes: &[Scope],
    defs: &mut Vec<Symbol>,
) {
    if let Some(key) = node.child_by_field_name("key") {
        let name = node_text(key, source);
        // for-in vars belong to the for_statement's block scope.
        // The block comes after the for_in_clause, so find scope at the
        // sibling block's start. Fall back to clause's own scope.
        let scope_id = find_for_body_scope(node, scopes);
        defs.push(Symbol {
            name,
            kind: SymbolKind::ForVar,
            range: node_to_lsp_range(key),
            byte_range: key.start_byte()..key.end_byte(),
            scope_id,
        });
    }
    if let Some(val) = node.child_by_field_name("value") {
        let name = node_text(val, source);
        let scope_id = find_for_body_scope(node, scopes);
        defs.push(Symbol {
            name,
            kind: SymbolKind::ForVar,
            range: node_to_lsp_range(val),
            byte_range: val.start_byte()..val.end_byte(),
            scope_id,
        });
    }
}

fn find_for_body_scope(for_in_node: tree_sitter::Node, scopes: &[Scope]) -> usize {
    // The for_in_clause's parent is for_statement, which contains a block.
    if let Some(for_stmt) = for_in_node.parent() {
        let mut cursor = for_stmt.walk();
        for child in for_stmt.children(&mut cursor) {
            if child.kind() == "block" {
                return find_scope(scopes, child.start_byte());
            }
        }
    }
    find_scope(scopes, for_in_node.start_byte())
}

fn classify_value(
    value_node: Option<tree_sitter::Node>,
    source: &[u8],
    var_name: &str,
) -> (SymbolKind, Option<ImportInfo>) {
    match value_node {
        Some(n) if n.kind() == "func_literal" => (SymbolKind::Func, None),
        Some(n) if n.kind() == "import_expression" => {
            let module_path = extract_import_path(n, source);
            let info = module_path.map(|p| ImportInfo {
                module_path: p,
                var_name: var_name.to_string(),
            });
            (SymbolKind::Import, info)
        }
        _ => (SymbolKind::Var, None),
    }
}

fn extract_import_path(import_node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    // import_expression children: "import" "(" expression ")"
    // The expression is typically a string_literal
    let mut cursor = import_node.walk();
    for child in import_node.children(&mut cursor) {
        if child.kind() == "string_literal" {
            let text = node_text(child, source);
            // Strip quotes
            let trimmed = text.trim_matches('"');
            return Some(trimmed.to_string());
        }
    }
    None
}

fn find_scope(scopes: &[Scope], byte: usize) -> usize {
    let mut best = 0usize;
    let mut best_size = usize::MAX;
    for scope in scopes {
        if byte >= scope.start_byte && byte < scope.end_byte {
            let size = scope.end_byte - scope.start_byte;
            if size < best_size {
                best = scope.id;
                best_size = size;
            }
        }
    }
    best
}

fn node_text(node: tree_sitter::Node, source: &[u8]) -> String {
    node.utf8_text(source).unwrap_or("").to_string()
}

pub fn node_to_lsp_range(node: tree_sitter::Node) -> tower_lsp::lsp_types::Range {
    let start = node.start_position();
    let end = node.end_position();
    tower_lsp::lsp_types::Range {
        start: tower_lsp::lsp_types::Position {
            line: start.row as u32,
            character: start.column as u32,
        },
        end: tower_lsp::lsp_types::Position {
            line: end.row as u32,
            character: end.column as u32,
        },
    }
}

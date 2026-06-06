use tower_lsp::lsp_types::{Position, Range};

/// Kind of an exported member.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Function,
    Variable,
}

/// A single member exported from a Tengo library's `export { ... }` map.
#[derive(Debug, Clone)]
pub struct ExportMember {
    pub name: String,
    /// Range of the export key; retained for callers that navigate to it.
    #[allow(dead_code)]
    pub key_range: Range,
    pub kind: ItemKind,
    /// Rendered signature for functions, e.g. `createFileDataset(a, b, ...rest)`.
    pub signature: Option<String>,
    /// Doc comment (consecutive `//` lines), markers stripped, joined with `\n`.
    pub doc: Option<String>,
}

/// Parse `source` and return every member of the top-level `export { ... }` map.
pub fn parse_exports(source: &str) -> Vec<ExportMember> {
    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter_tengo::LANGUAGE.into())
        .is_err()
    {
        return Vec::new();
    }
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return Vec::new(),
    };
    let root = tree.root_node();
    let export_map = match find_export_map(root) {
        Some(m) => m,
        None => return Vec::new(),
    };

    let mut members = Vec::new();
    let mut cursor = export_map.walk();
    for entry in export_map.children(&mut cursor) {
        if entry.kind() != "map_entry" {
            continue;
        }
        let key = match entry.child_by_field_name("key") {
            Some(k) => k,
            None => continue,
        };
        let name = match key.utf8_text(source.as_bytes()) {
            Ok(t) => t.trim_matches('"').to_string(),
            Err(_) => continue,
        };
        let value = entry.child_by_field_name("value");

        // Resolve where the value's func_literal (if any) lives, and where the
        // doc comment should be looked up.
        let (kind, signature, doc_anchor) = resolve_value(root, value, source);

        let doc = doc_anchor.and_then(|n| extract_doc(n, source));

        members.push(ExportMember {
            name,
            key_range: node_to_range(key),
            kind,
            signature,
            doc,
        });
    }
    members
}

/// Resolve an entry's value to a kind/signature, and pick the node whose
/// preceding comments are the doc: the inline `func_literal`'s statement
/// (the map_entry) or the top-level `name := func...` declaration.
fn resolve_value<'a>(
    root: tree_sitter::Node<'a>,
    value: Option<tree_sitter::Node<'a>>,
    source: &str,
) -> (ItemKind, Option<String>, Option<tree_sitter::Node<'a>>) {
    let value = match value {
        Some(v) => v,
        None => return (ItemKind::Variable, None, None),
    };

    // Inline func literal: `name: func(...) { ... }`.
    if value.kind() == "func_literal" {
        let sig = render_signature(value, source);
        // Doc anchor is the enclosing map_entry.
        let anchor = value.parent();
        return (ItemKind::Function, sig, anchor);
    }

    // `name: ident` — look up the top-level `ident := func(...)` binding.
    if value.kind() == "identifier" {
        let ident = value.utf8_text(source.as_bytes()).unwrap_or("");
        if let Some((decl, func)) = find_top_level_func_binding(root, ident, source) {
            let sig = render_signature(func, source);
            return (ItemKind::Function, sig, Some(decl));
        }
    }

    (ItemKind::Variable, None, None)
}

/// Find a top-level `name := func(...)` (or `name := func` via var_declaration)
/// binding. Returns (declaration_node, func_literal_node).
fn find_top_level_func_binding<'a>(
    root: tree_sitter::Node<'a>,
    name: &str,
    source: &str,
) -> Option<(tree_sitter::Node<'a>, tree_sitter::Node<'a>)> {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        let (decl_name, value) = match child.kind() {
            "var_declaration" => (
                child.child_by_field_name("name"),
                child.child_by_field_name("value"),
            ),
            "assignment_statement" => {
                let op = child.child_by_field_name("operator");
                if op.map(|o| o.kind()) != Some(":=") {
                    continue;
                }
                (
                    child.child_by_field_name("left"),
                    child.child_by_field_name("right"),
                )
            }
            _ => continue,
        };
        let decl_name = match decl_name {
            Some(n) if n.kind() == "identifier" => n,
            _ => continue,
        };
        if decl_name.utf8_text(source.as_bytes()).unwrap_or("") != name {
            continue;
        }
        if let Some(v) = value {
            if v.kind() == "func_literal" {
                return Some((child, v));
            }
        }
    }
    None
}

/// Render a `func_literal`'s parameter list as `name(p1, p2, ...rest)`.
fn render_signature(func: tree_sitter::Node, source: &str) -> Option<String> {
    let mut params: Vec<String> = Vec::new();
    let mut cursor = func.walk();
    for child in func.children(&mut cursor) {
        if child.kind() == "parameter_list" {
            let mut pc = child.walk();
            for p in child.named_children(&mut pc) {
                match p.kind() {
                    "identifier" => {
                        params.push(p.utf8_text(source.as_bytes()).unwrap_or("").to_string());
                    }
                    "variadic_parameter" => {
                        let ident = p
                            .named_child(0)
                            .and_then(|i| i.utf8_text(source.as_bytes()).ok())
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

/// Extract the doc comment immediately preceding `node`. Handles both a
/// JSDoc-style `/** ... */` block (the dominant style in the SDK) and a run of
/// consecutive `//` line comments. Returns None if there is no adjacent comment.
fn extract_doc(node: tree_sitter::Node, source: &str) -> Option<String> {
    let prev = node.prev_sibling()?;
    if prev.kind() != "comment" {
        return None;
    }
    // The comment must sit directly above the node (no blank line between).
    if prev.end_position().row + 1 != node.start_position().row {
        return None;
    }
    let text = prev.utf8_text(source.as_bytes()).unwrap_or("");

    if text.starts_with("/*") {
        return parse_block_doc(text);
    }
    if text.starts_with("//") {
        return extract_line_doc(node, source);
    }
    None
}

/// Parse a `/* ... */` (or `/** ... */`) block comment into doc text: strip the
/// delimiters and any per-line leading `*`, then trim blank edge lines.
fn parse_block_doc(text: &str) -> Option<String> {
    let inner = text
        .strip_prefix("/**")
        .or_else(|| text.strip_prefix("/*"))
        .unwrap_or(text)
        .strip_suffix("*/")
        .unwrap_or(text);

    let mut lines: Vec<String> = inner
        .lines()
        .map(|l| l.trim().trim_start_matches('*').trim().to_string())
        .collect();

    while lines.first().is_some_and(|l| l.is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }

    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

/// Collect consecutive `//` line comments immediately preceding `node`.
fn extract_line_doc(node: tree_sitter::Node, source: &str) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut current = node;
    let mut next_start_row = node.start_position().row;

    while let Some(prev) = current.prev_sibling() {
        if prev.kind() != "comment" {
            break;
        }
        let text = prev.utf8_text(source.as_bytes()).unwrap_or("");
        if !text.starts_with("//") {
            break;
        }
        if prev.end_position().row + 1 != next_start_row {
            break;
        }
        let stripped = text.trim_start_matches('/').trim_start().to_string();
        lines.push(stripped);
        next_start_row = prev.start_position().row;
        current = prev;
    }

    if lines.is_empty() {
        None
    } else {
        lines.reverse();
        Some(lines.join("\n"))
    }
}

/// Find the `map_literal` of the top-level `export` statement. Handles both a
/// bare `export { ... }` and a wrapped form like `export ll.toStrict({ ... })`
/// by searching the export subtree for the first `map_literal`.
pub fn find_export_map(root: tree_sitter::Node) -> Option<tree_sitter::Node> {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "export_statement" {
            return first_map_literal(child);
        }
    }
    None
}

/// Depth-first search for the first `map_literal` within `node`.
fn first_map_literal(node: tree_sitter::Node) -> Option<tree_sitter::Node> {
    if node.kind() == "map_literal" {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(found) = first_map_literal(child) {
            return Some(found);
        }
    }
    None
}

/// Parse `source` and return the LSP range of the `member` key inside the
/// `export { ... }` map literal, if present.
pub fn find_export_member_range(source: &str, member: &str) -> Option<Range> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_tengo::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(source, None)?;
    let export_map = find_export_map(tree.root_node())?;

    let mut cursor = export_map.walk();
    for entry in export_map.children(&mut cursor) {
        if entry.kind() != "map_entry" {
            continue;
        }
        let key = match entry.child_by_field_name("key") {
            Some(k) => k,
            None => continue,
        };
        let key_text = key.utf8_text(source.as_bytes()).ok()?.trim_matches('"');
        if key_text == member {
            return Some(node_to_range(key));
        }
    }
    None
}

/// True when the top-level export entry for `member` is the `member: member`
/// shorthand — i.e. its value is an identifier equal to the key. Only such a
/// member is backed by a top-level definition that should be renamed alongside
/// the key. Any other shape (`member: other.thing`, a literal, an inline func)
/// must NOT drag a same-named top-level binding into the rename, since that
/// binding is an unrelated private symbol.
pub fn export_member_is_shorthand(source: &str, member: &str) -> bool {
    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter_tengo::LANGUAGE.into())
        .is_err()
    {
        return false;
    }
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return false,
    };
    let export_map = match find_export_map(tree.root_node()) {
        Some(m) => m,
        None => return false,
    };
    let mut cursor = export_map.walk();
    for entry in export_map.children(&mut cursor) {
        if entry.kind() != "map_entry" {
            continue;
        }
        let key = match entry.child_by_field_name("key") {
            Some(k) => k,
            None => continue,
        };
        if key.utf8_text(source.as_bytes()).unwrap_or("").trim_matches('"') != member {
            continue;
        }
        return match entry.child_by_field_name("value") {
            Some(v) if v.kind() == "identifier" => {
                v.utf8_text(source.as_bytes()).unwrap_or("") == member
            }
            _ => false,
        };
    }
    false
}

pub fn node_to_range(node: tree_sitter::Node) -> Range {
    let s = node.start_position();
    let e = node.end_position();
    Range::new(
        Position::new(s.row as u32, s.column as u32),
        Position::new(e.row as u32, e.column as u32),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIB: &str = r#"util := import(":util")

// Builds a file dataset from the given args.
// Second doc line.
createFileDataset := func(blockId, sampleIdAxis, dataset, ...rest) {
    return blockId
}

isGrouped := false

export {
    inlineFn: func(a, b) {
        return a
    },
    createFileDataset: createFileDataset,
    isGrouped: isGrouped,
    literalVar: 42
}
"#;

    fn member<'a>(ms: &'a [ExportMember], name: &str) -> &'a ExportMember {
        ms.iter().find(|m| m.name == name).expect("member present")
    }

    #[test]
    fn extracts_all_members_with_kinds() {
        let ms = parse_exports(LIB);
        let names: Vec<&str> = ms.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["inlineFn", "createFileDataset", "isGrouped", "literalVar"]
        );

        assert_eq!(member(&ms, "inlineFn").kind, ItemKind::Function);
        assert_eq!(member(&ms, "createFileDataset").kind, ItemKind::Function);
        // `isGrouped: isGrouped` where isGrouped := false -> Variable.
        assert_eq!(member(&ms, "isGrouped").kind, ItemKind::Variable);
        assert_eq!(member(&ms, "literalVar").kind, ItemKind::Variable);
    }

    #[test]
    fn renders_signature_with_variadic() {
        let ms = parse_exports(LIB);
        assert_eq!(
            member(&ms, "createFileDataset").signature.as_deref(),
            Some("(blockId, sampleIdAxis, dataset, ...rest)")
        );
        assert_eq!(member(&ms, "inlineFn").signature.as_deref(), Some("(a, b)"));
    }

    #[test]
    fn extracts_doc_comment_for_referenced_func() {
        let ms = parse_exports(LIB);
        // This is the safety-net assertion for the prev_sibling comment walk.
        assert_eq!(
            member(&ms, "createFileDataset").doc.as_deref(),
            Some("Builds a file dataset from the given args.\nSecond doc line.")
        );
    }

    #[test]
    fn no_doc_is_none() {
        let ms = parse_exports(LIB);
        assert_eq!(member(&ms, "inlineFn").doc, None);
        assert_eq!(member(&ms, "literalVar").doc, None);
    }

    const BLOCK_DOC_LIB: &str = r#"/**
 * Returns true if map contains specified key.
 * Note: O(N) fallback.
 */
containsKey := func(map, key) {
    return true
}

export {
    containsKey: containsKey
}
"#;

    #[test]
    fn extracts_jsdoc_block_comment() {
        let ms = parse_exports(BLOCK_DOC_LIB);
        assert_eq!(
            member(&ms, "containsKey").doc.as_deref(),
            Some("Returns true if map contains specified key.\nNote: O(N) fallback.")
        );
    }

    // `export ll.toStrict({ ... })` — the map is wrapped in a call expression.
    const WRAPPED_EXPORT_LIB: &str = r#"ll := import(":ll")

/**
 * Builder for quota resource.
 */
quotaBuilder := func() {
    return undefined
}

export ll.toStrict({
    quotaBuilder: quotaBuilder
})
"#;

    #[test]
    fn resolves_member_and_doc_through_wrapped_export() {
        let ms = parse_exports(WRAPPED_EXPORT_LIB);
        let m = member(&ms, "quotaBuilder");
        assert_eq!(m.kind, ItemKind::Function);
        assert_eq!(m.signature.as_deref(), Some("()"));
        // Doc resolves from the top-level `quotaBuilder := func()` declaration.
        assert_eq!(m.doc.as_deref(), Some("Builder for quota resource."));
    }

    #[test]
    fn no_export_block_yields_empty() {
        assert!(parse_exports("x := 1\n").is_empty());
    }

    #[test]
    fn find_export_member_range_works() {
        let r = find_export_member_range(LIB, "createFileDataset").unwrap();
        assert!(find_export_member_range(LIB, "nope").is_none());
        // key sits on the line `    createFileDataset: createFileDataset,`
        let want_line = LIB
            .lines()
            .position(|l| l.trim_start().starts_with("createFileDataset:"))
            .unwrap() as u32;
        assert_eq!(r.start.line, want_line);
    }
}

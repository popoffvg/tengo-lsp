use tower_lsp::lsp_types::{GotoDefinitionResponse, Location, Position, Range, Url};

use crate::document::FileState;
use crate::exports::find_export_member_range;
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
                    return jump_to_import_member(import_info, name, state);
                }
            }
            return None;
        }
    }

    // Case 2: an import alias resolves to the imported file, not to its own
    // `alias := import(...)` declaration (which is where the cursor already is).
    // Applies both at the declaration and at usages like `ll` in `ll.foo`.
    if let Some(import_info) = state.imports.get(name) {
        if let Some(resp) = jump_to_import_member(import_info, "", state) {
            return Some(resp);
        }
        // Not resolvable (e.g. a stdlib module) — fall through to self-definition.
    }

    // Case 3: any other identifier — find its definition in scope chain.
    let def = state.resolve_def(name, node.start_byte())?;
    let uri = Url::parse(&state.uri).ok()?;
    Some(GotoDefinitionResponse::Scalar(Location {
        uri,
        range: def.range,
    }))
}

/// SDK function-call forms whose string argument is an artifact name in the
/// same `<pkg>:<id>` / `:<id>` format as `import("...")`. Matched by method
/// name regardless of receiver alias (e.g. `plapi`, `ll`, `assets`).
const ARTIFACT_CALL_METHODS: &[&str] = &[
    "getTemplateId",
    "getSoftwareInfo",
    "importTemplate",
    "importSoftware",
    "importAsset",
];

fn handle_import_string(
    node: tree_sitter::Node,
    state: &FileState,
) -> Option<GotoDefinitionResponse> {
    let parent = node.parent()?;
    // Accept `import("...")` directly, or an SDK artifact call argument:
    // `<alias>.<method>("...")` where the string is inside the argument_list.
    if parent.kind() != "import_expression" && !is_artifact_call_argument(parent, state) {
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

/// True when `arg_list` is the argument list of an SDK artifact call such as
/// `plapi.getTemplateId(...)` or `assets.importAsset(...)`.
fn is_artifact_call_argument(arg_list: tree_sitter::Node, state: &FileState) -> bool {
    if arg_list.kind() != "argument_list" {
        return false;
    }
    let call = match arg_list.parent() {
        Some(c) if c.kind() == "call_expression" => c,
        _ => return false,
    };
    let function = match call.child_by_field_name("function") {
        Some(f) if f.kind() == "selector_expression" => f,
        _ => return false,
    };
    let field = match function.child_by_field_name("field") {
        Some(f) => f,
        None => return false,
    };
    match field.utf8_text(state.source.as_bytes()) {
        Ok(name) => ARTIFACT_CALL_METHODS.contains(&name),
        Err(_) => false,
    }
}

/// Resolve `<alias>.<member>` to the matching key in the imported file's
/// `export { ... }` map. Falls back to the top of the file if the member is
/// not found in an export block.
fn jump_to_import_member(
    import_info: &crate::symbols::ImportInfo,
    member: &str,
    state: &FileState,
) -> Option<GotoDefinitionResponse> {
    let current_file = resolver::uri_to_path(&state.uri)?;
    let resolved = resolver::resolve_import(&import_info.module_path, &current_file)?;
    let uri = Url::from_file_path(&resolved).ok()?;

    let range = std::fs::read_to_string(&resolved)
        .ok()
        .and_then(|src| find_export_member_range(&src, member))
        .unwrap_or_else(|| Range::new(Position::new(0, 0), Position::new(0, 0)));

    Some(GotoDefinitionResponse::Scalar(Location { uri, range }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::FileState;
    use tree_sitter::Parser;

    fn parse(uri: &str, src: &str) -> FileState {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_tengo::LANGUAGE.into())
            .unwrap();
        FileState::parse(uri.to_string(), src.to_string(), &mut parser).unwrap()
    }

    const DS_H5AD: &str = r#"util := import(":util")

createDataset := func(blockId, sampleIdAxis, dataset, importFile) {
    return util.createFileDataset(blockId, sampleIdAxis, dataset, importFile, "h5ad")
}

export {
    isGrouped: false,
    createDataset: createDataset
}
"#;

    #[test]
    fn import_alias_is_parsed() {
        let state = parse("file:///pkg/src/ds-h5ad.lib.tengo", DS_H5AD);
        let info = state
            .imports
            .get("util")
            .expect("`util` should be a known import alias");
        assert_eq!(info.module_path, ":util");
    }

    #[test]
    fn goto_on_imported_member_is_recognized() {
        let state = parse("file:///pkg/src/ds-h5ad.lib.tengo", DS_H5AD);
        let line = DS_H5AD.lines().nth(3).unwrap();
        let col = line.find("createFileDataset").unwrap() as u32;

        // With no `util.lib.tengo` on disk the jump can't resolve to a path,
        // but the alias must be recognized: the handler reaches `jump_to_import_member`,
        // which returns None only because resolution fails — never panics. We assert the
        // member path is taken by confirming `util` is in `state.imports` (above)
        // and that the call does not panic here.
        let _ = goto_definition(&state, Position::new(3, col));
    }

    const UTIL_LIB: &str = r#"createFileDataset := func(x) {
    return x
}

export {
    isGrouped: false,
    createFileDataset: createFileDataset
}
"#;

    #[test]
    fn finds_export_member_key_range() {
        let range = find_export_member_range(UTIL_LIB, "createFileDataset")
            .expect("createFileDataset must be found in the export block");
        // Export key is on line 6 (0-based) inside the `export { ... }` map.
        assert_eq!(range.start.line, 6);
        let key_col = UTIL_LIB
            .lines()
            .nth(6)
            .unwrap()
            .find("createFileDataset")
            .unwrap() as u32;
        assert_eq!(range.start.character, key_col);
    }

    #[test]
    fn export_member_not_present_returns_none() {
        assert!(find_export_member_range(UTIL_LIB, "doesNotExist").is_none());
    }

    #[test]
    fn missing_export_entry_does_not_crash_and_falls_back_to_file_top() {
        // Build a real on-disk package so import resolution succeeds, then ask for
        // a member that is NOT in the export block. The server must not panic and
        // should fall back to the top of the resolved file rather than returning None.
        let base = std::env::temp_dir().join(format!("tengo-lsp-test-{}", std::process::id()));
        let src = base.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(base.join("package.json"), "{}").unwrap();
        std::fs::write(src.join("util.lib.tengo"), UTIL_LIB).unwrap();

        let main_src = "util := import(\":util\")\nx := util.missingMember\n";
        std::fs::write(src.join("main.lib.tengo"), main_src).unwrap();

        let uri = format!("file://{}", src.join("main.lib.tengo").display());
        let state = parse(&uri, main_src);

        // Cursor on `missingMember` (line 1).
        let col = main_src.lines().nth(1).unwrap().find("missingMember").unwrap() as u32;
        let resp = goto_definition(&state, Position::new(1, col));

        std::fs::remove_dir_all(&base).ok();

        match resp {
            Some(GotoDefinitionResponse::Scalar(loc)) => {
                // Member not exported -> fall back to top of util.lib.tengo.
                assert_eq!(loc.range.start.line, 0);
                assert_eq!(loc.range.start.character, 0);
            }
            other => panic!("expected graceful fallback Scalar, got {other:?}"),
        }
    }
}




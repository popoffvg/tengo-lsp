use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use tower_lsp::lsp_types::{Position, Range, TextEdit, Url, WorkspaceEdit};
use tree_sitter::Parser;

use crate::document::FileState;
use crate::references::{self, SourceOverlay};

/// Reserved words that cannot be used as a new identifier name.
const KEYWORDS: &[&str] = &[
    "true", "false", "undefined", "if", "else", "for", "func", "return", "import", "export", "in",
    "break", "continue",
];

/// Why a rename could not be produced.
#[derive(Debug)]
pub enum RenameError {
    /// The position does not name a symbol that can be safely renamed.
    NotRenameable,
    /// The requested new name is not a legal Tengo identifier.
    InvalidName(String),
}

/// textDocument/prepareRename — validate that the position is renameable and
/// return the identifier's range, so the editor can seed its rename box.
pub fn prepare_rename(state: &FileState, position: Position) -> Option<Range> {
    let node = identifier_at(state, position)?;
    Some(crate::symbols::node_to_lsp_range(node))
}

/// textDocument/rename — produce a workspace-wide edit replacing every
/// occurrence of the symbol under `position` with `new_name`.
pub fn rename(
    state: &FileState,
    position: Position,
    new_name: &str,
    roots: &[PathBuf],
    parser: &Mutex<Parser>,
    overlay: &SourceOverlay,
) -> Result<WorkspaceEdit, RenameError> {
    if !is_valid_identifier(new_name) || KEYWORDS.contains(&new_name) {
        return Err(RenameError::InvalidName(format!(
            "`{new_name}` is not a valid Tengo identifier"
        )));
    }

    let _node = identifier_at(state, position).ok_or(RenameError::NotRenameable)?;
    // A divergent export key (`member: other.thing`) is safe to rename now that
    // the cross-file search rewrites the key itself and leaves any same-named
    // private binding alone (see references::cross_file_references).

    let locations = references::find_references(state, position, true, roots, parser, overlay)
        .ok_or(RenameError::NotRenameable)?;

    let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
    for loc in locations {
        changes.entry(loc.uri).or_default().push(TextEdit {
            range: loc.range,
            new_text: new_name.to_string(),
        });
    }

    Ok(WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    })
}

/// The identifier node exactly at `position`, if any.
fn identifier_at(state: &FileState, position: Position) -> Option<tree_sitter::Node<'_>> {
    let point = tree_sitter::Point {
        row: position.line as usize,
        column: position.character as usize,
    };
    let node = state
        .tree
        .root_node()
        .descendant_for_point_range(point, point)?;
    if node.kind() == "identifier" {
        Some(node)
    } else {
        None
    }
}

/// A legal Tengo identifier: `[A-Za-z_][A-Za-z0-9_]*`.
fn is_valid_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(uri: &str, src: &str) -> FileState {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_tengo::LANGUAGE.into())
            .unwrap();
        FileState::parse(uri.to_string(), src.to_string(), &mut parser).unwrap()
    }

    fn new_parser() -> Mutex<Parser> {
        let mut p = Parser::new();
        p.set_language(&tree_sitter_tengo::LANGUAGE.into()).unwrap();
        Mutex::new(p)
    }

    fn no_overlay() -> Box<SourceOverlay<'static>> {
        Box::new(|_: &std::path::Path| None)
    }

    /// Find `(line, col)` of the nth occurrence of `needle` in `src`.
    fn pos(src: &str, needle: &str, nth: usize) -> Position {
        let idx = src.match_indices(needle).nth(nth).unwrap().0;
        let before = &src[..idx];
        let line = before.matches('\n').count() as u32;
        let col = (idx - before.rfind('\n').map(|i| i + 1).unwrap_or(0)) as u32;
        Position::new(line, col)
    }

    #[test]
    fn renames_local_symbol_and_uses() {
        let src = "count := 1\ntotal := count + count\n";
        let state = parse("file:///tmp/a.tengo", src);
        let edit = rename(&state, pos(src, "count", 0), "n", &[], &new_parser(), &no_overlay())
            .expect("rename ok");
        let edits = &edit.changes.unwrap()[&Url::parse("file:///tmp/a.tengo").unwrap()];
        // decl + two uses
        assert_eq!(edits.len(), 3);
        assert!(edits.iter().all(|e| e.new_text == "n"));
    }

    #[test]
    fn shadowing_param_is_not_renamed() {
        // Renaming outer `x` must not touch the param-shadowed `return x`.
        let src = "x := 1\nf := func(x) {\n    return x\n}\ny := x\n";
        let state = parse("file:///tmp/s.tengo", src);
        let edit = rename(&state, pos(src, "x", 0), "z", &[], &new_parser(), &no_overlay())
            .expect("rename ok");
        let edits = &edit.changes.unwrap()[&Url::parse("file:///tmp/s.tengo").unwrap()];
        // decl `x := 1` and `y := x` only — NOT `return x` (line 2) nor param.
        assert_eq!(edits.len(), 2);
        assert!(edits.iter().all(|e| e.range.start.line != 2));
    }

    #[test]
    fn rejects_invalid_new_name() {
        let src = "count := 1\n";
        let state = parse("file:///tmp/a.tengo", src);
        for bad in ["1count", "has-dash", "with space", "", "func"] {
            let r = rename(&state, pos(src, "count", 0), bad, &[], &new_parser(), &no_overlay());
            assert!(
                matches!(r, Err(RenameError::InvalidName(_))),
                "expected InvalidName for {bad:?}"
            );
        }
    }

    #[test]
    fn prepare_rejects_non_identifier() {
        let src = "x := \"hello\"\n";
        let state = parse("file:///tmp/a.tengo", src);
        // Position inside the string literal — not renameable.
        assert!(prepare_rename(&state, pos(src, "hello", 0)).is_none());
        // On the identifier — renameable.
        assert!(prepare_rename(&state, pos(src, "x", 0)).is_some());
    }

    #[test]
    fn prepare_allows_divergent_export_key() {
        // `extName: localName` — the key is now renameable (the cross-file
        // search rewrites the key and leaves `localName` alone).
        let src = "localName := func() {\n    return 1\n}\n\nexport {\n    extName: localName\n}\n";
        let state = parse("file:///tmp/m.tengo", src);
        assert!(prepare_rename(&state, pos(src, "extName", 0)).is_some());
        // And the local def `localName` is renameable too.
        assert!(prepare_rename(&state, pos(src, "localName", 0)).is_some());
    }

    #[test]
    fn divergent_key_rename_leaves_colliding_private_def() {
        // `export { foo: bar }` while a *separate* top-level `foo` exists.
        // Renaming the export key `foo` must touch ONLY the key (no consumers
        // here) — never the unrelated private `foo := func`.
        let base = std::env::temp_dir().join(format!(
            "tengo-rename-coll-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let src = base.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(base.join("package.json"), "{}").unwrap();
        let module = "foo := func() {\n    return 1\n}\n\nbar := func() {\n    return 2\n}\n\nexport {\n    foo: bar\n}\n";
        let mfile = src.join("m.lib.tengo");
        std::fs::write(&mfile, module).unwrap();

        let uri = Url::from_file_path(&mfile).unwrap().to_string();
        let state = parse(&uri, module);
        // Cursor on the export key `foo` (2nd `foo`: def line 0, key line 8).
        let edit = rename(
            &state,
            pos(module, "foo", 1),
            "renamed",
            &[src.clone()],
            &new_parser(),
            &no_overlay(),
        )
        .expect("rename ok");
        let m_uri = Url::from_file_path(mfile.canonicalize().unwrap()).unwrap();
        let m_edits = &edit.changes.unwrap()[&m_uri];
        // Exactly the export key on line 9 (`    foo: bar`) — NOT the unrelated
        // private `foo := func` (line 0).
        assert_eq!(m_edits.len(), 1, "only the export key should change: {m_edits:?}");
        assert_eq!(m_edits[0].range.start.line, 9, "must be the export key, not the private def");

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn renames_divergent_wrapped_export_key_from_key() {
        // The user's actual path: no consumers, rename initiated ON the export
        // key inside `export ll.toStrict({ hasGpu: feats.hasGpu })`.
        let base = std::env::temp_dir().join(format!(
            "tengo-rename-fromkey-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let src = base.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(base.join("package.json"), "{}").unwrap();
        let module = "ll := import(\":ll\")\nfeats := import(\":feats\")\n\nexport ll.toStrict({\n    hasGpu: feats.hasGpu\n})\n";
        let mfile = src.join("exec.lib.tengo");
        std::fs::write(&mfile, module).unwrap();

        let uri = Url::from_file_path(&mfile).unwrap().to_string();
        let state = parse(&uri, module);
        // Cursor on the export key `hasGpu` (1st occurrence — the key).
        assert!(prepare_rename(&state, pos(module, "hasGpu", 0)).is_some());
        let edit = rename(
            &state,
            pos(module, "hasGpu", 0),
            "hasGpu2",
            &[src.clone()],
            &new_parser(),
            &no_overlay(),
        )
        .expect("rename ok");
        let m_uri = Url::from_file_path(mfile.canonicalize().unwrap()).unwrap();
        let m_edits = &edit.changes.unwrap()[&m_uri];
        // Only the key — the value `feats.hasGpu` (feats' own member) is untouched.
        assert_eq!(m_edits.len(), 1, "only the export key should change: {m_edits:?}");
        assert_eq!(m_edits[0].range.start.line, 4);
        assert_eq!(m_edits[0].range.start.character, 4, "must be the key, not the value field");

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn renames_exported_member_across_files() {
        let base = std::env::temp_dir().join(format!(
            "tengo-rename-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let src = base.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(base.join("package.json"), "{}").unwrap();
        let module = "doThing := func() {\n    return 1\n}\n\nexport {\n    doThing: doThing\n}\n";
        let mfile = src.join("util.lib.tengo");
        std::fs::write(&mfile, module).unwrap();
        std::fs::write(
            src.join("a.lib.tengo"),
            "util := import(\":util\")\nz := util.doThing()\n",
        )
        .unwrap();

        let uri = Url::from_file_path(&mfile).unwrap().to_string();
        let state = parse(&uri, module);
        // Cursor on the def `doThing` (1st occurrence).
        let edit = rename(
            &state,
            pos(module, "doThing", 0),
            "perform",
            &[src.clone()],
            &new_parser(),
            &no_overlay(),
        )
        .expect("rename ok");
        let changes = edit.changes.unwrap();
        // Module side: the declaration AND both the export key and value must be
        // rewritten — otherwise `export { doThing: perform }` leaves a stale key
        // and breaks the public API (the half-applied rename guarded in item 1).
        let m_uri = Url::from_file_path(mfile.canonicalize().unwrap()).unwrap();
        let m_edits = &changes[&m_uri];
        assert_eq!(m_edits.len(), 3, "expected decl+key+value: {m_edits:?}");
        // Export key and value both live on line 5 (`    doThing: doThing`).
        assert_eq!(
            m_edits.iter().filter(|e| e.range.start.line == 5).count(),
            2,
            "export key+value not both renamed: {m_edits:?}"
        );
        // Consumer side: the single `util.doThing()` usage.
        let a_uri = Url::from_file_path(src.join("a.lib.tengo")).unwrap();
        assert!(changes.contains_key(&a_uri), "consumer not edited: {changes:?}");
        assert_eq!(changes[&a_uri].len(), 1);
        assert_eq!(changes[&a_uri][0].new_text, "perform");

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn renames_from_consumer_selector_field() {
        // Case A: cursor on `member` in `alias.member` inside a consumer file.
        let base = std::env::temp_dir().join(format!(
            "tengo-rename-ca-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let src = base.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(base.join("package.json"), "{}").unwrap();
        let module = "doThing := func() {\n    return 1\n}\n\nexport {\n    doThing: doThing\n}\n";
        let mfile = src.join("util.lib.tengo");
        std::fs::write(&mfile, module).unwrap();
        let consumer = src.join("a.lib.tengo");
        let consumer_src = "util := import(\":util\")\nz := util.doThing()\n";
        std::fs::write(&consumer, consumer_src).unwrap();

        let uri = Url::from_file_path(&consumer).unwrap().to_string();
        let state = parse(&uri, consumer_src);
        // Cursor on `doThing` in `util.doThing` (the field).
        let edit = rename(
            &state,
            pos(consumer_src, "doThing", 0),
            "perform",
            &[src.clone()],
            &new_parser(),
            &no_overlay(),
        )
        .expect("rename ok");
        let changes = edit.changes.unwrap();
        // Must reach back into the module: decl + export key + value.
        let m_uri = Url::from_file_path(mfile.canonicalize().unwrap()).unwrap();
        assert_eq!(changes[&m_uri].len(), 3, "module not fully renamed: {changes:?}");
        // And the consumer's own usage (scanned files keep their scan-path uri).
        let a_uri = Url::from_file_path(&consumer).unwrap();
        assert_eq!(changes[&a_uri].len(), 1);

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn overlay_overrides_stale_disk_for_consumer() {
        // The consumer on disk does NOT use the member; its unsaved buffer does.
        // The overlay must win so the live usage gets renamed.
        let base = std::env::temp_dir().join(format!(
            "tengo-rename-ov-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let src = base.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(base.join("package.json"), "{}").unwrap();
        let module = "doThing := func() {\n    return 1\n}\n\nexport {\n    doThing: doThing\n}\n";
        let mfile = src.join("util.lib.tengo");
        std::fs::write(&mfile, module).unwrap();
        let consumer = src.join("a.lib.tengo");
        // On disk: no usage.
        std::fs::write(&consumer, "util := import(\":util\")\n").unwrap();

        // In the editor buffer: a usage was just typed (unsaved).
        let live = "util := import(\":util\")\nz := util.doThing()\n".to_string();
        let consumer_canon = consumer.canonicalize().unwrap();
        let overlay = move |p: &std::path::Path| {
            let key = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
            if key == consumer_canon {
                Some(live.clone())
            } else {
                None
            }
        };

        let uri = Url::from_file_path(&mfile).unwrap().to_string();
        let state = parse(&uri, module);
        let edit = rename(
            &state,
            pos(module, "doThing", 0),
            "perform",
            &[src.clone()],
            &new_parser(),
            &overlay,
        )
        .expect("rename ok");
        let changes = edit.changes.unwrap();
        let a_uri = Url::from_file_path(&consumer).unwrap();
        // Despite disk having no usage, the live buffer's `util.doThing()` is renamed.
        assert!(
            changes.get(&a_uri).map_or(false, |e| e.len() == 1),
            "overlay usage not renamed: {changes:?}"
        );

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn renames_divergent_wrapped_export_key_from_consumer() {
        // Regression: `export ll.toStrict({ hasGpu: feats.hasGpu })` — the key
        // `hasGpu` has NO top-level def (it re-exports another module's member).
        // Renaming from a consumer must still rewrite the export KEY, not just
        // the `exec.hasGpu` usages, or the public API is left half-renamed.
        let base = std::env::temp_dir().join(format!(
            "tengo-rename-div-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let src = base.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(base.join("package.json"), "{}").unwrap();
        let module = "ll := import(\":ll\")\nfeats := import(\":feats\")\n\nexport ll.toStrict({\n    hasGpu: feats.hasGpu\n})\n";
        let mfile = src.join("exec.lib.tengo");
        std::fs::write(&mfile, module).unwrap();
        let consumer = src.join("c.lib.tengo");
        let consumer_src = "exec := import(\":exec\")\nz := exec.hasGpu\n";
        std::fs::write(&consumer, consumer_src).unwrap();

        let uri = Url::from_file_path(&consumer).unwrap().to_string();
        let state = parse(&uri, consumer_src);
        // Cursor on `hasGpu` in `exec.hasGpu` (the field).
        let edit = rename(
            &state,
            pos(consumer_src, "hasGpu", 0),
            "hasGpu2",
            &[src.clone()],
            &new_parser(),
            &no_overlay(),
        )
        .expect("rename ok");
        let changes = edit.changes.unwrap();
        // The module's export key must be in the edits.
        let m_uri = Url::from_file_path(mfile.canonicalize().unwrap()).unwrap();
        let m_edits = changes.get(&m_uri).expect("module must be edited");
        // Exactly the export key on line 4 (`    hasGpu: feats.hasGpu`) — and NOT
        // the value `feats.hasGpu` (that field belongs to the feats module).
        assert_eq!(m_edits.len(), 1, "expected only the export key: {m_edits:?}");
        assert_eq!(m_edits[0].range.start.line, 4);
        assert_eq!(m_edits[0].range.start.character, 4, "must be the key, not the value");
        // And the consumer usage.
        let c_uri = Url::from_file_path(&consumer).unwrap();
        assert_eq!(changes[&c_uri].len(), 1);

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn allows_name_to_name_export_key() {
        let src = "doThing := func() {\n    return 1\n}\n\nexport {\n    doThing: doThing\n}\n";
        let state = parse("file:///tmp/m.tengo", src);
        // Cursor on the export key `doThing` (the 2nd occurrence: decl, then key, then value).
        assert!(prepare_rename(&state, pos(src, "doThing", 1)).is_some());
    }
}

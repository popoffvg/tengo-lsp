use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tower_lsp::lsp_types::{Location, Position, Url};
use tree_sitter::Parser;

use crate::document::FileState;
use crate::resolver;
use crate::symbols::Symbol;

/// Directories that never contain first-party references and would make a
/// workspace scan needlessly slow.
const SKIP_DIRS: &[&str] = &["node_modules", "dist", "target", ".git"];

/// Upper bound on files visited during a workspace scan, as a safety valve
/// against pathologically large trees.
const MAX_FILES: usize = 10_000;

/// Provides the in-memory source for a path when the file is open in the
/// editor with unsaved changes. Cross-file scans consult this before falling
/// back to disk so emitted ranges match the editor's live buffer (critical for
/// rename, which writes those ranges back). Return `None` to use disk.
pub type SourceOverlay<'a> = dyn Fn(&Path) -> Option<String> + 'a;

/// Read a file's source, preferring the editor's in-memory buffer over disk.
fn read_source(overlay: &SourceOverlay, path: &Path) -> Option<String> {
    overlay(path).or_else(|| std::fs::read_to_string(path).ok())
}

/// Handle textDocument/references.
///
/// Two modes:
/// - **Cross-file**: when the cursor is on an exported member (either an
///   `export { ... }` key in the current file, or the `field` of
///   `alias.member` where `alias` is an import) we resolve the *module file*
///   that owns the member, then return every `alias.member` usage across the
///   workspace plus the member's own references inside that module.
/// - **Local**: any other identifier resolves to its definition's scope within
///   the current file only (previous behaviour).
pub fn find_references(
    state: &FileState,
    position: Position,
    include_declaration: bool,
    roots: &[PathBuf],
    parser: &Mutex<Parser>,
    overlay: &SourceOverlay,
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

    let name = node.utf8_text(state.source.as_bytes()).ok()?.to_string();

    // Decide whether this is a cross-file (exported member) reference search.
    if let Some((module_file, member)) = resolve_module_member(state, node, &name) {
        let search_roots = effective_roots(state, roots);
        return cross_file_references(&module_file, &member, include_declaration, &search_roots, parser, overlay);
    }

    // Local symbol: in-file references within the definition's scope.
    local_references(state, &name, node.start_byte(), include_declaration)
}

/// If the cursor sits on an exported member, return the absolute path of the
/// module file that owns it and the member name.
fn resolve_module_member(
    state: &FileState,
    node: tree_sitter::Node,
    name: &str,
) -> Option<(PathBuf, String)> {
    let current_file = resolver::uri_to_path(&state.uri)?;

    // Case A: `alias.member` — the cursor is on the `field`, and `alias` is an
    // import. The module is the resolved target of that import.
    if let Some(parent) = node.parent() {
        if parent.kind() == "selector_expression" {
            if let Some(field) = parent.child_by_field_name("field") {
                if field.id() == node.id() {
                    if let Some(object) = parent.child_by_field_name("object") {
                        if object.kind() == "identifier" {
                            if let Ok(obj_name) = object.utf8_text(state.source.as_bytes()) {
                                if let Some(import) = state.imports.get(obj_name) {
                                    let target =
                                        resolver::resolve_import(&import.module_path, &current_file)?;
                                    return Some((canonical(&target), name.to_string()));
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Case B: the cursor is on a name that the current file exports — the
    // module is the current file itself. Guard against a local symbol that
    // merely *shadows* an export name (e.g. a parameter): only treat it as the
    // exported member if the name resolves to a top-level binding (or to no
    // local binding at all, i.e. a free reference to the module-level def).
    let resolves_to_top_level = match state.resolve_def(name, node.start_byte()) {
        Some(def) => state
            .scopes
            .get(def.scope_id)
            .map_or(false, |s| s.parent.is_none()),
        None => true,
    };
    if resolves_to_top_level {
        let exports = crate::exports::parse_exports(&state.source);
        if exports.iter().any(|m| m.name == name) {
            return Some((canonical(Path::new(&current_file)), name.to_string()));
        }
    }

    None
}

/// Collect references to `member` of the module at `module_file`:
/// every `alias.member` across the workspace where `alias` resolves to that
/// module, plus the member's own references inside the module file.
fn cross_file_references(
    module_file: &Path,
    member: &str,
    include_declaration: bool,
    roots: &[PathBuf],
    parser: &Mutex<Parser>,
    overlay: &SourceOverlay,
) -> Option<Vec<Location>> {
    let mut locations: Vec<Location> = Vec::new();
    let target = canonical(module_file);

    // References inside the defining module (bare `member` usages).
    if let Some(src) = read_source(overlay, module_file) {
        if let Some(uri) = Url::from_file_path(module_file).ok() {
            let mut guard = parser.lock().unwrap();
            if let Some(state) = FileState::parse(uri.to_string(), src, &mut guard) {
                drop(guard);
                if let Some(def) = top_level_def(&state, member) {
                    locations.extend(scope_references(&state, member, def, include_declaration));
                }
            }
        }
    }

    // External usages: `alias.member` in every file that imports the module.
    let mut visited = 0usize;
    let mut seen_paths: HashSet<PathBuf> = HashSet::new();
    for root in roots {
        scan_dir(root, &mut visited, &mut seen_paths, &mut |path| {
            if canonical(path) == target {
                return; // module's own bare refs already handled above
            }
            let src = match read_source(overlay, path) {
                Some(s) => s,
                None => return,
            };
            let uri = match Url::from_file_path(path) {
                Ok(u) => u,
                Err(_) => return,
            };
            let mut guard = parser.lock().unwrap();
            let state = match FileState::parse(uri.to_string(), src, &mut guard) {
                Some(s) => s,
                None => return,
            };
            drop(guard);

            let path_str = path.to_string_lossy().to_string();
            for (alias, import) in &state.imports {
                let resolved = match resolver::resolve_import(&import.module_path, &path_str) {
                    Some(p) => canonical(&p),
                    None => continue,
                };
                if resolved != target {
                    continue;
                }
                collect_selector_refs(state.tree.root_node(), &state, alias, member, &uri, &mut locations);
            }
        });
    }

    dedup(&mut locations);
    if locations.is_empty() {
        None
    } else {
        Some(locations)
    }
}

/// Walk a subtree collecting `alias.member` selector field locations.
fn collect_selector_refs(
    node: tree_sitter::Node,
    state: &FileState,
    alias: &str,
    member: &str,
    uri: &Url,
    out: &mut Vec<Location>,
) {
    if node.kind() == "selector_expression" {
        if let (Some(object), Some(field)) = (
            node.child_by_field_name("object"),
            node.child_by_field_name("field"),
        ) {
            if object.kind() == "identifier" {
                let obj_ok = object.utf8_text(state.source.as_bytes()) == Ok(alias);
                let field_ok = field.utf8_text(state.source.as_bytes()) == Ok(member);
                if obj_ok && field_ok {
                    out.push(Location {
                        uri: uri.clone(),
                        range: crate::symbols::node_to_lsp_range(field),
                    });
                }
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_selector_refs(child, state, alias, member, uri, out);
    }
}

/// Find a top-level definition (module scope) named `name`.
fn top_level_def<'a>(state: &'a FileState, name: &str) -> Option<&'a Symbol> {
    state.defs.iter().find(|d| {
        d.name == name
            && state
                .scopes
                .get(d.scope_id)
                .map_or(false, |s| s.parent.is_none())
    })
}

/// In-file references to `name` within `def`'s scope.
fn scope_references(
    state: &FileState,
    name: &str,
    def: &Symbol,
    include_declaration: bool,
) -> Vec<Location> {
    let uri = match Url::parse(&state.uri) {
        Ok(u) => u,
        Err(_) => return Vec::new(),
    };
    let def_scope = match state.scopes.get(def.scope_id) {
        Some(s) => s,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    if include_declaration {
        out.push(Location {
            uri: uri.clone(),
            range: def.range,
        });
    }
    for r in &state.refs {
        if r.name == name
            && r.byte_range.start >= def_scope.start_byte
            && r.byte_range.start < def_scope.end_byte
            && r.byte_range.start >= def.byte_range.start
        {
            // Shadow-awareness: a bare identifier inside `def`'s scope may resolve
            // to an inner binding (e.g. a parameter named the same). Keep it only
            // if it actually resolves to *this* def; otherwise renaming would
            // rewrite an unrelated, shadowing symbol.
            match state.resolve_def(name, r.byte_range.start) {
                Some(resolved) if resolved.byte_range.start == def.byte_range.start => {}
                _ => continue,
            }
            out.push(Location {
                uri: uri.clone(),
                range: r.range,
            });
        }
    }
    out
}

/// Local (single-file) reference search around the cursor's definition.
fn local_references(
    state: &FileState,
    name: &str,
    byte_offset: usize,
    include_declaration: bool,
) -> Option<Vec<Location>> {
    let def = state.resolve_def(name, byte_offset)?;
    let locations = scope_references(state, name, def, include_declaration);
    if locations.is_empty() {
        None
    } else {
        Some(locations)
    }
}

/// Roots to scan: configured workspace folders, falling back to the current
/// file's package root, then its parent directory.
fn effective_roots(state: &FileState, roots: &[PathBuf]) -> Vec<PathBuf> {
    if !roots.is_empty() {
        return roots.to_vec();
    }
    if let Some(file) = resolver::uri_to_path(&state.uri) {
        let path = PathBuf::from(&file);
        if let Some(pkg) = find_package_root(&path) {
            return vec![pkg];
        }
        if let Some(parent) = path.parent() {
            return vec![parent.to_path_buf()];
        }
    }
    Vec::new()
}

fn find_package_root(start_file: &Path) -> Option<PathBuf> {
    let mut dir = start_file.parent()?;
    loop {
        if dir.join("package.json").exists() {
            return Some(dir.to_path_buf());
        }
        dir = dir.parent()?;
    }
}

/// Recursively visit `.tengo` files under `dir`, skipping vendored/build dirs.
fn scan_dir(
    dir: &Path,
    visited: &mut usize,
    seen: &mut HashSet<PathBuf>,
    f: &mut dyn FnMut(&Path),
) {
    if *visited >= MAX_FILES {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if file_type.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if SKIP_DIRS.contains(&name) {
                continue;
            }
            scan_dir(&path, visited, seen, f);
        } else if file_type.is_file() {
            if path.extension().and_then(|e| e.to_str()) == Some("tengo") {
                let canon = canonical(&path);
                if seen.insert(canon) {
                    *visited += 1;
                    if *visited > MAX_FILES {
                        return;
                    }
                    f(&path);
                }
            }
        }
    }
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn dedup(locations: &mut Vec<Location>) {
    let mut seen: HashSet<(String, u32, u32, u32, u32)> = HashSet::new();
    locations.retain(|l| {
        let key = (
            l.uri.to_string(),
            l.range.start.line,
            l.range.start.character,
            l.range.end.line,
            l.range.end.character,
        );
        seen.insert(key)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn new_parser() -> Mutex<Parser> {
        let mut p = Parser::new();
        p.set_language(&tree_sitter_tengo::LANGUAGE.into()).unwrap();
        Mutex::new(p)
    }

    /// Overlay that never overrides disk — tests read fixtures from disk.
    fn no_overlay() -> Box<SourceOverlay<'static>> {
        Box::new(|_: &Path| None)
    }

    fn parse(uri: &str, src: &str) -> FileState {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_tengo::LANGUAGE.into())
            .unwrap();
        FileState::parse(uri.to_string(), src.to_string(), &mut parser).unwrap()
    }

    /// Build a temp package with a module and two consumers, returning its src dir.
    fn build_pkg() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "tengo-refs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let src = base.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(base.join("package.json"), "{}").unwrap();

        // The module: defines and exports `doThing`, and uses it internally.
        std::fs::write(
            src.join("util.lib.tengo"),
            "doThing := func(x) {\n    return x\n}\n\nwrap := func(y) {\n    return doThing(y)\n}\n\nexport {\n    doThing: doThing\n}\n",
        )
        .unwrap();

        // Consumer A: imports util and calls util.doThing.
        std::fs::write(
            src.join("a.lib.tengo"),
            "util := import(\":util\")\n\nrun := func() {\n    return util.doThing(1)\n}\n",
        )
        .unwrap();

        // Consumer B: aliases differently and calls twice.
        std::fs::write(
            src.join("b.lib.tengo"),
            "u := import(\":util\")\n\nx := u.doThing(2)\ny := u.doThing(3)\n",
        )
        .unwrap();
        src
    }

    #[test]
    fn selector_field_finds_cross_file_references() {
        let src = build_pkg();
        let consumer = src.join("a.lib.tengo");
        let text = std::fs::read_to_string(&consumer).unwrap();
        let uri = Url::from_file_path(&consumer).unwrap().to_string();
        let state = parse(&uri, &text);

        // Cursor on `doThing` in `util.doThing` (line 3).
        let line = text.lines().nth(3).unwrap();
        let col = line.find("doThing").unwrap() as u32;

        let parser = new_parser();
        let locs = find_references(&state, Position::new(3, col), false, &[], &parser, &no_overlay())
            .expect("should find cross-file references");

        // a.lib.tengo (1) + b.lib.tengo (2) usages of `.doThing`.
        let external = locs
            .iter()
            .filter(|l| {
                let p = l.uri.to_file_path().unwrap();
                p.ends_with("a.lib.tengo") || p.ends_with("b.lib.tengo")
            })
            .count();
        assert!(external >= 3, "expected >=3 external refs, got {external} in {locs:?}");

        std::fs::remove_dir_all(src.parent().unwrap()).ok();
    }

    #[test]
    fn exported_symbol_includes_internal_and_external() {
        let src = build_pkg();
        let module = src.join("util.lib.tengo");
        let text = std::fs::read_to_string(&module).unwrap();
        let uri = Url::from_file_path(&module).unwrap().to_string();
        let state = parse(&uri, &text);

        // Cursor on the export key `doThing`.
        let key_line = text.lines().position(|l| l.contains("doThing: doThing")).unwrap();
        let col = text.lines().nth(key_line).unwrap().find("doThing").unwrap() as u32;

        let parser = new_parser();
        let locs = find_references(&state, Position::new(key_line as u32, col), true, &[], &parser, &no_overlay())
            .expect("should find references");

        let in_module = locs
            .iter()
            .filter(|l| l.uri.to_file_path().unwrap().ends_with("util.lib.tengo"))
            .count();
        let external = locs
            .iter()
            .filter(|l| {
                let p = l.uri.to_file_path().unwrap();
                p.ends_with("a.lib.tengo") || p.ends_with("b.lib.tengo")
            })
            .count();
        assert!(in_module >= 1, "expected internal refs, got {in_module}");
        assert!(external >= 3, "expected external refs, got {external}");

        std::fs::remove_dir_all(src.parent().unwrap()).ok();
    }

    #[test]
    fn private_local_symbol_stays_in_file() {
        let src = build_pkg();
        let module = src.join("util.lib.tengo");
        let text = std::fs::read_to_string(&module).unwrap();
        let uri = Url::from_file_path(&module).unwrap().to_string();
        let state = parse(&uri, &text);

        // `wrap` is defined but NOT exported; references must be in-file only.
        let wrap_line = text.lines().position(|l| l.contains("wrap := func")).unwrap();
        let col = text.lines().nth(wrap_line).unwrap().find("wrap").unwrap() as u32;

        let parser = new_parser();
        let locs = find_references(&state, Position::new(wrap_line as u32, col), true, &[], &parser, &no_overlay());

        if let Some(locs) = locs {
            assert!(
                locs.iter()
                    .all(|l| l.uri.to_file_path().unwrap().ends_with("util.lib.tengo")),
                "private symbol leaked outside its file: {locs:?}"
            );
        }

        std::fs::remove_dir_all(src.parent().unwrap()).ok();
    }

    #[test]
    fn shadowing_local_does_not_trigger_cross_file() {
        let base = std::env::temp_dir().join(format!(
            "tengo-shadow-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let src = base.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(base.join("package.json"), "{}").unwrap();

        // `doThing` is exported, but a parameter of `helper` shadows it.
        let module = "doThing := func() {\n    return 1\n}\n\nhelper := func(doThing) {\n    return doThing()\n}\n\nexport {\n    doThing: doThing\n}\n";
        let mfile = src.join("util.lib.tengo");
        std::fs::write(&mfile, module).unwrap();

        // An external consumer uses the real export.
        std::fs::write(
            src.join("c.lib.tengo"),
            "u := import(\":util\")\nz := u.doThing()\n",
        )
        .unwrap();

        let uri = Url::from_file_path(&mfile).unwrap().to_string();
        let state = parse(&uri, module);

        // Cursor on the shadowed `doThing` inside helper's body (line 5).
        let line = module.lines().position(|l| l.contains("return doThing()")).unwrap();
        let col = module.lines().nth(line).unwrap().find("doThing").unwrap() as u32;

        let parser = new_parser();
        let locs = find_references(&state, Position::new(line as u32, col), true, &[src.clone()], &parser, &no_overlay());

        if let Some(locs) = locs {
            assert!(
                locs.iter()
                    .all(|l| l.uri.to_file_path().unwrap().ends_with("util.lib.tengo")),
                "shadowing param leaked to cross-file refs: {locs:?}"
            );
        }

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn no_package_does_not_panic() {
        let state = parse(
            "file:///tmp/standalone.tengo",
            "x := 1\ny := x + x\n",
        );
        let parser = new_parser();
        // `x` is local; must not panic with no workspace.
        let _ = find_references(&state, Position::new(0, 0), true, &[], &parser, &no_overlay());
    }
}

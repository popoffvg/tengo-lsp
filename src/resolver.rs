use std::path::{Path, PathBuf};

/// Known Tengo stdlib module names that don't resolve to files.
const STDLIB_MODULES: &[&str] = &[
    "base64", "enum", "fmt", "hex", "json", "math", "os", "rand", "text", "times",
];

/// Resolve an import path to an absolute file path.
///
/// Supported forms:
/// - Local package artifact import (`:ll`, `:pframes.util`) resolves via current package `src`.
/// - NPM artifact import (`@scope/pkg:index`, `pkg:index`) resolves via nearest `node_modules`.
/// - Stdlib module names return None (no file to navigate to).
pub fn resolve_import(module_path: &str, current_file: &str) -> Option<PathBuf> {
    if STDLIB_MODULES.contains(&module_path) {
        return None;
    }

    if module_path.starts_with(':') {
        return resolve_local_artifact(module_path, current_file);
    }

    if let Some((pkg, artifact)) = split_package_artifact(module_path) {
        return resolve_node_module_artifact(pkg, artifact, current_file);
    }

    None
}

fn split_package_artifact(module_path: &str) -> Option<(&str, &str)> {
    let idx = module_path.rfind(':')?;
    let (pkg, artifact_with_colon) = module_path.split_at(idx);
    let artifact = artifact_with_colon.strip_prefix(':')?;
    if pkg.is_empty() || artifact.is_empty() {
        return None;
    }
    Some((pkg, artifact))
}

fn resolve_local_artifact(module_path: &str, current_file: &str) -> Option<PathBuf> {
    let artifact = module_path.strip_prefix(':')?;
    let package_root = find_up_with_child(Path::new(current_file), "package.json")?;
    resolve_artifact_from_package_root(&package_root, artifact)
}

fn resolve_node_module_artifact(pkg: &str, artifact: &str, current_file: &str) -> Option<PathBuf> {
    let mut dir = Path::new(current_file).parent()?;

    loop {
        let pkg_dir = dir.join("node_modules").join(pkg);
        if pkg_dir.exists() {
            if let Some(p) = resolve_artifact_from_package_root(&pkg_dir, artifact) {
                return Some(p);
            }
        }

        dir = dir.parent()?;
    }
}

fn resolve_artifact_from_package_root(package_root: &Path, artifact: &str) -> Option<PathBuf> {
    let src = package_root.join("src");
    let path_style = artifact.replace('.', "/");

    let candidates = [
        src.join(format!("{}.lib.tengo", path_style)),
        src.join(format!("{}.tpl.tengo", path_style)),
        src.join(format!("{}.tengo", path_style)),
        src.join(path_style.clone()).join("index.lib.tengo"),
        src.join(path_style.clone()).join("index.tpl.tengo"),
        src.join(path_style).join("index.tengo"),
    ];

    for candidate in candidates {
        if candidate.exists() {
            return Some(candidate.canonicalize().unwrap_or(candidate));
        }
    }

    None
}

fn find_up_with_child(start_file: &Path, child_name: &str) -> Option<PathBuf> {
    let mut dir = start_file.parent()?;
    loop {
        if dir.join(child_name).exists() {
            return Some(dir.to_path_buf());
        }
        dir = dir.parent()?;
    }
}

/// Convert a file:// URI to a filesystem path.
pub fn uri_to_path(uri: &str) -> Option<String> {
    uri.strip_prefix("file://").map(|s| {
        // Handle percent-encoding for common cases
        s.replace("%20", " ")
    })
}

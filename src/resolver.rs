use std::path::{Path, PathBuf};

/// Known Tengo stdlib module names that don't resolve to files.
const STDLIB_MODULES: &[&str] = &[
    "base64", "enum", "fmt", "hex", "json", "math", "os", "rand", "text", "times",
];

/// Resolve an import path to an absolute file path.
///
/// - Relative paths (`./foo`, `../bar`) resolve relative to `current_file`.
/// - Stdlib module names return None (no file to navigate to).
/// - Tries `.tengo` extension first, then bare path.
pub fn resolve_import(module_path: &str, current_file: &str) -> Option<PathBuf> {
    if STDLIB_MODULES.contains(&module_path) {
        return None;
    }

    if !module_path.starts_with("./") && !module_path.starts_with("../") {
        // Could be a user module referenced by bare name — try relative anyway
        let current = Path::new(current_file);
        let base = current.parent()?;
        return try_resolve(base, module_path);
    }

    let current = Path::new(current_file);
    let base = current.parent()?;
    try_resolve(base, module_path)
}

fn try_resolve(base: &Path, module_path: &str) -> Option<PathBuf> {
    // Try with .tengo extension
    let with_ext = base.join(format!("{}.tengo", module_path));
    if with_ext.exists() {
        return Some(with_ext.canonicalize().unwrap_or(with_ext));
    }

    // Try bare path
    let bare = base.join(module_path);
    if bare.exists() {
        return Some(bare.canonicalize().unwrap_or(bare));
    }

    None
}

/// Convert a file:// URI to a filesystem path.
pub fn uri_to_path(uri: &str) -> Option<String> {
    uri.strip_prefix("file://").map(|s| {
        // Handle percent-encoding for common cases
        s.replace("%20", " ")
    })
}

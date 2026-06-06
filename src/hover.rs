use tower_lsp::lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position};

use crate::document::FileState;
use crate::exports::{self, ExportMember, ItemKind};
use crate::resolver;

/// Handle textDocument/hover. When the cursor is on the member of
/// `<alias>.<member>` and `<alias>` is an imported module, show the member's
/// signature and doc comment.
pub fn hover(state: &FileState, position: Position) -> Option<Hover> {
    let (alias, member) = alias_member_at(state, position)?;
    let import_info = state.imports.get(&alias)?;

    let current_file = resolver::uri_to_path(&state.uri)?;
    let resolved = resolver::resolve_import(&import_info.module_path, &current_file)?;
    let source = std::fs::read_to_string(&resolved).ok()?;

    let members = exports::parse_exports(&source);
    let found = members.into_iter().find(|m| m.name == member)?;

    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: render_hover(&found),
        }),
        range: None,
    })
}

fn render_hover(m: &ExportMember) -> String {
    let head = match m.kind {
        ItemKind::Function => match &m.signature {
            Some(sig) => format!("{}{}", m.name, sig),
            None => m.name.clone(),
        },
        ItemKind::Variable => m.name.clone(),
    };
    let mut out = format!("```tengo\n{head}\n```");
    if let Some(doc) = &m.doc {
        out.push_str("\n\n");
        out.push_str(&format_doc(doc));
    }
    out
}

/// Render a doc comment as Markdown, turning JSDoc-style `@param` / `@return`
/// tags into readable sections instead of one collapsed paragraph (Markdown
/// treats single newlines as spaces, so consecutive tag lines run together).
fn format_doc(doc: &str) -> String {
    let mut desc: Vec<String> = Vec::new();
    let mut params: Vec<String> = Vec::new();
    let mut returns: Vec<String> = Vec::new();
    let mut other: Vec<String> = Vec::new();

    // Where the next plain (non-tag) line belongs: the lead description until a
    // tag is seen, then the most recent tag's bucket (so wrapped descriptions
    // and sub-fields stay attached to their tag instead of leaking into the lead).
    enum Cur {
        Desc,
        Params,
        Returns,
        Other,
    }
    let mut cur = Cur::Desc;

    for raw in doc.lines() {
        let line = raw.trim();
        if let Some(rest) = strip_tag(line, &["@param", "@arg", "@argument"]) {
            params.push(format_param(rest));
            cur = Cur::Params;
        } else if let Some(rest) = strip_tag(line, &["@return", "@returns"]) {
            returns.push(format_return(rest));
            cur = Cur::Returns;
        } else if let Some(rest) = line.strip_prefix('@') {
            let (tag, body) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            other.push(format!("**{}** {}", tag, body.trim()).trim().to_string());
            cur = Cur::Other;
        } else if let Cur::Desc = cur {
            // Lead description: keep lines (blanks make paragraph breaks).
            desc.push(line.to_string());
        } else if !line.is_empty() {
            // Continuation of the last tag: append to its bullet.
            let bucket = match cur {
                Cur::Params => &mut params,
                Cur::Returns => &mut returns,
                _ => &mut other,
            };
            if let Some(last) = bucket.last_mut() {
                // A sub-bullet (`- foo`, `* foo`) keeps its own line; wrapped
                // prose is space-joined onto the previous line.
                if matches!(line.chars().next(), Some('-' | '*' | '•')) {
                    last.push_str("  \n  - ");
                    last.push_str(line[1..].trim_start());
                } else {
                    last.push(' ');
                    last.push_str(line);
                }
            } else {
                bucket.push(line.to_string());
            }
        }
    }

    let mut sections: Vec<String> = Vec::new();
    let body = desc.join("\n");
    let body = body.trim();
    if !body.is_empty() {
        sections.push(body.to_string());
    }
    if !params.is_empty() {
        sections.push(format!("**Parameters**\n\n{}", params.join("  \n")));
    }
    if !returns.is_empty() {
        sections.push(format!("**Returns**\n\n{}", returns.join("  \n")));
    }
    if !other.is_empty() {
        sections.push(other.join("\n\n"));
    }
    sections.join("\n\n")
}

/// Strip a leading tag (one of `tags`) followed by a word boundary, returning
/// the trimmed remainder.
fn strip_tag<'a>(line: &'a str, tags: &[&str]) -> Option<&'a str> {
    for tag in tags {
        if let Some(rest) = line.strip_prefix(tag) {
            if rest.is_empty() || rest.starts_with(|c: char| c.is_whitespace()) {
                return Some(rest.trim());
            }
        }
    }
    None
}

/// Format a `@param` body. Accepts `name: type - desc`, `name - desc`,
/// `{type} name desc`, or `name desc`.
fn format_param(body: &str) -> String {
    // JSDoc `{type} name desc`
    let (ty, body) = if let Some(stripped) = body.strip_prefix('{') {
        match stripped.split_once('}') {
            Some((t, r)) => (Some(t.trim().to_string()), r.trim()),
            None => (None, body),
        }
    } else {
        (None, body)
    };

    // name is up to the first `:` or whitespace.
    let split = body
        .find(|c: char| c == ':' || c.is_whitespace())
        .unwrap_or(body.len());
    let name = &body[..split];
    let mut rest = body[split..].trim_start();

    // `name: type - desc` — pull the type from before " - ".
    let mut ty = ty;
    if rest.starts_with(':') {
        let after = rest[1..].trim_start();
        if let Some((t, d)) = after.split_once(" - ") {
            ty = Some(t.trim().to_string());
            rest = d.trim_start();
        } else {
            rest = after;
        }
    } else if let Some(stripped) = rest.strip_prefix('-') {
        rest = stripped.trim_start();
    } else if let Some((_, d)) = rest.split_once(" - ") {
        rest = d.trim_start();
    }

    let desc = rest.trim();
    let mut out = format!("`{name}`");
    if let Some(t) = ty.filter(|t| !t.is_empty()) {
        out.push_str(&format!(" *({t})*"));
    }
    if !desc.is_empty() {
        out.push_str(&format!(" — {desc}"));
    }
    out
}

/// Format a `@return` body. Accepts `name: desc`, `name - desc`, or just `desc`.
fn format_return(body: &str) -> String {
    let (ty, body) = if let Some(stripped) = body.strip_prefix('{') {
        match stripped.split_once('}') {
            Some((t, r)) => (Some(t.trim().to_string()), r.trim()),
            None => (None, body),
        }
    } else {
        (None, body)
    };
    // A leading `name:` or `name -` introduces a named return value.
    let named = body.split_once(" - ").or_else(|| {
        let colon = body.find(':')?;
        let name = body[..colon].trim();
        // Only treat as a name if it's a single bare word (not a sentence).
        (!name.is_empty() && !name.contains(char::is_whitespace))
            .then(|| (name, body[colon + 1..].trim_start()))
    });
    let text = match named {
        Some((name, desc)) => format!("`{}` — {}", name.trim(), desc.trim()),
        None => body.to_string(),
    };
    match ty.filter(|t| !t.is_empty()) {
        Some(t) => format!("*({t})* {text}"),
        None => text,
    }
}

/// Resolve `<alias>.<member>` at `position`, where the cursor is on `member`.
fn alias_member_at(state: &FileState, position: Position) -> Option<(String, String)> {
    let point = tree_sitter::Point {
        row: position.line as usize,
        column: position.character as usize,
    };
    let root = state.tree.root_node();
    let node = root.descendant_for_point_range(point, point)?;
    if node.kind() != "identifier" {
        return None;
    }
    let parent = node.parent()?;
    if parent.kind() != "selector_expression" {
        return None;
    }
    let field = parent.child_by_field_name("field")?;
    if field.id() != node.id() {
        return None;
    }
    let object = parent.child_by_field_name("object")?;
    if object.kind() != "identifier" {
        return None;
    }
    let alias = object.utf8_text(state.source.as_bytes()).ok()?.to_string();
    let member = field.utf8_text(state.source.as_bytes()).ok()?.to_string();
    Some((alias, member))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exports::{ExportMember, ItemKind};
    use tower_lsp::lsp_types::{Position, Range};

    fn rng() -> Range {
        Range::new(Position::new(0, 0), Position::new(0, 0))
    }

    #[test]
    fn renders_function_with_doc() {
        let m = ExportMember {
            name: "createFileDataset".into(),
            key_range: rng(),
            kind: ItemKind::Function,
            signature: Some("(a, b)".into()),
            doc: Some("Builds a dataset.".into()),
        };
        let s = render_hover(&m);
        assert!(s.contains("```tengo\ncreateFileDataset(a, b)\n```"));
        assert!(s.ends_with("Builds a dataset."));
    }

    #[test]
    fn formats_param_and_return_tags() {
        let doc = "Builder function for creating a RunCommand resource.\n\n@param workdir: smart.reference - the working directory for the command.\n@return builder - the builder object with methods.";
        let s = format_doc(doc);
        assert!(s.contains("Builder function for creating a RunCommand resource."));
        assert!(s.contains("**Parameters**"));
        assert!(s.contains("`workdir` *(smart.reference)* — the working directory for the command."));
        assert!(s.contains("**Returns**"));
        assert!(s.contains("`builder` — the builder object with methods."));
    }

    #[test]
    fn attaches_continuation_lines_to_their_tag() {
        let doc = "Creates ephemeral render template resource.\n@param tpl: template resource\n@param opts: (optional) a map of options:\n  - metaInputs: a map of meta inputs. No effect on ephemeral templates.\n@return renderer: a smart resource";
        let s = format_doc(doc);
        // The metaInputs continuation must stay under opts, not leak into the lead.
        let lead = s.split("**Parameters**").next().unwrap();
        assert!(!lead.contains("metaInputs"));
        // Sub-bullet keeps its own line (not flattened inline after the colon).
        assert!(s.contains("`opts` — (optional) a map of options:  \n  - metaInputs: a map of meta inputs. No effect on ephemeral templates."));
        assert!(s.contains("`tpl` — template resource"));
        assert!(s.contains("`renderer` — a smart resource"));
    }

    #[test]
    fn wraps_plain_continuation_inline() {
        let doc = "@param x: a value\n   that wraps across lines";
        let s = format_doc(doc);
        assert!(s.contains("`x` — a value that wraps across lines"));
    }

    #[test]
    fn formats_param_without_type() {
        assert_eq!(format_param("count - number of items"), "`count` — number of items");
    }

    #[test]
    fn formats_jsdoc_brace_type_param() {
        assert_eq!(
            format_param("{string} name the user name"),
            "`name` *(string)* — the user name"
        );
    }

    #[test]
    fn plain_doc_is_unchanged() {
        assert_eq!(format_doc("Builds a dataset."), "Builds a dataset.");
    }

    #[test]
    fn renders_variable_without_doc() {
        let m = ExportMember {
            name: "isGrouped".into(),
            key_range: rng(),
            kind: ItemKind::Variable,
            signature: None,
            doc: None,
        };
        let s = render_hover(&m);
        assert_eq!(s, "```tengo\nisGrouped\n```");
    }

    #[test]
    fn end_to_end_hover_resolves_member() {
        let base = std::env::temp_dir().join(format!("tengo-lsp-hover-{}", std::process::id()));
        let src_dir = base.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(base.join("package.json"), "{}").unwrap();
        let lib = "// Adds two numbers.\nadd := func(a, b) {\n    return a + b\n}\n\nexport {\n    add: add\n}\n";
        std::fs::write(src_dir.join("util.lib.tengo"), lib).unwrap();

        let main_src = "util := import(\":util\")\nx := util.add\n";
        std::fs::write(src_dir.join("main.lib.tengo"), main_src).unwrap();

        let uri = format!("file://{}", src_dir.join("main.lib.tengo").display());
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_tengo::LANGUAGE.into())
            .unwrap();
        let state = FileState::parse(uri, main_src.into(), &mut parser).unwrap();

        let col = main_src.lines().nth(1).unwrap().find("add").unwrap() as u32;
        let h = hover(&state, Position::new(1, col));

        std::fs::remove_dir_all(&base).ok();

        let h = h.expect("hover should resolve add");
        match h.contents {
            HoverContents::Markup(mc) => {
                assert!(mc.value.contains("add(a, b)"));
                assert!(mc.value.contains("Adds two numbers."));
            }
            other => panic!("expected markup, got {other:?}"),
        }
    }

    #[test]
    fn unresolvable_module_no_panic() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_tengo::LANGUAGE.into())
            .unwrap();
        let src = "util := import(\":nonexistent\")\nx := util.add\n";
        let state =
            FileState::parse("file:///nowhere/main.lib.tengo".into(), src.into(), &mut parser)
                .unwrap();
        let col = src.lines().nth(1).unwrap().find("add").unwrap() as u32;
        assert!(hover(&state, Position::new(1, col)).is_none());
    }
}

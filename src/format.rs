//! Tengo source formatter.
//!
//! Reimplements the rules of the reference Emacs/go-mode formatter
//! (`format.el`): **indentation only**, not a full pretty-printer. It:
//!   - strips trailing whitespace from every line,
//!   - reindents each line with **tabs**, one level per enclosing multi-line
//!     bracket container (`{}` `[]` `()`),
//!   - normalizes the file to a single trailing newline.
//!
//! It deliberately does NOT reflow code, normalize intra-line spacing, align
//! fields, or manage blank lines — matching what `indent-region` does.
//!
//! Note: LSP `FormattingOptions.insert_spaces`/`tab_size` are intentionally
//! ignored — the reference mandates tabs.

use tree_sitter::{Node, Parser, Tree};

/// Format `source`, parsing it fresh. Returns the source unchanged if it can't
/// be parsed or contains syntax errors (never reindent broken input).
pub fn format(source: &str) -> String {
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_tengo::LANGUAGE.into())
        .is_err()
    {
        return source.to_string();
    }
    match parser.parse(source, None) {
        Some(tree) => format_with_tree(source, &tree),
        None => source.to_string(),
    }
}

/// Format `source` given its already-parsed `tree` (the LSP path — reuses the
/// document's tree). Bails out unchanged if the tree has any error nodes.
pub fn format_with_tree(source: &str, tree: &Tree) -> String {
    let root = tree.root_node();
    if root.has_error() {
        return source.to_string();
    }

    let bytes = source.as_bytes();
    // Byte offset of the start of each line.
    let mut line_starts = vec![0usize];
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'\n' {
            line_starts.push(i + 1);
        }
    }

    // Preserve the file's newline style instead of forcing LF (so a CRLF file
    // stays CRLF — we only touch indentation/trailing whitespace).
    let nl = if source.contains("\r\n") { "\r\n" } else { "\n" };

    // Open/close delimiter tokens in source order: (byte_offset, +1 | -1).
    // Counting tokens (not named nodes) is robust to grammars that park the
    // parens on a parent node rather than on argument_list/parameter_list.
    let mut delims: Vec<(usize, i32)> = Vec::new();
    collect_delimiters(root, &mut delims);
    // A multi-line builder/fluent chain (leading-dot continuation) contributes
    // one *virtual* bracket spanning from the end of its first line to its end,
    // so every continuation line — and anything nested in a chained call's args
    // — indents one level under the chain start.
    let mut seen = std::collections::HashSet::new();
    collect_chain_indents(root, bytes, &line_starts, &mut seen, &mut delims);
    delims.sort_unstable_by_key(|&(off, _)| off);

    // Bucket each delimiter's ±1 onto the row it sits on, preserving offset
    // (= source) order within the row. `delims` is already sorted by offset.
    let num_rows = line_starts.len();
    let mut row_deltas: Vec<Vec<i32>> = vec![Vec::new(); num_rows];
    for &(off, d) in &delims {
        let row = line_starts.partition_point(|&s| s <= off) - 1;
        row_deltas[row].push(d);
    }

    // Line-based indentation (gofmt-style), not per-bracket. Several brackets
    // opening on one line (`foo({`) add a SINGLE level; the matching closers
    // remove a single level. `raw_depth` is the true bracket-nesting count;
    // `stack` holds, for each open indent level, the raw depth just *below* the
    // scope that introduced it ("threshold"). A line's indent is the number of
    // thresholds strictly below its depth — robust to the gaps that collapsing
    // consecutive opens creates (`({` yields thresholds like [0, 2]).
    let mut lines: Vec<String> = Vec::with_capacity(num_rows);
    let mut raw_depth: i32 = 0;
    let mut stack: Vec<i32> = Vec::new();
    for (row, &start) in line_starts.iter().enumerate() {
        let end = line_starts.get(row + 1).map_or(bytes.len(), |&s| s - 1); // exclude '\n'
        let content = source[start..end].trim(); // strips leading/trailing ws incl. a trailing '\r'

        if content.is_empty() {
            lines.push(String::new());
        } else {
            // A line that opens with closers aligns one level out per leading
            // closer, to sit with the opener line.
            let lead_closers = content
                .chars()
                .take_while(|&c| matches!(c, '}' | ']' | ')'))
                .count() as i32;
            let lead_depth = (raw_depth - lead_closers).max(0);
            let depth = stack.iter().filter(|&&t| t < lead_depth).count();
            let mut rendered = "\t".repeat(depth);
            rendered.push_str(content);
            lines.push(rendered);
        }

        // Advance bracket state for the following lines. A line introduces at
        // most one new level — at the lowest depth it reaches (`min_after`),
        // so a line that closes then reopens (`} else {`) still indents its
        // body. Closers pop every level opened at or above the new depth.
        let mut min_after = raw_depth;
        for &d in &row_deltas[row] {
            raw_depth += d;
            if d < 0 {
                while stack.last().is_some_and(|&t| t >= raw_depth) {
                    stack.pop();
                }
            }
            if raw_depth < min_after {
                min_after = raw_depth;
            }
        }
        if raw_depth > min_after && stack.last() != Some(&min_after) {
            stack.push(min_after);
        }
    }

    // Collapse runs of blank lines to a single blank line (gofmt-style), and
    // drop blank lines at the very start of the file.
    let mut collapsed: Vec<String> = Vec::with_capacity(lines.len());
    for line in lines {
        if line.is_empty() && collapsed.last().map_or(true, |l: &String| l.is_empty()) {
            continue;
        }
        collapsed.push(line);
    }
    let mut lines = collapsed;

    // Drop trailing blank lines, then join with one trailing newline (none if
    // there's no content at all).
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        return String::new();
    }
    let mut out = lines.join(nl);
    out.push_str(nl);
    out
}

/// Collect open/close bracket tokens (`{} [] ()`) as (start_byte, ±1).
/// Tokens inside strings/comments aren't delimiter nodes, so they're ignored.
fn collect_delimiters(node: Node, out: &mut Vec<(usize, i32)>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "{" | "[" | "(" => out.push((child.start_byte(), 1)),
            "}" | "]" | ")" => out.push((child.start_byte(), -1)),
            _ => {}
        }
        collect_delimiters(child, out);
    }
}

/// Find multi-line method chains and emit a virtual bracket pair for each.
/// A chain is detected by a `.` token sitting at a line boundary — either the
/// first non-whitespace on its line (leading-dot style, `.bar()`) or the last
/// (trailing-dot style, `foo().`). Its root is the outermost enclosing
/// call/selector expression. The virtual open sits at the newline ending the
/// chain's first line; the close at the chain's end byte, so every
/// continuation line indents one level under the chain start.
fn collect_chain_indents(
    node: Node,
    bytes: &[u8],
    line_starts: &[usize],
    seen: &mut std::collections::HashSet<(usize, usize)>,
    out: &mut Vec<(usize, i32)>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "." {
            let row = child.start_position().row;
            let line_start = line_starts[row];
            let line_end = line_starts.get(row + 1).map_or(bytes.len(), |&s| s - 1);
            let is_leading = bytes[line_start..child.start_byte()]
                .iter()
                .all(|b| *b == b' ' || *b == b'\t');
            let is_trailing = bytes[child.end_byte()..line_end]
                .iter()
                .all(|b| *b == b' ' || *b == b'\t' || *b == b'\r');
            if is_leading || is_trailing {
                let mut chain_root = child.parent().unwrap_or(child);
                while let Some(p) = chain_root.parent() {
                    if matches!(p.kind(), "call_expression" | "selector_expression") {
                        chain_root = p;
                    } else {
                        break;
                    }
                }
                let r0 = chain_root.start_position().row;
                let key = (chain_root.start_byte(), chain_root.end_byte());
                if r0 + 1 < line_starts.len() && seen.insert(key) {
                    out.push((line_starts[r0 + 1] - 1, 1)); // newline of first line
                    out.push((chain_root.end_byte(), -1));
                }
            }
        }
        collect_chain_indents(child, bytes, line_starts, seen, out);
    }
}

/// CLI entry point for `tengo-lsp fmt [--write] [files...]`.
///
/// With no files, formats stdin to stdout. With files, prints formatted output
/// to stdout, or rewrites in place with `--write`. Returns a process exit code.
pub fn run_cli(args: &[String]) -> i32 {
    use std::io::{Read, Write};

    let mut write = false;
    let mut files: Vec<&String> = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--write" | "-w" => write = true,
            "--help" | "-h" => {
                eprintln!("usage: tengo-lsp fmt [--write] [files...]");
                return 0;
            }
            _ => files.push(arg),
        }
    }

    if files.is_empty() {
        let mut src = String::new();
        if std::io::stdin().read_to_string(&mut src).is_err() {
            eprintln!("tengo-lsp fmt: failed to read stdin");
            return 1;
        }
        print!("{}", format(&src));
        return 0;
    }

    let mut code = 0;
    for file in files {
        let src = match std::fs::read_to_string(file) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("tengo-lsp fmt: {}: {}", file, e);
                code = 1;
                continue;
            }
        };
        let formatted = format(&src);
        if write {
            if formatted == src {
                continue;
            }
            if let Err(e) = std::fs::write(file, &formatted) {
                eprintln!("tengo-lsp fmt: {}: {}", file, e);
                code = 1;
            }
        } else {
            let _ = std::io::stdout().write_all(formatted.as_bytes());
        }
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Canonical, correctly-formatted sample (tab-indented), with comments in
    /// the spots most likely to expose tree/indent divergence.
    const CANONICAL: &str = "\
// top-level comment
m := {
\ta: 1,
\t// own-line comment inside map
\tb: [
\t\t1,
\t\t2,
\t],
\tc: func(x, y) {
\t\tif x {
\t\t\treturn y // trailing comment
\t\t}
\t\tfor i := 0; i < 10; i++ {
\t\t\tx += i
\t\t}
\t\t// comment before closing brace
\t\treturn x
\t},
}
result := compute(
\ta,
\tb,
)
";

    #[test]
    fn formats_canonical_unchanged() {
        // Identity check: well-formatted input must survive untouched.
        assert_eq!(format(CANONICAL), CANONICAL);
    }

    #[test]
    fn reindents_mangled_input() {
        // The discriminating test: strip ALL leading whitespace, then format.
        // Passes only if depth is actually computed (a no-op would fail here).
        let mangled: String = CANONICAL
            .lines()
            .map(|l| format!("{}\n", l.trim_start()))
            .collect();
        assert_ne!(mangled, CANONICAL, "fixture must actually be indented");
        assert_eq!(format(&mangled), CANONICAL);
    }

    #[test]
    fn reindents_overindented_input() {
        // Prepend noise indentation to every non-blank line.
        let mangled: String = CANONICAL
            .lines()
            .map(|l| {
                if l.trim().is_empty() {
                    "\n".to_string()
                } else {
                    format!("\t\t\t  {}\n", l.trim_start())
                }
            })
            .collect();
        assert_eq!(format(&mangled), CANONICAL);
    }

    #[test]
    fn idempotent() {
        let once = format(CANONICAL);
        assert_eq!(format(&once), once);
    }

    #[test]
    fn strips_trailing_whitespace() {
        assert_eq!(format("x := 1   \n"), "x := 1\n");
    }

    #[test]
    fn normalizes_trailing_newline() {
        assert_eq!(format("x := 1\n\n\n"), "x := 1\n");
        assert_eq!(format("x := 1"), "x := 1\n");
    }

    #[test]
    fn leaves_broken_input_untouched() {
        let broken = "m := {\n  a: 1,\n"; // unclosed brace -> ERROR node
        assert_eq!(format(broken), broken);
    }

    #[test]
    fn empty_input() {
        assert_eq!(format(""), "");
    }

    #[test]
    fn formats_chain_with_multiline_args() {
        // The gap fix: args of a chained call indent under the continuation
        // (chain +1) *and* the open paren (+1), and the closer aligns with its
        // `.bar(` line.
        let mangled = "x := foo()\n.bar(\na,\nb,\n)\n.baz()\n";
        let want = "x := foo()\n\t.bar(\n\t\ta,\n\t\tb,\n\t)\n\t.baz()\n";
        assert_eq!(format(mangled), want);
        assert_eq!(format(&want), want, "must be idempotent");
    }

    #[test]
    fn formats_trailing_dot_builder_chain() {
        // Trailing-dot style: each line ENDS with `.`. Continuation lines
        // indent one level under the chain start.
        let mangled = "wd := workdir.builder().\ninQueue(q).\ncpu(c).\nbuild()\n";
        let want = "wd := workdir.builder().\n\tinQueue(q).\n\tcpu(c).\n\tbuild()\n";
        assert_eq!(format(mangled), want);
        assert_eq!(format(&want), want, "must be idempotent");
    }

    #[test]
    fn formats_trailing_dot_chain_inside_block() {
        let mangled = "f := func() {\nwd := b().\nwith(x).\nbuild()\n}\n";
        let want = "f := func() {\n\twd := b().\n\t\twith(x).\n\t\tbuild()\n}\n";
        assert_eq!(format(mangled), want);
        assert_eq!(format(&want), want, "must be idempotent");
    }

    #[test]
    fn formats_leading_dot_builder_chain() {
        // Builder/fluent pattern: the dot leads each continuation line, which
        // is indented one level under the start of the chain.
        let mangled = "x := foo()\n.bar()\n.baz()\n";
        let want = "x := foo()\n\t.bar()\n\t.baz()\n";
        assert_eq!(format(mangled), want);
    }

    #[test]
    fn formats_builder_chain_inside_block() {
        // The chain's base indent is its enclosing block, continuations +1.
        let mangled = "f := func() {\nreturn b()\n.with(x)\n.build()\n}\n";
        let want = "f := func() {\n\treturn b()\n\t\t.with(x)\n\t\t.build()\n}\n";
        assert_eq!(format(mangled), want);
    }

    #[test]
    fn reindents_misindented_imports() {
        // Imports are top-level statements -> depth 0; noise indent removed.
        let mangled = "\t\ta := import(\"x\")\n  b := import(\"y\")\n";
        let want = "a := import(\"x\")\nb := import(\"y\")\n";
        assert_eq!(format(mangled), want);
    }

    #[test]
    fn several_imports_stay_flat() {
        let src = "a := import(\"x\")\nb := import(\"y\")\nc := import(\"z\")\n";
        assert_eq!(format(src), src);
    }

    #[test]
    fn formats_export_section() {
        // `export { ... }` wraps a map_literal: entries get one tab, the
        // closing brace dedents to column 0.
        let mangled = "export {\nfoo: foo,\nbar: func(x) {\nreturn x\n},\nbaz: baz,\n}\n";
        let want =
            "export {\n\tfoo: foo,\n\tbar: func(x) {\n\t\treturn x\n\t},\n\tbaz: baz,\n}\n";
        assert_eq!(format(mangled), want);
    }

    #[test]
    fn formats_multiline_call_args() {
        // Each arg line indents one level; nested call adds another; closers
        // dedent to their opener.
        let mangled = "r := outer(\ninner(\na,\nb,\n),\nc,\n)\n";
        let want = "r := outer(\n\tinner(\n\t\ta,\n\t\tb,\n\t),\n\tc,\n)\n";
        assert_eq!(format(mangled), want);
    }

    #[test]
    fn collapses_runs_of_blank_lines() {
        // Multiple consecutive blanks collapse to one; a single blank survives;
        // leading blanks are dropped.
        assert_eq!(format("\n\n\na := 1\n\n\n\nb := 2\n"), "a := 1\n\nb := 2\n");
        assert_eq!(format("a := 1\n\nb := 2\n"), "a := 1\n\nb := 2\n");
        // Inside a block too.
        let mangled = "m := {\na: 1,\n\n\n\nb: 2,\n}\n";
        let want = "m := {\n\ta: 1,\n\n\tb: 2,\n}\n";
        assert_eq!(format(mangled), want);
        assert_eq!(format(&want), want, "must be idempotent");
    }

    #[test]
    fn collapses_consecutive_opens_to_one_level() {
        // The bug fix: `(` and `{` opening on the SAME line (a map literal as
        // the sole call arg, `ll.toStrict({`) add ONE indent level, not two.
        let mangled = "self = call({\na: 1,\nb: func(x) {\nreturn x\n},\n})\n";
        let want = "self = call({\n\ta: 1,\n\tb: func(x) {\n\t\treturn x\n\t},\n})\n";
        assert_eq!(format(mangled), want);
        assert_eq!(format(&want), want, "must be idempotent");
    }

    #[test]
    fn collapses_triple_close() {
        // Three closers on one line (`}))`) dedent a single level, matching the
        // three opens that were collapsed on `h(use({`.
        let mangled = "x := h(use({\na: 1,\n}))\n";
        let want = "x := h(use({\n\ta: 1,\n}))\n";
        assert_eq!(format(mangled), want);
        assert_eq!(format(&want), want, "must be idempotent");
    }

    #[test]
    fn close_then_reopen_keeps_indent() {
        // `} else {` closes a block and opens a new one on one line; the new
        // block's body must still indent (the min_after rule).
        let mangled = "if x {\na\n} else {\nb\n}\n";
        let want = "if x {\n\ta\n} else {\n\tb\n}\n";
        assert_eq!(format(mangled), want);
        assert_eq!(format(&want), want, "must be idempotent");
    }

    #[test]
    fn preserves_crlf_line_endings() {
        // Indentation-only: a CRLF file must stay CRLF, not silently become LF.
        let mangled = "m := {\r\na: 1,\r\n}\r\n";
        let want = "m := {\r\n\ta: 1,\r\n}\r\n";
        assert_eq!(format(mangled), want);
    }

    #[test]
    fn format_with_tree_reuses_parsed_tree() {
        // Exercises the LSP path directly (the server passes its cached tree).
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_tengo::LANGUAGE.into())
            .unwrap();
        let mangled = "m := {\na: 1,\n}\n";
        let tree = parser.parse(mangled, None).unwrap();
        assert_eq!(format_with_tree(mangled, &tree), "m := {\n\ta: 1,\n}\n");
    }

    #[test]
    fn braces_in_strings_and_comments_dont_indent() {
        // Verifies the collect_delimiters assumption: a brace inside a string
        // or comment is not a delimiter token, so it must not shift depth.
        let src = "s := \"}\"\ny := 1\n// a } in a comment\nz := 2\n";
        assert_eq!(format(src), src);
    }
}

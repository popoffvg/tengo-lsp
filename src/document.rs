use tree_sitter::{Parser, Tree};

use crate::scope::Scope;
use crate::symbols::{ImportInfo, Symbol, SymbolRef};

pub struct FileState {
    pub uri: String,
    pub source: String,
    pub tree: Tree,
    pub defs: Vec<Symbol>,
    pub refs: Vec<SymbolRef>,
    pub imports: std::collections::HashMap<String, ImportInfo>,
    pub scopes: Vec<Scope>,
}

impl FileState {
    pub fn parse(uri: String, source: String, parser: &mut Parser) -> Option<Self> {
        let tree = parser.parse(&source, None)?;
        let mut state = FileState {
            uri,
            source,
            tree,
            defs: Vec::new(),
            refs: Vec::new(),
            imports: std::collections::HashMap::new(),
            scopes: Vec::new(),
        };
        state.analyze();
        Some(state)
    }

    pub fn reparse(&mut self, source: String, parser: &mut Parser) {
        if let Some(tree) = parser.parse(&source, None) {
            self.source = source;
            self.tree = tree;
            self.defs.clear();
            self.refs.clear();
            self.imports.clear();
            self.scopes.clear();
            self.analyze();
        }
    }

    fn analyze(&mut self) {
        crate::scope::build_scopes(&self.tree, &mut self.scopes);
        crate::symbols::extract_symbols(
            &self.tree,
            self.source.as_bytes(),
            &self.scopes,
            &mut self.defs,
            &mut self.refs,
            &mut self.imports,
        );
    }

    /// Find the innermost scope containing the given byte offset.
    pub fn scope_at(&self, byte: usize) -> Option<usize> {
        let mut best: Option<usize> = None;
        for (i, scope) in self.scopes.iter().enumerate() {
            if byte >= scope.start_byte && byte < scope.end_byte {
                match best {
                    None => best = Some(i),
                    Some(prev) => {
                        let prev_scope = &self.scopes[prev];
                        let size = scope.end_byte - scope.start_byte;
                        let prev_size = prev_scope.end_byte - prev_scope.start_byte;
                        if size < prev_size {
                            best = Some(i);
                        }
                    }
                }
            }
        }
        best
    }

    /// Resolve a definition for `name` visible at `byte_offset`, walking scope chain.
    pub fn resolve_def(&self, name: &str, byte_offset: usize) -> Option<&Symbol> {
        let mut scope_id = self.scope_at(byte_offset);
        while let Some(sid) = scope_id {
            for def in &self.defs {
                if def.name == name
                    && def.scope_id == sid
                    && def.byte_range.start <= byte_offset
                {
                    return Some(def);
                }
            }
            scope_id = self.scopes[sid].parent;
        }
        None
    }
}

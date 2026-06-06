use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use dashmap::DashMap;
use tower_lsp::jsonrpc::{Error, Result};
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};
use tree_sitter::Parser;

use crate::completion as completion_impl;
use crate::definition;
use crate::document::FileState;
use crate::hover as hover_impl;
use crate::outline;
use crate::references;
use crate::rename as rename_impl;

pub struct Backend {
    client: Client,
    documents: DashMap<String, FileState>,
    parser: Mutex<Parser>,
    workspace_roots: Mutex<Vec<std::path::PathBuf>>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_tengo::LANGUAGE.into())
            .expect("failed to set tengo language");
        Backend {
            client,
            documents: DashMap::new(),
            parser: Mutex::new(parser),
            workspace_roots: Mutex::new(Vec::new()),
        }
    }

    fn parse_and_store(&self, uri: String, text: String) {
        let mut parser = self.parser.lock().unwrap();
        if let Some(state) = FileState::parse(uri.clone(), text, &mut parser) {
            self.documents.insert(uri, state);
        }
    }

    /// Snapshot the live source of every open document, keyed by canonical
    /// path. Cross-file scans use this so they read unsaved editor buffers
    /// instead of stale disk content. Materialised up-front (not a closure over
    /// `self.documents`) to avoid re-entrant DashMap access during a scan.
    fn source_overlay(&self) -> HashMap<PathBuf, String> {
        let mut map = HashMap::new();
        for entry in self.documents.iter() {
            if let Ok(url) = Url::parse(entry.key()) {
                if let Ok(path) = url.to_file_path() {
                    let key = path.canonicalize().unwrap_or(path);
                    map.insert(key, entry.value().source.clone());
                }
            }
        }
        map
    }
}

/// Build an overlay closure that resolves a path to its in-memory source, if open.
fn overlay_lookup(map: &HashMap<PathBuf, String>) -> impl Fn(&Path) -> Option<String> + '_ {
    move |path: &Path| {
        let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        map.get(&key).cloned()
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        // Capture workspace roots so references can scan the whole workspace.
        let mut roots: Vec<std::path::PathBuf> = Vec::new();
        if let Some(folders) = &params.workspace_folders {
            for f in folders {
                if let Ok(p) = f.uri.to_file_path() {
                    roots.push(p);
                }
            }
        }
        #[allow(deprecated)]
        if roots.is_empty() {
            if let Some(root_uri) = &params.root_uri {
                if let Ok(p) = root_uri.to_file_path() {
                    roots.push(p);
                }
            }
        }
        *self.workspace_roots.lock().unwrap() = roots;

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![".".into()]),
                    ..Default::default()
                }),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: Default::default(),
                })),
                document_formatting_provider: Some(OneOf::Left(true)),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        log::info!("tengo-lsp initialized");
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        let text = params.text_document.text;
        self.parse_and_store(uri, text);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        if let Some(change) = params.content_changes.into_iter().last() {
            // Full sync — we get the entire document text
            if let Some(mut state) = self.documents.get_mut(&uri) {
                let mut parser = self.parser.lock().unwrap();
                state.reparse(change.text, &mut parser);
            } else {
                self.parse_and_store(uri, change.text);
            }
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        self.documents.remove(&uri);
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = params.text_document_position_params.text_document.uri.to_string();
        let position = params.text_document_position_params.position;

        let result = self
            .documents
            .get(&uri)
            .and_then(|state| definition::goto_definition(&state, position));

        Ok(result)
    }

    async fn completion(
        &self,
        params: CompletionParams,
    ) -> Result<Option<CompletionResponse>> {
        let uri = params
            .text_document_position
            .text_document
            .uri
            .to_string();
        let position = params.text_document_position.position;

        let result = self
            .documents
            .get(&uri)
            .and_then(|state| completion_impl::completion(&state, position));

        Ok(result)
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .to_string();
        let position = params.text_document_position_params.position;

        let result = self
            .documents
            .get(&uri)
            .and_then(|state| hover_impl::hover(&state, position));

        Ok(result)
    }

    async fn references(
        &self,
        params: ReferenceParams,
    ) -> Result<Option<Vec<Location>>> {
        let uri = params.text_document_position.text_document.uri.to_string();
        let position = params.text_document_position.position;
        let include_decl = params.context.include_declaration;

        let roots = self.workspace_roots.lock().unwrap().clone();
        let overlay_map = self.source_overlay();
        let overlay = overlay_lookup(&overlay_map);
        let result = self.documents.get(&uri).and_then(|state| {
            references::find_references(&state, position, include_decl, &roots, &self.parser, &overlay)
        });

        Ok(result)
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let uri = params.text_document_position.text_document.uri.to_string();
        let position = params.text_document_position.position;
        let new_name = params.new_name;

        let roots = self.workspace_roots.lock().unwrap().clone();
        let overlay_map = self.source_overlay();
        let overlay = overlay_lookup(&overlay_map);

        let result = self.documents.get(&uri).map(|state| {
            rename_impl::rename(&state, position, &new_name, &roots, &self.parser, &overlay)
        });

        match result {
            Some(Ok(edit)) => Ok(Some(edit)),
            Some(Err(rename_impl::RenameError::InvalidName(msg))) => {
                Err(Error::invalid_params(msg))
            }
            // NotRenameable / unknown document: no edit (client shows "cannot rename").
            Some(Err(rename_impl::RenameError::NotRenameable)) | None => Ok(None),
        }
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let uri = params.text_document.uri.to_string();
        let position = params.position;
        let result = self
            .documents
            .get(&uri)
            .and_then(|state| rename_impl::prepare_rename(&state, position))
            .map(PrepareRenameResponse::Range);
        Ok(result)
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri = params.text_document.uri.to_string();
        let result = self
            .documents
            .get(&uri)
            .and_then(|state| outline::document_symbols(&state));
        Ok(result)
    }

    async fn formatting(
        &self,
        params: DocumentFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        // FormattingOptions (insert_spaces/tab_size) are intentionally ignored:
        // the Tengo style mandates tabs.
        let uri = params.text_document.uri.to_string();
        let result = self.documents.get(&uri).map(|state| {
            let formatted = crate::format::format_with_tree(&state.source, &state.tree);
            if formatted == state.source {
                Vec::new()
            } else {
                vec![TextEdit {
                    range: full_document_range(&state.source),
                    new_text: formatted,
                }]
            }
        });
        Ok(result)
    }
}

/// A range covering the entire document, in LSP (UTF-16, line/character) terms.
fn full_document_range(source: &str) -> Range {
    let mut last_line = 0u32;
    let mut last_col = 0u32;
    for ch in source.chars() {
        if ch == '\n' {
            last_line += 1;
            last_col = 0;
        } else {
            last_col += ch.len_utf16() as u32;
        }
    }
    Range {
        start: Position::new(0, 0),
        end: Position::new(last_line, last_col),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_range_empty() {
        let r = full_document_range("");
        assert_eq!(r.end, Position::new(0, 0));
    }

    #[test]
    fn full_range_single_line_no_newline() {
        let r = full_document_range("x := 1");
        assert_eq!(r.end, Position::new(0, 6));
    }

    #[test]
    fn full_range_trailing_newline() {
        // End sits at the start of the empty final line.
        let r = full_document_range("a\nbb\n");
        assert_eq!(r.end, Position::new(2, 0));
    }

    #[test]
    fn full_range_counts_utf16_units() {
        // "café" — 'é' is one UTF-16 unit; "x🦀" — crab is a surrogate pair (2).
        assert_eq!(full_document_range("café").end, Position::new(0, 4));
        assert_eq!(full_document_range("x🦀").end, Position::new(0, 3));
    }
}

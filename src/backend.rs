use std::sync::Mutex;

use dashmap::DashMap;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};
use tree_sitter::Parser;

use crate::completion as completion_impl;
use crate::definition;
use crate::document::FileState;
use crate::hover as hover_impl;
use crate::references;

pub struct Backend {
    client: Client,
    documents: DashMap<String, FileState>,
    parser: Mutex<Parser>,
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
        }
    }

    fn parse_and_store(&self, uri: String, text: String) {
        let mut parser = self.parser.lock().unwrap();
        if let Some(state) = FileState::parse(uri.clone(), text, &mut parser) {
            self.documents.insert(uri, state);
        }
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
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

        let result = self
            .documents
            .get(&uri)
            .and_then(|state| references::find_references(&state, position, include_decl));

        Ok(result)
    }
}

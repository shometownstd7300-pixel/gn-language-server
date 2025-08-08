// Copyright 2025 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{
    collections::{btree_map::Entry, BTreeMap},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::Instant,
};

use tokio::spawn;
use tower_lsp::{
    lsp_types::{
        CompletionOptions, CompletionParams, CompletionResponse, DidChangeConfigurationParams,
        DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
        DocumentFormattingParams, DocumentLink, DocumentLinkOptions, DocumentLinkParams,
        DocumentSymbolParams, DocumentSymbolResponse, GotoDefinitionParams, GotoDefinitionResponse,
        Hover, HoverParams, HoverProviderCapability, InitializeParams, InitializeResult,
        InitializedParams, Location, MessageType, OneOf, ReferenceParams, ServerCapabilities,
        TextDocumentSyncCapability, TextDocumentSyncKind, TextEdit, Url,
    },
    LanguageServer, LspService, Server,
};

use crate::{
    analyze::Analyzer,
    client::TestableClient,
    error::RpcResult,
    storage::DocumentStorage,
    utils::{find_nearest_workspace_root, walk_source_dirs, AsyncSignal},
};

struct ServerContext {
    pub storage: Arc<Mutex<DocumentStorage>>,
    pub analyzer: Arc<Analyzer>,
    pub initialize_params: Arc<OnceLock<InitializeParams>>,
    pub indexed: Arc<Mutex<BTreeMap<PathBuf, AsyncSignal>>>,
    pub client: TestableClient,
}

impl ServerContext {
    #[cfg(test)]
    pub fn new_for_testing() -> Self {
        let storage = Arc::new(Mutex::new(DocumentStorage::new()));
        let analyzer = Arc::new(Analyzer::new(&storage));
        Self {
            storage,
            analyzer,
            initialize_params: Default::default(),
            indexed: Default::default(),
            client: TestableClient::new_for_testing(),
        }
    }

    pub fn request(&self) -> RequestContext {
        RequestContext {
            storage: self.storage.clone(),
            analyzer: self.analyzer.clone(),
            initialize_params: self.initialize_params.clone(),
            indexed: self.indexed.clone(),
            client: self.client.clone(),
            request_time: Instant::now(),
        }
    }
}

#[derive(Clone)]
pub struct RequestContext {
    pub storage: Arc<Mutex<DocumentStorage>>,
    pub analyzer: Arc<Analyzer>,
    pub initialize_params: Arc<OnceLock<InitializeParams>>,
    pub indexed: Arc<Mutex<BTreeMap<PathBuf, AsyncSignal>>>,
    pub client: TestableClient,
    pub request_time: Instant,
}

impl RequestContext {
    #[cfg(test)]
    pub fn new_for_testing() -> Self {
        ServerContext::new_for_testing().request()
    }
}

struct Backend {
    context: ServerContext,
}

impl Backend {
    pub fn new(
        storage: &Arc<Mutex<DocumentStorage>>,
        analyzer: &Arc<Analyzer>,
        client: TestableClient,
    ) -> Self {
        Self {
            context: ServerContext {
                storage: storage.clone(),
                analyzer: analyzer.clone(),
                initialize_params: Default::default(),
                indexed: Default::default(),
                client,
            },
        }
    }

    async fn maybe_index_workspace_for(
        &self,
        context: &RequestContext,
        path: &Path,
        parallel_indexing: bool,
    ) {
        let Ok(workspace_root) = find_nearest_workspace_root(path) else {
            return;
        };
        let workspace_root = workspace_root.to_path_buf();

        let mut indexed = match context
            .indexed
            .lock()
            .unwrap()
            .entry(workspace_root.to_path_buf())
        {
            Entry::Occupied(_) => return,
            Entry::Vacant(entry) => entry.insert(AsyncSignal::new()).clone(),
        };

        let context = context.clone();
        spawn(async move {
            crate::providers::indexing::index(&context, &workspace_root, parallel_indexing).await;
            indexed.set();
        });
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> RpcResult<InitializeResult> {
        self.context.initialize_params.set(params).unwrap();
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                definition_provider: Some(OneOf::Left(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                document_link_provider: Some(DocumentLinkOptions {
                    resolve_provider: Some(true),
                    work_done_progress_options: Default::default(),
                }),
                document_symbol_provider: Some(OneOf::Left(true)),
                completion_provider: Some(CompletionOptions::default()),
                document_formatting_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _params: InitializedParams) {
        let context = self.context.request();
        context
            .client
            .log_message(MessageType::INFO, "GN language server initialized")
            .await;

        let configurations = self.context.client.configurations().await;
        if !configurations.background_indexing {
            return;
        }

        let initialize_params = context.initialize_params.get().unwrap();
        if let Some(root_uri) = &initialize_params.root_uri {
            if let Ok(root_path) = root_uri.to_file_path() {
                self.maybe_index_workspace_for(
                    &context,
                    &root_path,
                    configurations.experimental.parallel_indexing,
                )
                .await;
                for workspace_root in
                    walk_source_dirs(&root_path).filter(|path| path.join(".gn").exists())
                {
                    self.maybe_index_workspace_for(
                        &context,
                        &workspace_root,
                        configurations.experimental.parallel_indexing,
                    )
                    .await;
                }
            }
        }
    }

    async fn shutdown(&self) -> RpcResult<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let context = self.context.request();
        let Ok(path) = Url::to_file_path(&params.text_document.uri) else {
            return;
        };
        let configurations = self.context.client.configurations().await;
        if configurations.background_indexing {
            self.maybe_index_workspace_for(
                &context,
                &path,
                configurations.experimental.parallel_indexing,
            )
            .await;
        }
        crate::providers::document::did_open(&self.context.request(), params).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        crate::providers::document::did_change(&self.context.request(), params).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        crate::providers::document::did_close(&self.context.request(), params).await;
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        crate::providers::configuration::did_change_configuration(&self.context.request(), params)
            .await;
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> RpcResult<Option<GotoDefinitionResponse>> {
        Ok(
            crate::providers::goto_definition::goto_definition(&self.context.request(), params)
                .await?,
        )
    }

    async fn hover(&self, params: HoverParams) -> RpcResult<Option<Hover>> {
        Ok(crate::providers::hover::hover(&self.context.request(), params).await?)
    }

    async fn document_link(
        &self,
        params: DocumentLinkParams,
    ) -> RpcResult<Option<Vec<DocumentLink>>> {
        Ok(crate::providers::document_link::document_link(&self.context.request(), params).await?)
    }

    async fn document_link_resolve(&self, link: DocumentLink) -> RpcResult<DocumentLink> {
        Ok(
            crate::providers::document_link::document_link_resolve(&self.context.request(), link)
                .await?,
        )
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> RpcResult<Option<DocumentSymbolResponse>> {
        Ok(
            crate::providers::document_symbol::document_symbol(&self.context.request(), params)
                .await?,
        )
    }

    async fn completion(&self, params: CompletionParams) -> RpcResult<Option<CompletionResponse>> {
        Ok(crate::providers::completion::completion(&self.context.request(), params).await?)
    }

    async fn references(&self, params: ReferenceParams) -> RpcResult<Option<Vec<Location>>> {
        Ok(crate::providers::references::references(&self.context.request(), params).await?)
    }

    async fn formatting(
        &self,
        params: DocumentFormattingParams,
    ) -> RpcResult<Option<Vec<TextEdit>>> {
        Ok(crate::providers::formatting::formatting(&self.context.request(), params).await?)
    }
}

pub async fn run() {
    let storage = Arc::new(Mutex::new(DocumentStorage::new()));
    let analyzer = Arc::new(Analyzer::new(&storage));
    let (service, socket) = LspService::new(move |client| {
        Backend::new(&storage, &analyzer, TestableClient::new(client))
    });

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    Server::new(stdin, stdout, socket).serve(service).await;
}

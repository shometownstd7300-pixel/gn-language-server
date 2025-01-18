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

use std::{borrow::Cow, path::PathBuf};

use itertools::Itertools;
use tokio::sync::Mutex;
use tower_lsp::{lsp_types::*, Client, LanguageServer, LspService, Server};

use crate::{
    analyze::{AnalyzedFile, Analyzer, Link},
    ast::{Identifier, Node},
    builtins::BUILTINS,
    storage::DocumentStorage,
};

type RpcResult<T> = tower_lsp::jsonrpc::Result<T>;

fn into_rpc_error(err: std::io::Error) -> tower_lsp::jsonrpc::Error {
    let mut rpc_err = tower_lsp::jsonrpc::Error::internal_error();
    rpc_err.message = Cow::from(err.to_string());
    rpc_err
}

fn lookup_identifier_at(file: &AnalyzedFile, position: Position) -> Option<&Identifier> {
    let offset = file.document.line_index.offset(position)?;
    file.ast_root
        .identifiers()
        .find(|ident| ident.span.start() <= offset && offset <= ident.span.end())
}

#[derive(serde::Serialize, serde::Deserialize)]
struct TargetLinkData {
    path: PathBuf,
    name: String,
}

struct Backend {
    analyzer: Mutex<Analyzer>,
    client: Client,
}

impl Backend {
    pub fn new(analyzer: Analyzer, client: Client) -> Self {
        Self {
            analyzer: Mutex::new(analyzer),
            client,
        }
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> RpcResult<InitializeResult> {
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
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _params: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "GN language server initialized")
            .await;
    }

    async fn shutdown(&self) -> RpcResult<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let Ok(path) = params.text_document.uri.to_file_path() else {
            return;
        };

        self.analyzer.lock().await.storage_mut().load_to_memory(
            &path,
            &params.text_document.text,
            params.text_document.version,
        );

        // let file = self.parser.lock().await.analyze(&path).unwrap().clone();
        // self.client.log_message(MessageType::INFO, format!("{:?}", file.root)).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let Ok(path) = params.text_document.uri.to_file_path() else {
            return;
        };
        let Some(change) = params.content_changes.first() else {
            return;
        };

        self.analyzer.lock().await.storage_mut().load_to_memory(
            &path,
            &change.text,
            params.text_document.version,
        );
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let Ok(path) = params.text_document.uri.to_file_path() else {
            return;
        };

        self.analyzer
            .lock()
            .await
            .storage_mut()
            .unload_from_memory(&path);
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> RpcResult<Option<GotoDefinitionResponse>> {
        let Ok(path) = params
            .text_document_position_params
            .text_document
            .uri
            .to_file_path()
        else {
            return Ok(None);
        };

        let current_file = self
            .analyzer
            .lock()
            .await
            .analyze(&path)
            .map_err(into_rpc_error)?;

        let Some(ident) =
            lookup_identifier_at(&current_file, params.text_document_position_params.position)
        else {
            return Ok(None);
        };

        let links = current_file
            .templates_at(ident.span.start())
            .into_iter()
            .filter(|template| template.name == ident.name)
            .map(|template| LocationLink {
                origin_selection_range: Some(current_file.document.line_index.range(ident.span)),
                target_uri: Url::from_file_path(&template.document.path).unwrap(),
                target_range: template.document.line_index.range(template.span),
                target_selection_range: template.document.line_index.range(template.header),
            })
            .collect();

        Ok(Some(GotoDefinitionResponse::Link(links)))
    }

    async fn hover(&self, params: HoverParams) -> RpcResult<Option<Hover>> {
        let Ok(path) = params
            .text_document_position_params
            .text_document
            .uri
            .to_file_path()
        else {
            return Ok(None);
        };

        let current_file = self
            .analyzer
            .lock()
            .await
            .analyze(&path)
            .map_err(into_rpc_error)?;

        let Some(ident) =
            lookup_identifier_at(&current_file, params.text_document_position_params.position)
        else {
            return Ok(None);
        };

        let mut docs: Vec<Vec<MarkedString>> = Vec::new();

        // Check templates.
        let templates: Vec<_> = current_file
            .templates_at(ident.span.start())
            .into_iter()
            .filter(|template| template.name == ident.name)
            .sorted_by_key(|template| (&template.document.path, template.span.start()))
            .collect();
        for template in templates {
            let mut contents = vec![MarkedString::from_language_code(
                "text".to_string(),
                format!("template(\"{}\") {{ ... }}", template.name),
            )];
            if let Some(comments) = &template.comments {
                contents.push(MarkedString::from_language_code(
                    "text".to_string(),
                    comments.clone(),
                ));
            };
            contents.push(MarkedString::from_markdown(format!(
                "Go to [Definition]({}#L{})",
                Url::from_file_path(&template.document.path).unwrap(),
                template
                    .document
                    .line_index
                    .range(template.header)
                    .start
                    .line
                    + 1
            )));
            docs.push(contents);
        }

        // Check variables.
        let variables: Vec<_> = current_file
            .variables_at(ident.span.start())
            .into_iter()
            .filter(|variable| variable.name == ident.name)
            .sorted_by_key(|variable| (&variable.document.path, variable.span.start()))
            .collect();
        for variable in variables {
            let mut contents = Vec::new();
            let value = match &variable.value {
                Some(expr) if expr.span().as_str().len() <= 100 => expr.span().as_str(),
                _ => "...",
            };
            contents.push(MarkedString::from_language_code(
                "text".to_string(),
                format!("{} = {}", variable.name, value),
            ));
            contents.push(MarkedString::from_markdown(format!(
                "Go to [Initial Assignment]({}#L{})",
                Url::from_file_path(&variable.document.path).unwrap(),
                variable.document.line_index.range(variable.span).start.line + 1
            )));
            docs.push(contents);
        }

        // Check builtin rules.
        if let Some(symbol) = BUILTINS.all().find(|symbol| symbol.name == ident.name) {
            docs.push(vec![MarkedString::from_markdown(symbol.doc.to_string())]);
        }

        if docs.is_empty() {
            return Ok(None);
        }

        let contents = docs.join(&MarkedString::from_markdown("---".to_string()));
        return Ok(Some(Hover {
            contents: HoverContents::Array(contents),
            range: Some(current_file.document.line_index.range(ident.span)),
        }));
    }

    async fn document_link(
        &self,
        params: DocumentLinkParams,
    ) -> RpcResult<Option<Vec<DocumentLink>>> {
        let Ok(path) = params.text_document.uri.to_file_path() else {
            return Ok(None);
        };

        let current_file = self
            .analyzer
            .lock()
            .await
            .analyze(&path)
            .map_err(into_rpc_error)?;

        let links = current_file
            .links
            .iter()
            .map(|link| match link {
                Link::File { path, span } => DocumentLink {
                    target: Some(Url::from_file_path(path).unwrap()),
                    range: current_file.document.line_index.range(*span),
                    tooltip: None,
                    data: None,
                },
                Link::Target { path, name, span } => DocumentLink {
                    target: None, // Resolve with positions later.
                    range: current_file.document.line_index.range(*span),
                    tooltip: None,
                    data: Some(
                        serde_json::to_value(TargetLinkData {
                            path: path.to_path_buf(),
                            name: name.to_string(),
                        })
                        .unwrap(),
                    ),
                },
            })
            .collect();

        Ok(Some(links))
    }

    async fn document_link_resolve(&self, mut link: DocumentLink) -> RpcResult<DocumentLink> {
        let Some(data) = link
            .data
            .take()
            .and_then(|value| serde_json::from_value::<TargetLinkData>(value).ok())
        else {
            return Err(tower_lsp::jsonrpc::Error::internal_error());
        };

        let target_file = self
            .analyzer
            .lock()
            .await
            .analyze(&data.path)
            .map_err(into_rpc_error)?;

        let Some(target) = target_file
            .targets_at(usize::MAX)
            .into_iter()
            .find(|t| t.name == data.name)
        else {
            return Err(tower_lsp::jsonrpc::Error::internal_error());
        };

        let range = target.document.line_index.range(target.span);
        let mut uri = Url::from_file_path(&data.path).unwrap();
        uri.set_fragment(Some(&format!(
            "L{},{}",
            range.start.line + 1,
            range.start.character + 1,
        )));
        link.target = Some(uri);
        Ok(link)
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> RpcResult<Option<DocumentSymbolResponse>> {
        let Ok(path) = params.text_document.uri.to_file_path() else {
            return Ok(None);
        };

        let current_file = self
            .analyzer
            .lock()
            .await
            .analyze(&path)
            .map_err(into_rpc_error)?;

        Ok(Some(DocumentSymbolResponse::Nested(
            current_file.symbols.clone(),
        )))
    }
}

pub async fn run() {
    let analyzer = Analyzer::new(DocumentStorage::new());
    let (service, socket) = LspService::new(move |client| Backend::new(analyzer, client));

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    Server::new(stdin, stdout, socket).serve(service).await;
}

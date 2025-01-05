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

use tokio::sync::Mutex;
use tower_lsp::{lsp_types::*, Client, LanguageServer, LspService, Server};
use tree_sitter::{Node, Point};

use crate::{
    analyze::{Analyzer, Link},
    builtins::BUILTINS,
    parse::{ParsedFile, RecursiveParser},
    storage::DocumentStorage,
    util::to_lsp_range,
};

type RpcResult<T> = tower_lsp::jsonrpc::Result<T>;

fn into_rpc_error(err: std::io::Error) -> tower_lsp::jsonrpc::Error {
    let mut rpc_err = tower_lsp::jsonrpc::Error::internal_error();
    rpc_err.message = Cow::from(err.to_string());
    rpc_err
}

fn lookup_node<'file>(file: &'file ParsedFile, position: &Position) -> Node<'file> {
    let position = Point {
        row: position.line as usize,
        column: position.character as usize,
    };
    let mut cursor = file.tree.walk();
    while cursor.goto_first_child_for_point(position).is_some() {}
    cursor.node()
}

fn lookup_node_of_kind<'file>(
    file: &'file ParsedFile,
    position: &Position,
    kind: &str,
) -> Option<Node<'file>> {
    let node = lookup_node(file, position);
    (node.kind() == kind).then_some(node)
}

#[derive(serde::Serialize, serde::Deserialize)]
struct TargetLinkData {
    path: PathBuf,
    name: String,
}

struct Backend {
    parser: Mutex<RecursiveParser>,
    analyzer: Analyzer,
    client: Client,
}

impl Backend {
    pub fn new(parser: RecursiveParser, analyzer: Analyzer, client: Client) -> Self {
        Self {
            parser: Mutex::new(parser),
            analyzer,
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

    async fn shutdown(&self) -> RpcResult<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let Ok(path) = params.text_document.uri.to_file_path() else {
            return;
        };

        self.parser.lock().await.storage_mut().load_to_memory(
            &path,
            params.text_document.text.as_bytes(),
            params.text_document.version,
        );

        // let file = self.parser.lock().await.parse(&path).unwrap().clone();
        // let sexp = file.tree.root_node().to_sexp();
        // self.client.log_message(MessageType::INFO, sexp).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let Ok(path) = params.text_document.uri.to_file_path() else {
            return;
        };
        let Some(change) = params.content_changes.first() else {
            return;
        };

        self.parser.lock().await.storage_mut().load_to_memory(
            &path,
            change.text.as_bytes(),
            params.text_document.version,
        );
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let Ok(path) = params.text_document.uri.to_file_path() else {
            return;
        };

        self.parser
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
            .parser
            .lock()
            .await
            .parse(&path)
            .map_err(into_rpc_error)?;
        let Some(current_node) = lookup_node_of_kind(
            &current_file,
            &params.text_document_position_params.position,
            "identifier",
        ) else {
            return Ok(None);
        };

        let files = self
            .parser
            .lock()
            .await
            .parse_all(&path)
            .map_err(into_rpc_error)?;

        let name = current_node
            .utf8_text(current_file.document.data.as_slice())
            .unwrap();

        let links = files
            .iter()
            .flat_map(|file| self.analyzer.scan_templates(file))
            .filter(|template| template.name == name)
            .map(|template| LocationLink {
                origin_selection_range: Some(to_lsp_range(&current_node.range())),
                target_uri: template.block.uri.clone(),
                target_range: template.block.range,
                target_selection_range: template.header.range,
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

        let files = self
            .parser
            .lock()
            .await
            .parse_all(&path)
            .map_err(into_rpc_error)?;

        let current_file = &files[0];
        let Some(current_node) = lookup_node_of_kind(
            current_file,
            &params.text_document_position_params.position,
            "identifier",
        ) else {
            return Ok(None);
        };
        let name = current_node
            .utf8_text(current_file.document.data.as_slice())
            .unwrap();

        // Check builtins first.
        if let Some(symbol) = BUILTINS.all().find(|symbol| symbol.name == name) {
            let contents = vec![MarkedString::from_markdown(symbol.doc.to_string())];
            return Ok(Some(Hover {
                contents: HoverContents::Array(contents),
                range: Some(to_lsp_range(&current_node.range())),
            }));
        }

        let Some(template) = files
            .iter()
            .flat_map(|file| self.analyzer.scan_templates(file))
            .find(|template| template.name == name)
        else {
            return Ok(None);
        };

        // Build the hover contents.
        let mut contents = vec![MarkedString::from_language_code(
            "text".to_string(),
            format!("template(\"{}\") {{ ... }}", template.name),
        )];
        if let Some(comment) = &template.comment {
            contents.push(MarkedString::from_markdown("---".to_string()));
            contents.push(MarkedString::from_language_code(
                "text".to_string(),
                comment.clone(),
            ));
        };
        contents.push(MarkedString::from_markdown(format!(
            "Go to [Definition]({}#L{})",
            template.header.uri,
            template.header.range.start.line + 1
        )));

        Ok(Some(Hover {
            contents: HoverContents::Array(contents),
            range: Some(to_lsp_range(&current_node.range())),
        }))
    }

    async fn document_link(
        &self,
        params: DocumentLinkParams,
    ) -> RpcResult<Option<Vec<DocumentLink>>> {
        let Ok(path) = params.text_document.uri.to_file_path() else {
            return Ok(None);
        };

        let file = self
            .parser
            .lock()
            .await
            .parse(&path)
            .map_err(into_rpc_error)?;

        let links = self.analyzer.scan_links(&file);

        let document_links = links
            .into_iter()
            .map(|link| match link {
                Link::File { uri, location } => DocumentLink {
                    target: Some(uri),
                    range: location.range,
                    tooltip: None,
                    data: None,
                },
                Link::Target {
                    uri,
                    name,
                    location,
                } => DocumentLink {
                    target: None, // Resolve with positions later.
                    range: location.range,
                    tooltip: None,
                    data: Some(
                        serde_json::to_value(TargetLinkData {
                            path: uri.to_file_path().unwrap(),
                            name,
                        })
                        .unwrap(),
                    ),
                },
            })
            .collect();

        Ok(Some(document_links))
    }

    async fn document_link_resolve(&self, mut link: DocumentLink) -> RpcResult<DocumentLink> {
        let Some(data) = link
            .data
            .take()
            .and_then(|value| serde_json::from_value::<TargetLinkData>(value).ok())
        else {
            return Err(tower_lsp::jsonrpc::Error::internal_error());
        };

        let file = self
            .parser
            .lock()
            .await
            .parse(&data.path)
            .map_err(into_rpc_error)?;

        let targets = self.analyzer.scan_targets(&file);
        let Some(target) = targets.into_iter().find(|t| t.name == data.name) else {
            return Err(tower_lsp::jsonrpc::Error::internal_error());
        };

        let mut uri = Url::from_file_path(&data.path).unwrap();
        uri.set_fragment(Some(&format!(
            "L{},{}",
            target.header.range.start.line + 1,
            target.header.range.start.character + 1
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

        let file = self
            .parser
            .lock()
            .await
            .parse(&path)
            .map_err(into_rpc_error)?;

        let templates = self.analyzer.scan_templates(&file);
        let targets = self.analyzer.scan_targets(&file);

        #[allow(deprecated)]
        let symbols = templates
            .into_iter()
            .map(|template| DocumentSymbol {
                name: template.name.clone(),
                detail: Some(format!("template(\"{}\")", template.name)),
                kind: SymbolKind::CLASS,
                range: template.block.range,
                selection_range: template.header.range,
                tags: None,
                deprecated: None,
                children: None,
            })
            .chain(targets.into_iter().map(|target| DocumentSymbol {
                name: target.name.clone(),
                detail: Some(format!("{}(\"{}\")", target.kind, target.name)),
                kind: SymbolKind::VARIABLE,
                range: target.block.range,
                selection_range: target.header.range,
                tags: None,
                deprecated: None,
                children: None,
            }))
            .collect();

        Ok(Some(DocumentSymbolResponse::Nested(symbols)))
    }
}

pub async fn run() {
    let parser = RecursiveParser::new(DocumentStorage::new());
    let analyzer = Analyzer::new();
    let (service, socket) = LspService::new(move |client| Backend::new(parser, analyzer, client));

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    Server::new(stdin, stdout, socket).serve(service).await;
}

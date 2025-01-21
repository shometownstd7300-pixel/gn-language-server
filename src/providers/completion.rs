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

use itertools::Itertools;
use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, CompletionParams, CompletionResponse, Documentation,
    MarkupContent, MarkupKind,
};

use crate::{
    ast::{Node, Statement},
    builtins::BUILTINS,
};

use super::{into_rpc_error, ProviderContext, RpcResult};

fn is_after_dot(data: &str, offset: usize) -> bool {
    for ch in data[..offset].chars().rev() {
        match ch {
            '.' => return true,
            'A'..='Z' | 'a'..='z' | '0'..='9' | '_' => continue,
            _ => return false,
        }
    }
    false
}

pub async fn completion(
    context: &ProviderContext,
    params: CompletionParams,
) -> RpcResult<Option<CompletionResponse>> {
    let Ok(path) = params
        .text_document_position
        .text_document
        .uri
        .to_file_path()
    else {
        return Ok(None);
    };

    let current_file = context
        .analyzer
        .lock()
        .unwrap()
        .analyze(&path)
        .map_err(into_rpc_error)?;

    let offset = current_file
        .document
        .line_index
        .offset(params.text_document_position.position)
        .unwrap_or(0);

    if is_after_dot(&current_file.document.data, offset) {
        return Ok(None);
    }

    let scope = current_file.scope_at(offset);
    let templates = current_file.templates_at(offset);

    // Enumerate variables at the current scope.
    let variable_items = scope
        .all_variables()
        .into_iter()
        .filter_map(|(name, variable)| {
            let first_assignment = variable
                .assignments
                .iter()
                .sorted_by_key(|a| (&a.document.path, a.statement.span().start()))
                .next()?;
            let single_assignment = variable.assignments.len() == 1;
            let snippet = if single_assignment {
                match first_assignment.statement {
                    Statement::Assignment(assignment) => {
                        let raw_value = assignment.rvalue.span().as_str();
                        let display_value = if raw_value.lines().count() <= 5 {
                            raw_value
                        } else {
                            "..."
                        };
                        format!(
                            "{} {} {}",
                            assignment.lvalue.span().as_str(),
                            assignment.op,
                            display_value
                        )
                    }
                    Statement::Call(call) => {
                        assert_eq!(call.function.name, "forward_variables_from");
                        call.span.as_str().to_string()
                    }
                    _ => unreachable!(),
                }
            } else {
                format!("{} = ...", name)
            };
            Some(CompletionItem {
                label: name.to_string(),
                kind: Some(CompletionItemKind::VARIABLE),
                documentation: Some(Documentation::MarkupContent(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: format!("```gn\n{}\n```\n", snippet),
                })),
                ..Default::default()
            })
        });

    // Enumerate templates defined at the current position.
    let template_items = templates.iter().map(|template| {
        let doc_header = format!("```gn\ntemplate(\"{}\") {{ ... }}\n```\n", template.name);

        let doc_comments = if let Some(comments) = &template.comments {
            format!("```text\n{}\n```\n", comments)
        } else {
            String::new()
        };

        let doc = [doc_header, doc_comments].concat();

        CompletionItem {
            label: template.name.to_string(),
            kind: Some(CompletionItemKind::FUNCTION),
            documentation: Some(Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value: doc,
            })),
            ..Default::default()
        }
    });

    // Enumerate buildins.
    let builtin_function_items = BUILTINS
        .functions
        .iter()
        .chain(BUILTINS.targets.iter())
        .map(|symbol| CompletionItem {
            label: symbol.name.to_string(),
            kind: Some(CompletionItemKind::FUNCTION),
            documentation: Some(Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value: symbol.doc.to_string(),
            })),
            ..Default::default()
        });
    let builtin_variable_items = BUILTINS
        .predefined_variables
        .iter()
        .chain(BUILTINS.target_variables.iter())
        .map(|symbol| CompletionItem {
            label: symbol.name.to_string(),
            kind: Some(CompletionItemKind::VARIABLE),
            documentation: Some(Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value: symbol.doc.to_string(),
            })),
            ..Default::default()
        });

    // Keywords.
    let keyword_items = ["true", "false", "if", "else"].map(|name| CompletionItem {
        label: name.to_string(),
        kind: Some(CompletionItemKind::KEYWORD),
        ..Default::default()
    });

    let items: Vec<CompletionItem> = variable_items
        .chain(template_items)
        .chain(builtin_function_items)
        .chain(builtin_variable_items)
        .chain(keyword_items)
        .collect();
    Ok(Some(CompletionResponse::Array(items)))
}

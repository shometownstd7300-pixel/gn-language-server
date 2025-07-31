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
use tower_lsp::lsp_types::{Hover, HoverContents, HoverParams, MarkedString, Url};

use crate::{
    ast::{Node, Statement},
    builtins::BUILTINS,
    error::{Error, Result},
    server::RequestContext,
};

use super::lookup_identifier_at;

pub async fn hover(context: &RequestContext, params: HoverParams) -> Result<Option<Hover>> {
    let Ok(path) = params
        .text_document_position_params
        .text_document
        .uri
        .to_file_path()
    else {
        return Err(Error::General(format!(
            "invalid file URI: {}",
            params.text_document_position_params.text_document.uri
        )));
    };

    let current_file = context
        .analyzer
        .lock()
        .unwrap()
        .analyze(&path, context.ticket)?;

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
            "gn".to_string(),
            format!("template(\"{}\") {{ ... }}", template.name),
        )];
        if !template.comments.is_empty() {
            contents.push(MarkedString::from_language_code(
                "text".to_string(),
                template.comments.to_string(),
            ));
        };
        let position = template
            .document
            .line_index
            .position(template.header.start());
        contents.push(MarkedString::from_markdown(format!(
            "Defined at [{}:{}:{}]({}#L{},{})",
            current_file.workspace.format_path(&template.document.path),
            position.line + 1,
            position.character + 1,
            Url::from_file_path(&template.document.path).unwrap(),
            position.line + 1,
            position.character + 1,
        )));
        docs.push(contents);
    }

    // Check variables.
    let scope = current_file.scope_at(ident.span.start());
    if let Some(variable) = scope.get(ident.name) {
        if let Some(first_assignment) = variable
            .assignments
            .iter()
            .sorted_by_key(|a| (&a.document.path, a.statement.span().start()))
            .next()
        {
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
                format!("{} = ...", ident.name)
            };

            let mut contents = vec![MarkedString::from_language_code("gn".to_string(), snippet)];

            if single_assignment {
                if let Statement::Assignment(assignment) = first_assignment.statement {
                    contents.push(MarkedString::from_language_code(
                        "text".to_string(),
                        assignment.comments.to_string(),
                    ));
                };
            }

            let position = first_assignment
                .document
                .line_index
                .position(first_assignment.statement.span().start());
            contents.push(if single_assignment {
                MarkedString::from_markdown(format!(
                    "Defined at [{}:{}:{}]({}#L{},{})",
                    current_file
                        .workspace
                        .format_path(&first_assignment.document.path),
                    position.line + 1,
                    position.character + 1,
                    Url::from_file_path(&first_assignment.document.path).unwrap(),
                    position.line + 1,
                    position.character + 1,
                ))
            } else {
                MarkedString::from_markdown(format!(
                    "Defined and modified in {} locations",
                    variable.assignments.len()
                ))
            });
            docs.push(contents);
        }
    }

    // Check builtin rules.
    if let Some(symbol) = BUILTINS.all().find(|symbol| symbol.name == ident.name) {
        docs.push(vec![MarkedString::from_markdown(symbol.doc.to_string())]);
    }

    if docs.is_empty() {
        return Ok(None);
    }

    let contents = docs.join(&MarkedString::from_markdown("---".to_string()));
    Ok(Some(Hover {
        contents: HoverContents::Array(contents),
        range: Some(current_file.document.line_index.range(ident.span)),
    }))
}

#[cfg(test)]
mod tests {
    use tower_lsp::lsp_types::{
        Position, Range, TextDocumentIdentifier, TextDocumentPositionParams, WorkDoneProgressParams,
    };

    use crate::testutil::testdata;

    use super::*;

    #[tokio::test]
    async fn test_hover() {
        let uri = Url::from_file_path(testdata("workspaces/hover/BUILD.gn")).unwrap();
        let params = HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: Position {
                    line: 17,
                    character: 0,
                },
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let response = hover(&RequestContext::new_for_testing(), params)
            .await
            .unwrap();

        assert_eq!(
            response,
            Some(Hover {
                contents: HoverContents::Array(vec![
                    MarkedString::from_language_code("gn".to_string(), "a = 1".to_string()),
                    MarkedString::from_language_code("text".to_string(), "".to_string()),
                    MarkedString::from_markdown(format!(
                        "Defined at [//BUILD.gn:18:1]({}#L18,1)",
                        uri
                    )),
                ]),
                range: Some(Range {
                    start: Position {
                        line: 17,
                        character: 0,
                    },
                    end: Position {
                        line: 17,
                        character: 1,
                    },
                }),
            })
        );
    }
}

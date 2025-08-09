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

use std::path::{Path, PathBuf};

use itertools::Itertools;
use tower_lsp::lsp_types::{Position, TextDocumentIdentifier, Url};

use crate::{
    analyzer::{
        AnalyzedFile, AnalyzedTarget, AnalyzedTemplate, AnalyzedVariable, ShallowAnalyzedFile,
    },
    common::error::{Error, Result},
    parser::{Identifier, Node, Statement},
};

pub fn get_text_document_path(text_document: &TextDocumentIdentifier) -> Result<PathBuf> {
    text_document
        .uri
        .to_file_path()
        .map_err(|_| Error::General(format!("invalid file URI: {}", text_document.uri)))
}

pub fn lookup_identifier_at(file: &AnalyzedFile, position: Position) -> Option<&Identifier> {
    let offset = file.document.line_index.offset(position)?;
    file.ast_root
        .identifiers()
        .find(|ident| ident.span.start() <= offset && offset <= ident.span.end())
}

pub fn lookup_target_name_string_at(
    file: &AnalyzedFile,
    position: Position,
) -> Option<&AnalyzedTarget> {
    let offset = file.document.line_index.offset(position)?;
    file.analyzed_root
        .targets()
        .find(|target| target.header.start() < offset && offset < target.header.end())
}

pub fn find_target<'a>(
    file: &'a ShallowAnalyzedFile,
    name: &str,
) -> Option<&'a AnalyzedTarget<'static, 'static>> {
    let targets: Vec<_> = file
        .analyzed_root
        .targets
        .locals()
        .values()
        .sorted_by_key(|target| (&target.document.path, target.span.start()))
        .collect();

    // Try target name prefixes.
    for name in (1..=name.len()).rev().map(|len| &name[..len]) {
        if let Some(target) = targets.iter().find(|t| t.name == name) {
            return Some(target);
        }
    }

    None
}

pub fn format_path(path: &Path, workspace_root: &Path) -> String {
    if let Ok(relative_path) = path.strip_prefix(workspace_root) {
        format!("//{}", relative_path.to_string_lossy())
    } else {
        path.to_string_lossy().to_string()
    }
}

pub fn format_variable_help(variable: &AnalyzedVariable, workspace_root: &Path) -> Vec<String> {
    let first_assignment = variable
        .assignments
        .values()
        .sorted_by_key(|a| (&a.document.path, a.statement.span().start()))
        .next()
        .unwrap();
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
        format!("{} = ...", first_assignment.name)
    };

    let mut paragraphs = vec![format!("```gn\n{snippet}\n```")];

    if single_assignment {
        if let Statement::Assignment(assignment) = first_assignment.statement {
            paragraphs.push(format!(
                "```text\n{}\n```",
                assignment.comments.to_string().trim()
            ));
        };
    }

    let position = first_assignment
        .document
        .line_index
        .position(first_assignment.statement.span().start());
    paragraphs.push(if single_assignment {
        format!(
            "Defined at [{}:{}:{}]({}#L{},{})",
            format_path(&first_assignment.document.path, workspace_root),
            position.line + 1,
            position.character + 1,
            Url::from_file_path(&first_assignment.document.path).unwrap(),
            position.line + 1,
            position.character + 1,
        )
    } else {
        format!(
            "Defined and modified in {} locations",
            variable.assignments.len()
        )
    });

    paragraphs
}

pub fn format_template_help(template: &AnalyzedTemplate, workspace_root: &Path) -> Vec<String> {
    let mut paragraphs = vec![format!(
        "```gn\ntemplate(\"{}\") {{ ... }}\n```",
        template.name
    )];
    if !template.comments.is_empty() {
        paragraphs.push(format!(
            "```text\n{}\n```",
            template.comments.to_string().trim()
        ));
    };
    let position = template
        .document
        .line_index
        .position(template.header.start());
    paragraphs.push(format!(
        "Defined at [{}:{}:{}]({}#L{},{})",
        format_path(&template.document.path, workspace_root),
        position.line + 1,
        position.character + 1,
        Url::from_file_path(&template.document.path).unwrap(),
        position.line + 1,
        position.character + 1,
    ));

    paragraphs
}

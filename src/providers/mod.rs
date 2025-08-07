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

use std::path::PathBuf;

use itertools::Itertools;
use tower_lsp::lsp_types::{Position, TextDocumentIdentifier};

use crate::{
    analyze::{AnalyzedEvent, AnalyzedFile, AnalyzedTarget, ShallowAnalyzedFile},
    ast::{Identifier, Node},
    error::{Error, Result},
};

pub mod completion;
pub mod configuration;
pub mod diagnostics;
pub mod document;
pub mod document_link;
pub mod document_symbol;
pub mod formatting;
pub mod goto_definition;
pub mod hover;
pub mod indexing;
pub mod references;

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
        .top_level_events()
        .filter_map(|event| match event {
            AnalyzedEvent::Target(target) => Some(target),
            _ => None,
        })
        .find(|target| target.header.start() < offset && offset < target.header.end())
}

pub fn find_target<'a>(
    file: &'a ShallowAnalyzedFile,
    name: &str,
) -> Option<&'a AnalyzedTarget<'static, 'static>> {
    let targets: Vec<_> = file
        .analyzed_root
        .targets
        .items()
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

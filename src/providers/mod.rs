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
use tower_lsp::lsp_types::Position;

use crate::{
    analyze::{AnalyzedEvent, AnalyzedFile, AnalyzedTarget},
    ast::{Identifier, Node},
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

/// Finds the position of a target.
pub fn find_target_position(file: &AnalyzedFile, name: &str) -> Option<Position> {
    let targets = file.targets_at(usize::MAX);
    let targets: Vec<_> = targets
        .items()
        .values()
        .sorted_by_key(|target| (&target.document.path, target.span.start()))
        .collect();

    // Try target name prefixes.
    for name in (1..=name.len()).rev().map(|len| &name[..len]) {
        if let Some(target) = targets.iter().find(|t| t.name == name) {
            return Some(target.document.line_index.position(target.span.start()));
        }
    }

    None
}

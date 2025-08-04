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

use tower_lsp::lsp_types::{Location, ReferenceParams, Url};

use crate::{
    analyze::{AnalyzedBlock, AnalyzedEvent, AnalyzedFile, AnalyzedLink},
    error::{Error, Result},
    providers::lookup_target_name_string_at,
    server::RequestContext,
};

fn get_overlapping_targets<'i>(root: &AnalyzedBlock<'i, '_>, prefix: &str) -> Vec<&'i str> {
    root.top_level_events()
        .filter_map(|event| match event {
            AnalyzedEvent::Target(target) => Some(target.name),
            _ => None,
        })
        .filter(|name| name.len() > prefix.len() && name.starts_with(prefix))
        .collect()
}

fn target_references(
    context: &RequestContext,
    current_file: &AnalyzedFile,
    target_name: &str,
) -> Result<Option<Vec<Location>>> {
    let bad_prefixes = get_overlapping_targets(&current_file.analyzed_root, target_name);

    let cached_files = context
        .analyzer
        .lock()
        .unwrap()
        .workspace_cache_for(&current_file.document.path)?
        .files();

    let mut references: Vec<Location> = Vec::new();
    for file in cached_files {
        for link in &file.links {
            let AnalyzedLink::Target { path, name, span } = link else {
                continue;
            };
            if path != &current_file.document.path {
                continue;
            }
            if bad_prefixes
                .iter()
                .any(|bad_prefix| name.starts_with(bad_prefix))
            {
                continue;
            }
            if !name.starts_with(target_name) {
                continue;
            }
            references.push(Location {
                uri: Url::from_file_path(&file.document.path).unwrap(),
                range: file.document.line_index.range(*span),
            });
        }
    }

    Ok(Some(references))
}

pub async fn references(
    context: &RequestContext,
    params: ReferenceParams,
) -> Result<Option<Vec<Location>>> {
    // Require background indexing.
    if !context.client.configurations().await.background_indexing {
        return Ok(None);
    }

    let Ok(path) = params
        .text_document_position
        .text_document
        .uri
        .to_file_path()
    else {
        return Err(Error::General(format!(
            "invalid file URI: {}",
            params.text_document_position.text_document.uri
        )));
    };

    let current_file = context
        .analyzer
        .lock()
        .unwrap()
        .analyze(&path, context.cache_config)?;

    // Wait for the background indexing to finish.
    let indexing = context
        .analyzer
        .lock()
        .unwrap()
        .workspace_cache_for(&path)?
        .indexing();
    indexing.wait().await;

    let position = params.text_document_position.position;

    if let Some(target) = lookup_target_name_string_at(&current_file, position) {
        return target_references(context, &current_file, target.name);
    };

    Ok(None)
}

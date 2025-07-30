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

use std::{path::Path, time::Instant};

use tower_lsp::lsp_types::MessageType;
use walkdir::{DirEntry, WalkDir};

use crate::server::RequestContext;

fn contains_args_gn(entry: &DirEntry) -> bool {
    entry.file_type().is_dir() && entry.path().join("args.gn").exists()
}

async fn index_file(path: &Path, context: &RequestContext) {
    let mut analyzer = context.analyzer.lock().unwrap();
    analyzer.analyze(path, context.ticket).ok();
}

pub async fn index(context: &RequestContext, initiator_path: &Path) {
    let (workspace, mut indexing) = {
        let mut analyzer = context.analyzer.lock().unwrap();
        let Ok(workspace_cache) = analyzer.workspace_cache_for(initiator_path) else {
            return;
        };
        (
            workspace_cache.context().root.clone(),
            workspace_cache.indexing(),
        )
    };

    context
        .client
        .log_message(
            MessageType::INFO,
            format!("Indexing {} in the background...", workspace.display()),
        )
        .await;

    let start_time = Instant::now();
    let mut count = 0;

    let walk = WalkDir::new(&workspace)
        .into_iter()
        .filter_entry(|entry| !contains_args_gn(entry));
    for entry in walk {
        let Ok(entry) = entry else { continue };
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.ends_with(".gn") || name.ends_with(".gni"))
        {
            index_file(entry.path(), context).await;
            count += 1;
        }
    }

    let elapsed = start_time.elapsed();
    context
        .client
        .log_message(
            MessageType::INFO,
            format!(
                "Finished indexing {}: processed {} files in {:.1}s",
                workspace.display(),
                count,
                elapsed.as_secs_f64()
            ),
        )
        .await;

    indexing.mark_done();
}

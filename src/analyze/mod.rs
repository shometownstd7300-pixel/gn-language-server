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
    collections::BTreeMap,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex},
};

pub use data::{
    AnalyzedAssignment, AnalyzedBlock, AnalyzedEvent, AnalyzedFile, AnalyzedImport, AnalyzedLink,
    AnalyzedTarget, AnalyzedTemplate, ShallowAnalyzedFile,
};

use crate::{
    analyze::{data::WorkspaceContext, dotgn::evaluate_dot_gn, full::FullAnalyzer},
    error::{Error, Result},
    storage::DocumentStorage,
    utils::{find_workspace_root, CacheConfig},
};

mod data;
mod dotgn;
mod full;
mod links;
mod shallow;
mod symbols;
mod tests;
mod utils;

pub struct Analyzer {
    workspaces: BTreeMap<PathBuf, WorkspaceAnalyzer>,
    storage: Arc<Mutex<DocumentStorage>>,
}

impl Analyzer {
    pub fn new(storage: &Arc<Mutex<DocumentStorage>>) -> Self {
        Self {
            workspaces: BTreeMap::new(),
            storage: storage.clone(),
        }
    }

    pub fn analyze(
        &mut self,
        path: &Path,
        cache_config: CacheConfig,
    ) -> Result<Pin<Arc<AnalyzedFile>>> {
        if !path.is_absolute() {
            return Err(Error::General("Path must be absolute".to_string()));
        }
        self.workspace_for(path)?.analyze(path, cache_config)
    }

    pub fn cached_files(&self, workspace_root: &Path) -> Vec<Pin<Arc<AnalyzedFile>>> {
        let Some(workspace) = self.workspaces.get(workspace_root) else {
            return Vec::new();
        };
        workspace.analyzer.cached_files()
    }

    fn workspace_for(&mut self, path: &Path) -> Result<&mut WorkspaceAnalyzer> {
        let workspace_root = find_workspace_root(path)?;
        let dot_gn_path = workspace_root.join(".gn");
        let dot_gn_version = {
            let storage = self.storage.lock().unwrap();
            storage.read_version(&dot_gn_path)?
        };

        let cache_hit = self
            .workspaces
            .get(workspace_root)
            .is_some_and(|workspace| workspace.context.dot_gn_version == dot_gn_version);
        if cache_hit {
            return Ok(self.workspaces.get_mut(workspace_root).unwrap());
        }

        let build_config = {
            let storage = self.storage.lock().unwrap();
            let document = storage.read(&dot_gn_path)?;
            evaluate_dot_gn(workspace_root, &document.data)?
        };

        let context = WorkspaceContext {
            root: workspace_root.to_path_buf(),
            dot_gn_version,
            build_config,
        };

        let workspace = WorkspaceAnalyzer::new(&context, &self.storage);
        Ok(self
            .workspaces
            .entry(workspace_root.to_path_buf())
            .or_insert(workspace))
    }
}

struct WorkspaceAnalyzer {
    context: WorkspaceContext,
    analyzer: FullAnalyzer,
}

impl WorkspaceAnalyzer {
    pub fn new(context: &WorkspaceContext, storage: &Arc<Mutex<DocumentStorage>>) -> Self {
        Self {
            context: context.clone(),
            analyzer: FullAnalyzer::new(context, storage),
        }
    }

    pub fn analyze(
        &mut self,
        path: &Path,
        cache_config: CacheConfig,
    ) -> Result<Pin<Arc<AnalyzedFile>>> {
        self.analyzer.analyze(path, cache_config)
    }
}

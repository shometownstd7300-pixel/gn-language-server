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
    sync::{Arc, Mutex, RwLock},
    time::Instant,
};

pub use data::{
    AnalyzedAssignment, AnalyzedBlock, AnalyzedFile, AnalyzedImport, AnalyzedLink, AnalyzedTarget,
    AnalyzedTemplate, ShallowAnalyzedFile, Target, Template, Variable,
};

use crate::{
    analyzer::{
        data::WorkspaceContext, dotgn::evaluate_dot_gn, full::FullAnalyzer,
        shallow::ShallowAnalysisSnapshot,
    },
    common::{
        error::{Error, Result},
        storage::DocumentStorage,
        utils::find_nearest_workspace_root,
    },
};

mod cache;
mod data;
mod diagnostics;
mod dotgn;
mod full;
mod links;
mod shallow;
mod symbols;
mod tests;
mod utils;

pub struct Analyzer {
    storage: Arc<Mutex<DocumentStorage>>,
    workspaces: RwLock<BTreeMap<PathBuf, Arc<WorkspaceAnalyzer>>>,
}

impl Analyzer {
    pub fn new(storage: &Arc<Mutex<DocumentStorage>>) -> Self {
        Self {
            storage: storage.clone(),
            workspaces: Default::default(),
        }
    }

    pub fn analyze(&self, path: &Path, request_time: Instant) -> Result<Pin<Arc<AnalyzedFile>>> {
        if !path.is_absolute() {
            return Err(Error::General("Path must be absolute".to_string()));
        }
        Ok(self.workspace_for(path)?.analyze(path, request_time))
    }

    pub fn analyze_shallow(
        &self,
        path: &Path,
        request_time: Instant,
    ) -> Result<Pin<Arc<ShallowAnalyzedFile>>> {
        if !path.is_absolute() {
            return Err(Error::General("Path must be absolute".to_string()));
        }
        Ok(self
            .workspace_for(path)?
            .analyze_shallow(path, request_time))
    }

    pub fn cached_files(&self, workspace_root: &Path) -> Vec<Pin<Arc<ShallowAnalyzedFile>>> {
        let Some(workspace) = self.workspaces.read().unwrap().get(workspace_root).cloned() else {
            return Vec::new();
        };
        workspace.analyzer.get_shallow().cached_files()
    }

    pub fn workspace_roots(&self) -> Vec<PathBuf> {
        self.workspaces.read().unwrap().keys().cloned().collect()
    }

    fn workspace_for(&self, path: &Path) -> Result<Arc<WorkspaceAnalyzer>> {
        let workspace_root = find_nearest_workspace_root(path)?;
        let dot_gn_path = workspace_root.join(".gn");
        let dot_gn_version = {
            let storage = self.storage.lock().unwrap();
            storage.read_version(&dot_gn_path)
        };

        {
            let read_lock = self.workspaces.read().unwrap();
            if let Some(workspace) = read_lock.get(workspace_root) {
                if workspace.context.dot_gn_version == dot_gn_version {
                    return Ok(workspace.clone());
                }
            }
        }

        let build_config = {
            let storage = self.storage.lock().unwrap();
            let document = storage.read(&dot_gn_path);
            evaluate_dot_gn(workspace_root, &document.data)?
        };

        let context = WorkspaceContext {
            root: workspace_root.to_path_buf(),
            dot_gn_version,
            build_config,
        };

        let workspace = Arc::new(WorkspaceAnalyzer::new(&context, &self.storage));

        let mut write_lock = self.workspaces.write().unwrap();
        Ok(write_lock
            .entry(workspace_root.to_path_buf())
            .or_insert(workspace)
            .clone())
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

    pub fn analyze(&self, path: &Path, request_time: Instant) -> Pin<Arc<AnalyzedFile>> {
        self.analyzer.analyze(path, request_time)
    }

    pub fn analyze_shallow(
        &self,
        path: &Path,
        request_time: Instant,
    ) -> Pin<Arc<ShallowAnalyzedFile>> {
        self.analyzer
            .get_shallow()
            .analyze(path, request_time, &mut ShallowAnalysisSnapshot::new())
    }
}

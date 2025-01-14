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
    collections::{BTreeMap, BTreeSet},
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::Arc,
};

use itertools::Itertools;

use crate::{
    ast::{parse, Block},
    storage::{Document, DocumentStorage},
    util::parse_simple_literal,
};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WorkspaceContext {
    pub root: PathBuf,
}

impl WorkspaceContext {
    pub fn find_for(path: &Path) -> std::io::Result<Self> {
        for dir in path.ancestors().skip(1) {
            if dir.join(".gn").try_exists()? {
                return Ok(Self {
                    root: dir.to_path_buf(),
                });
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Workspace not found for {}", path.to_string_lossy()),
        ))
    }

    pub fn resolve_path(&self, name: &str, current_dir: &Path) -> PathBuf {
        if let Some(rest) = name.strip_prefix("//") {
            self.root.join(rest)
        } else {
            current_dir.join(name)
        }
    }
}

fn scan_imports<'i>(root: &'i Block) -> impl Iterator<Item = &'i str> {
    root.top_level_calls().filter_map(|call| {
        if call.function.name != "import" {
            return None;
        }
        let string = call.args.iter().exactly_one().ok()?.as_primary_string()?;
        parse_simple_literal(&string.raw_value)
    })
}

#[derive(Clone)]
pub struct ParsedFile {
    pub document: Arc<Document>,
    pub workspace: WorkspaceContext,
    pub root: Block<'static>,
    pub imports: Vec<Arc<ParsedFile>>,
}

impl ParsedFile {
    pub fn empty(path: &Path, workspace: &WorkspaceContext) -> Arc<Self> {
        let document = Arc::new(Document::empty(path));
        let root = Block::empty(&document.data);
        // SAFETY: root's contents are backed by document.data that are guaranteed to have
        // the identical lifetime because ParsedFile in Arc is immutable.
        let root = unsafe { std::mem::transmute::<Block, Block>(root) };
        Arc::new(ParsedFile {
            document,
            workspace: workspace.clone(),
            root,
            imports: Vec::new(),
        })
    }

    pub fn flatten(self: &Arc<Self>) -> Vec<Arc<ParsedFile>> {
        let mut files: Vec<Arc<ParsedFile>> = Vec::new();
        let mut seen: BTreeSet<&Path> = BTreeSet::new();
        self.collect_flatten(&mut files, &mut seen);
        files.reverse();
        files
    }

    fn collect_flatten<'f>(
        self: &'f Arc<Self>,
        files: &mut Vec<Arc<ParsedFile>>,
        seen: &mut BTreeSet<&'f Path>,
    ) {
        for import in &self.imports {
            import.collect_flatten(files, seen);
        }
        if seen.insert(self.document.path.as_path()) {
            files.push(self.clone());
        }
    }
}

pub struct Parser {
    storage: DocumentStorage,
    cache: BTreeMap<PathBuf, Arc<ParsedFile>>,
}

impl Parser {
    pub fn new(storage: DocumentStorage) -> Self {
        Self {
            storage,
            cache: BTreeMap::new(),
        }
    }

    pub fn storage_mut(&mut self) -> &mut DocumentStorage {
        &mut self.storage
    }

    pub fn parse(&mut self, path: &Path) -> std::io::Result<Arc<ParsedFile>> {
        let path = path.canonicalize()?;
        let workspace = WorkspaceContext::find_for(&path)?;
        self.parse_cached(&path, &workspace, &mut Vec::new())
    }

    fn parse_cached(
        &mut self,
        path: &Path,
        workspace: &WorkspaceContext,
        actives: &mut Vec<PathBuf>,
    ) -> std::io::Result<Arc<ParsedFile>> {
        if let Some(cached_file) = self.cache.get(path) {
            if &cached_file.workspace == workspace {
                let latest_version = self.storage.read_version(path)?;
                if latest_version == cached_file.document.version {
                    return Ok(cached_file.clone());
                }
            }
        }

        let new_file = self.parse_uncached(path, workspace, actives)?;
        self.cache.insert(path.to_path_buf(), new_file.clone());

        Ok(new_file)
    }

    fn parse_uncached(
        &mut self,
        path: &Path,
        workspace: &WorkspaceContext,
        actives: &mut Vec<PathBuf>,
    ) -> std::io::Result<Arc<ParsedFile>> {
        if actives.iter().any(|p| p == path) {
            let cycle = actives
                .iter()
                .map(|p| p.as_path())
                .chain(std::iter::once(path))
                .map(|p| p.to_string_lossy())
                .join(" -> ");
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Cycle found in imports: {}", cycle),
            ));
        }

        actives.push(path.to_path_buf());
        let result = self.parse_uncached_inner(path, workspace, actives);
        actives.pop();
        result
    }

    fn parse_uncached_inner(
        &mut self,
        path: &Path,
        workspace: &WorkspaceContext,
        actives: &mut Vec<PathBuf>,
    ) -> std::io::Result<Arc<ParsedFile>> {
        let document = self.storage.read(path)?;
        let root = parse(&document.data);

        let current_dir = path.parent().unwrap();
        let mut imports: Vec<Arc<ParsedFile>> = Vec::new();
        for name in scan_imports(&root) {
            let path = workspace.resolve_path(name, current_dir);
            let import = match self.parse_cached(&path, workspace, actives) {
                Err(err) if err.kind() == ErrorKind::NotFound => {
                    // Ignore missing imports as they might be imported conditionally.
                    ParsedFile::empty(&path, workspace)
                }
                other => other?,
            };
            imports.push(import);
        }

        // SAFETY: root's contents are backed by document.data that are guaranteed to have
        // the identical lifetime because ParsedFile in Arc is immutable.
        let root = unsafe { std::mem::transmute::<Block, Block>(root) };
        Ok(Arc::new(ParsedFile {
            document,
            workspace: workspace.clone(),
            root,
            imports,
        }))
    }
}

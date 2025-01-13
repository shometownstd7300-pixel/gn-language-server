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

#[derive(Clone)]
pub struct ParsedFile {
    pub document: Arc<Document>,
    pub workspace: WorkspaceContext,
    pub root: Block<'static>,
    pub imports: Vec<PathBuf>,
}

fn scan_imports<'i>(root: &'i Block) -> impl Iterator<Item = &'i str> {
    root.top_level_calls().filter_map(|call| {
        if call.function.name != "import" {
            return None;
        }
        let string = call.args.first().and_then(|arg| arg.as_primary_string())?;
        parse_simple_literal(&string.raw_value)
    })
}

pub struct SimpleParser {
    storage: DocumentStorage,
}

impl SimpleParser {
    pub fn new(storage: DocumentStorage) -> Self {
        Self { storage }
    }

    pub fn storage_mut(&mut self) -> &mut DocumentStorage {
        &mut self.storage
    }

    pub fn parse(
        &mut self,
        path: &Path,
        workspace: &WorkspaceContext,
    ) -> std::io::Result<Arc<ParsedFile>> {
        let document = self.storage.read(path)?;
        let root = parse(&document.data);

        let current_dir = path.parent().unwrap();
        let imports: Vec<PathBuf> = scan_imports(&root)
            .map(|name| workspace.resolve_path(name, current_dir))
            .collect();

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

pub struct CachedParser {
    parser: SimpleParser,
    cache: BTreeMap<PathBuf, Arc<ParsedFile>>,
}

impl CachedParser {
    pub fn new(storage: DocumentStorage) -> Self {
        Self {
            parser: SimpleParser::new(storage),
            cache: BTreeMap::new(),
        }
    }

    pub fn storage_mut(&mut self) -> &mut DocumentStorage {
        self.parser.storage_mut()
    }

    pub fn parse(
        &mut self,
        path: &Path,
        workspace: &WorkspaceContext,
    ) -> std::io::Result<Arc<ParsedFile>> {
        if let Some(cached_file) = self.cache.get(path) {
            if &cached_file.workspace == workspace {
                let latest_version = self.parser.storage_mut().read_version(path)?;
                if latest_version == cached_file.document.version {
                    return Ok(cached_file.clone());
                }
            }
        }

        let new_file = self.parser.parse(path, workspace)?;
        self.cache.insert(path.to_path_buf(), new_file.clone());
        Ok(new_file)
    }
}

pub struct RecursiveParser {
    parser: CachedParser,
}

impl RecursiveParser {
    pub fn new(storage: DocumentStorage) -> Self {
        Self {
            parser: CachedParser::new(storage),
        }
    }

    pub fn storage_mut(&mut self) -> &mut DocumentStorage {
        self.parser.storage_mut()
    }

    pub fn parse(&mut self, path: &Path) -> std::io::Result<Arc<ParsedFile>> {
        let path = path.canonicalize()?;
        let workspace = WorkspaceContext::find_for(&path)?;
        self.parser.parse(&path, &workspace)
    }

    pub fn parse_all(&mut self, path: &Path) -> std::io::Result<Vec<Arc<ParsedFile>>> {
        let path = path.canonicalize()?;
        let workspace = WorkspaceContext::find_for(&path)?;
        let dot_gn_path = workspace.root.join(".gn");

        let mut files = Vec::new();
        let mut stack = vec![path.to_path_buf()];
        let mut visited = BTreeSet::from([path.to_path_buf()]);
        while let Some(path) = stack.pop() {
            let file = match self.parser.parse(&path, &workspace) {
                // Ignore missing imports as they might be imported conditionally.
                Err(err) if err.kind() == ErrorKind::NotFound && !files.is_empty() => {
                    continue;
                }
                other => other?,
            };
            files.push(file.clone());

            if !visited.contains(&dot_gn_path) {
                visited.insert(dot_gn_path.clone());
                stack.push(dot_gn_path.clone());
            }
            for import in &file.imports {
                if !visited.contains(import) {
                    visited.insert(import.clone());
                    stack.push(import.clone());
                }
            }
        }
        Ok(files)
    }
}

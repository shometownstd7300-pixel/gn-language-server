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
    sync::{Arc, OnceLock},
};

use streaming_iterator::StreamingIterator;
use tree_sitter::{Language, Parser, Query, QueryCursor, Tree};

use crate::storage::{Document, DocumentStorage};

static GN_LANGUAGE: OnceLock<Language> = OnceLock::new();

pub fn gn_language() -> &'static Language {
    GN_LANGUAGE.get_or_init(tree_sitter_gn::language)
}

struct Queries {
    pub imports: Query,
}

impl Queries {
    pub fn new() -> Self {
        let imports = Query::new(
            gn_language(),
            r#"
            (import_statement
                (primary_expression
                    (string
                        (string_content) @import-path)))
        "#,
        )
        .unwrap();

        Self { imports }
    }
}

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
    pub tree: Tree,
    pub imports: Vec<PathBuf>,
}

pub struct SimpleParser {
    storage: DocumentStorage,
    parser: Parser,
    queries: Queries,
}

impl SimpleParser {
    pub fn new(storage: DocumentStorage) -> Self {
        let mut parser = Parser::new();
        parser.set_language(gn_language()).unwrap();
        Self {
            storage,
            parser,
            queries: Queries::new(),
        }
    }

    pub fn storage_mut(&mut self) -> &mut DocumentStorage {
        &mut self.storage
    }

    pub fn parse(
        &mut self,
        path: &Path,
        workspace: &WorkspaceContext,
        old_tree: Option<&Tree>,
    ) -> std::io::Result<Arc<ParsedFile>> {
        let current_dir = path.parent().unwrap();

        let document = self.storage.read(path)?;
        let tree = self.parser.parse(&document.data, old_tree).unwrap();

        let imports: Vec<PathBuf> = {
            let mut cursor = QueryCursor::new();
            let mut matches = cursor.matches(
                &self.queries.imports,
                tree.root_node(),
                document.data.as_slice(),
            );
            let mut imports = Vec::new();
            while let Some(m) = matches.next() {
                let name = m
                    .nodes_for_capture_index(0)
                    .next()
                    .unwrap()
                    .utf8_text(&document.data)
                    .unwrap();
                let path = workspace.resolve_path(name, current_dir);
                imports.push(path);
            }
            imports
        };

        Ok(Arc::new(ParsedFile {
            document,
            workspace: workspace.clone(),
            tree,
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

        let new_file = self.parser.parse(path, workspace, None)?;
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

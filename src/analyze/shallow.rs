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
    io::ErrorKind,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex},
};

use either::Either;
use itertools::Itertools;

use crate::{
    analyze::base::{
        AnalyzedAssignment, AnalyzedTarget, AnalyzedTemplate, ShallowAnalyzedBlock,
        ShallowAnalyzedFile, WorkspaceContext,
    },
    ast::{parse, Block, Comments, LValue, Node, Statement},
    builtins::{DECLARE_ARGS, FOREACH, FORWARD_VARIABLES_FROM, IMPORT, SET_DEFAULTS, TEMPLATE},
    storage::{Document, DocumentStorage},
    util::parse_simple_literal,
};

fn is_exported(name: &str) -> bool {
    !name.starts_with("_")
}

#[derive(Debug)]
struct LoopError {
    cycle: Vec<PathBuf>,
}

impl std::fmt::Display for LoopError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "Cycle detected: ")?;
        for (i, path) in self.cycle.iter().enumerate() {
            if i > 0 {
                write!(f, " -> ")?;
            }
            write!(f, "{}", path.to_string_lossy())?;
        }
        Ok(())
    }
}

impl std::error::Error for LoopError {}

impl From<LoopError> for std::io::Error {
    fn from(err: LoopError) -> Self {
        std::io::Error::new(std::io::ErrorKind::InvalidData, err)
    }
}

pub struct ShallowAnalyzer {
    storage: Arc<Mutex<DocumentStorage>>,
    cache: BTreeMap<PathBuf, Arc<ShallowAnalyzedFile>>,
}

impl ShallowAnalyzer {
    pub fn new(storage: &Arc<Mutex<DocumentStorage>>) -> Self {
        Self {
            storage: storage.clone(),
            cache: BTreeMap::new(),
        }
    }

    pub fn analyze(
        &mut self,
        path: &Path,
        workspace: &WorkspaceContext,
    ) -> std::io::Result<Arc<ShallowAnalyzedFile>> {
        self.analyze_cached(path, workspace, &mut Vec::new())
    }

    fn analyze_cached(
        &mut self,
        path: &Path,
        workspace: &WorkspaceContext,
        visiting: &mut Vec<PathBuf>,
    ) -> std::io::Result<Arc<ShallowAnalyzedFile>> {
        if let Some(cached_file) = self.cache.get(path) {
            if &cached_file.workspace == workspace
                && cached_file.is_fresh(&self.storage.lock().unwrap())?
            {
                return Ok(cached_file.clone());
            }
        }

        let new_file = self.analyze_uncached(path, workspace, visiting)?;
        self.cache.insert(path.to_path_buf(), new_file.clone());

        Ok(new_file)
    }

    fn analyze_uncached(
        &mut self,
        path: &Path,
        workspace: &WorkspaceContext,
        visiting: &mut Vec<PathBuf>,
    ) -> std::io::Result<Arc<ShallowAnalyzedFile>> {
        if visiting.iter().any(|p| p == path) {
            return Err(LoopError {
                cycle: std::mem::take(visiting),
            }
            .into());
        }

        visiting.push(path.to_path_buf());
        let result = self.analyze_uncached_inner(path, workspace, visiting);
        visiting.pop();
        result
    }

    fn analyze_uncached_inner(
        &mut self,
        path: &Path,
        workspace: &WorkspaceContext,
        visiting: &mut Vec<PathBuf>,
    ) -> std::io::Result<Arc<ShallowAnalyzedFile>> {
        let document = match self.storage.lock().unwrap().read(path) {
            Ok(document) => document,
            Err(err) if err.kind() == ErrorKind::NotFound => {
                // Ignore missing imports as they might be imported conditionally.
                return Ok(ShallowAnalyzedFile::empty(path, workspace));
            }
            Err(err) => return Err(err),
        };
        let ast_root = Box::pin(parse(&document.data));
        let mut deps = Vec::new();
        let analyzed_root =
            self.analyze_block(&ast_root, workspace, &document, &mut deps, visiting)?;

        // SAFETY: analyzed_root's contents are backed by pinned document and pinned ast_root.
        let analyzed_root = unsafe {
            std::mem::transmute::<ShallowAnalyzedBlock, ShallowAnalyzedBlock>(analyzed_root)
        };
        // SAFETY: ast_root's contents are backed by pinned document.
        let ast_root = unsafe { std::mem::transmute::<Pin<Box<Block>>, Pin<Box<Block>>>(ast_root) };

        Ok(Arc::new(ShallowAnalyzedFile {
            document,
            workspace: workspace.clone(),
            ast_root,
            analyzed_root,
            deps,
        }))
    }

    fn analyze_block<'i, 'p>(
        &mut self,
        block: &'p Block<'i>,
        workspace: &WorkspaceContext,
        document: &'i Document,
        deps: &mut Vec<Arc<ShallowAnalyzedFile>>,
        visiting: &mut Vec<PathBuf>,
    ) -> std::io::Result<ShallowAnalyzedBlock<'i, 'p>> {
        let mut analyzed_block = ShallowAnalyzedBlock::new_top_level();

        for statement in &block.statements {
            match statement {
                Statement::Assignment(assignment) => {
                    let identifier = match &assignment.lvalue {
                        LValue::Identifier(identifier) => identifier,
                        LValue::ArrayAccess(array_access) => &array_access.array,
                        LValue::ScopeAccess(scope_access) => &scope_access.scope,
                    };
                    if is_exported(identifier.name) {
                        analyzed_block.scope.insert(AnalyzedAssignment {
                            name: identifier.name,
                            comments: assignment.comments.clone(),
                            statement,
                            document,
                            variable_span: identifier.span,
                        });
                    }
                }
                Statement::Call(call) => match call.function.name {
                    IMPORT => {
                        if let Some(name) = call
                            .args
                            .iter()
                            .exactly_one()
                            .ok()
                            .and_then(|expr| expr.as_primary_string())
                            .and_then(|s| parse_simple_literal(s.raw_value))
                        {
                            let path =
                                workspace.resolve_path(name, document.path.parent().unwrap());
                            let file = self.analyze_cached(&path, workspace, visiting)?;
                            analyzed_block.merge(&file.analyzed_root);
                            deps.push(file);
                        }
                    }
                    TEMPLATE => {
                        if let Some(name) = call
                            .args
                            .iter()
                            .exactly_one()
                            .ok()
                            .and_then(|expr| expr.as_primary_string())
                            .and_then(|s| parse_simple_literal(s.raw_value))
                        {
                            if is_exported(name) {
                                analyzed_block.templates.insert(AnalyzedTemplate {
                                    name,
                                    comments: call.comments.clone(),
                                    document,
                                    header: call.function.span,
                                    span: call.span,
                                });
                            }
                        }
                    }
                    DECLARE_ARGS | FOREACH => {
                        if let Some(block) = &call.block {
                            analyzed_block.merge(
                                &self.analyze_block(block, workspace, document, deps, visiting)?,
                            );
                        }
                    }
                    SET_DEFAULTS => {}
                    FORWARD_VARIABLES_FROM => {
                        if let Some(strings) = call
                            .args
                            .get(1)
                            .and_then(|expr| expr.as_primary_list())
                            .map(|list| {
                                list.values
                                    .iter()
                                    .filter_map(|expr| expr.as_primary_string())
                                    .collect::<Vec<_>>()
                            })
                        {
                            for string in strings {
                                if let Some(name) = parse_simple_literal(string.raw_value) {
                                    if is_exported(name) {
                                        analyzed_block.scope.insert(AnalyzedAssignment {
                                            name,
                                            comments: Comments::default(),
                                            statement,
                                            document,
                                            variable_span: string.span,
                                        });
                                    }
                                }
                            }
                        }
                    }
                    _ => {
                        if let Some(name) = call
                            .args
                            .iter()
                            .exactly_one()
                            .ok()
                            .and_then(|expr| expr.as_primary_string())
                            .and_then(|s| parse_simple_literal(s.raw_value))
                        {
                            analyzed_block.targets.insert(AnalyzedTarget {
                                name,
                                document,
                                header: call.args[0].span(),
                                span: call.span,
                            });
                        }
                    }
                },
                Statement::Condition(condition) => {
                    let mut current_condition = condition;
                    loop {
                        analyzed_block.merge(&self.analyze_block(
                            &current_condition.then_block,
                            workspace,
                            document,
                            deps,
                            visiting,
                        )?);
                        match &current_condition.else_block {
                            None => break,
                            Some(Either::Left(next_condition)) => {
                                current_condition = next_condition;
                            }
                            Some(Either::Right(block)) => {
                                analyzed_block.merge(
                                    &self.analyze_block(
                                        block, workspace, document, deps, visiting,
                                    )?,
                                );
                                break;
                            }
                        }
                    }
                }
                Statement::Unknown(_) | Statement::UnmatchedBrace(_) => {}
            }
        }

        Ok(analyzed_block)
    }
}
